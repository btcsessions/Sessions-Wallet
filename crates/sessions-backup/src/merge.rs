//! Merging two devices' wallet state (sync between linked devices).
//!
//! A three-way merge against the last state both devices agreed on (the
//! `base`, kept by each device after every sync). Whatever changed on only
//! one side wins; when both changed the same thing, `remote` (the newer
//! snapshot) wins. That's done item by item, so a rename on the phone and a
//! new label on the computer both survive:
//!
//! * objects merge key by key, recursively;
//! * lists of records merge record by record, matched by `id` (accounts,
//!   modules) or by `type` + `ref` (BIP329 labels); a record deleted on one
//!   side stays deleted unless the other side changed it;
//! * counters named `next…` take the larger value, so ids never repeat;
//! * anything else is a plain value: the side that changed it wins.

use serde_json::{Map, Value};

/// Merge `local` and `remote`, both descended from `base` (`None`: no common
/// state yet, as for a device that just linked: then this is a two-way
/// merge where remote wins conflicts and nothing counts as deleted).
pub fn merge(base: Option<&Value>, local: &Value, remote: &Value) -> Value {
    merge_opt(base, Some(local), Some(remote), "").unwrap_or(Value::Null)
}

fn merge_opt(base: Option<&Value>, local: Option<&Value>, remote: Option<&Value>, key: &str) -> Option<Value> {
    if local == remote {
        return local.cloned();
    }
    if base.is_some() {
        if local == base {
            return remote.cloned();
        }
        if remote == base {
            return local.cloned();
        }
    }
    match (local, remote) {
        // Deleted on one side, changed (or added) on the other: keep it.
        (None, Some(r)) => Some(r.clone()),
        (Some(l), None) => Some(l.clone()),
        (None, None) => None,
        (Some(l), Some(r)) => Some(merge_both(base, l, r, key)),
    }
}

/// Both sides have a value and they differ.
fn merge_both(base: Option<&Value>, l: &Value, r: &Value, key: &str) -> Value {
    if key.starts_with("next") {
        if let (Some(a), Some(b)) = (l.as_u64(), r.as_u64()) {
            return Value::from(a.max(b));
        }
    }
    match (l, r) {
        (Value::Object(lo), Value::Object(ro)) => {
            let bo = base.and_then(Value::as_object);
            Value::Object(merge_objects(bo, lo, ro))
        }
        (Value::Array(la), Value::Array(ra)) if keyed(la) && keyed(ra) => {
            let ba = base.and_then(Value::as_array).filter(|b| keyed(b));
            Value::Array(merge_records(ba.map(Vec::as_slice), la, ra))
        }
        _ => r.clone(),
    }
}

fn merge_objects(base: Option<&Map<String, Value>>, l: &Map<String, Value>, r: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    // Remote's order first, then keys only local has.
    let keys = r.keys().chain(l.keys().filter(|k| !r.contains_key(*k)));
    for k in keys {
        let b = base.and_then(|b| b.get(k));
        // A key both sides lack but base had was deleted on both: skip.
        let b = if base.is_none() { None } else { b };
        let merged = if base.is_some() {
            merge_opt(b, l.get(k), r.get(k), k)
        } else {
            // No base: nothing was deleted, union of both.
            match (l.get(k), r.get(k)) {
                (Some(x), Some(y)) if x == y => Some(x.clone()),
                (Some(x), Some(y)) => Some(merge_both(None, x, y, k)),
                (x, y) => x.or(y).cloned(),
            }
        };
        if let Some(v) = merged {
            out.insert(k.clone(), v);
        }
    }
    out
}

/// A record's identity in a list, if it has one.
fn record_key(v: &Value) -> Option<String> {
    let o = v.as_object()?;
    if let Some(id) = o.get("id").and_then(Value::as_str) {
        return Some(format!("id:{id}"));
    }
    match (o.get("type").and_then(Value::as_str), o.get("ref").and_then(Value::as_str)) {
        (Some(t), Some(r)) => Some(format!("ref:{t}:{r}")),
        _ => None,
    }
}

/// A list of records we can match up (empty lists count).
fn keyed(a: &[Value]) -> bool {
    a.iter().all(|v| record_key(v).is_some())
}

fn merge_records(base: Option<&[Value]>, l: &[Value], r: &[Value]) -> Vec<Value> {
    let find = |list: &[Value], k: &str| list.iter().find(|v| record_key(v).as_deref() == Some(k)).cloned();
    let mut order: Vec<String> = Vec::new();
    for v in r.iter().chain(l.iter()) {
        let k = record_key(v).expect("keyed");
        if !order.contains(&k) {
            order.push(k);
        }
    }
    let mut out = Vec::new();
    for k in order {
        let (lv, rv) = (find(l, &k), find(r, &k));
        let merged = match base {
            Some(b) => {
                let bv = find(b, &k);
                merge_opt(bv.as_ref(), lv.as_ref(), rv.as_ref(), "")
            }
            None => match (&lv, &rv) {
                (Some(x), Some(y)) if x == y => Some(x.clone()),
                (Some(x), Some(y)) => Some(merge_both(None, x, y, "")),
                (x, y) => x.clone().or(y.clone()),
            },
        };
        if let Some(v) = merged {
            out.push(v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn one_sided_changes_win() {
        let base = json!({"a": 1, "b": 2});
        let local = json!({"a": 5, "b": 2});
        let remote = json!({"a": 1, "b": 7});
        assert_eq!(merge(Some(&base), &local, &remote), json!({"a": 5, "b": 7}));
    }

    #[test]
    fn true_conflicts_go_to_remote() {
        let base = json!({"theme": "dark"});
        assert_eq!(
            merge(Some(&base), &json!({"theme": "light"}), &json!({"theme": "sessions"})),
            json!({"theme": "sessions"})
        );
    }

    #[test]
    fn accounts_merge_by_id_with_renames_adds_and_deletes() {
        let base = json!({"accounts": [
            {"id": "spending", "name": "Spending"},
            {"id": "savings", "name": "On-Chain"},
            {"id": "watch-2", "name": "Dad's"},
        ], "nextId": 3});
        // Phone renamed savings and added a vault.
        let local = json!({"accounts": [
            {"id": "spending", "name": "Spending"},
            {"id": "savings", "name": "Rainy day"},
            {"id": "watch-2", "name": "Dad's"},
            {"id": "vault-3-a1", "name": "Vault"},
        ], "nextId": 4});
        // Computer deleted Dad's and added a hardware account.
        let remote = json!({"accounts": [
            {"id": "spending", "name": "Spending"},
            {"id": "savings", "name": "On-Chain"},
            {"id": "hw-3-b2", "name": "Trezor"},
        ], "nextId": 4});
        let m = merge(Some(&base), &local, &remote);
        let names: Vec<&str> = m["accounts"].as_array().unwrap().iter().map(|a| a["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Spending", "Rainy day", "Trezor", "Vault"]);
        assert_eq!(m["nextId"], 4);
    }

    #[test]
    fn a_change_beats_a_delete() {
        let base = json!({"accounts": [{"id": "x", "name": "Old"}]});
        let local = json!({"accounts": []});
        let remote = json!({"accounts": [{"id": "x", "name": "New"}]});
        assert_eq!(merge(Some(&base), &local, &remote)["accounts"], json!([{"id": "x", "name": "New"}]));
        assert_eq!(merge(Some(&base), &remote, &local)["accounts"], json!([{"id": "x", "name": "New"}]));
    }

    #[test]
    fn labels_merge_by_type_and_ref() {
        let base = json!({"labels": {"savings": [{"type": "tx", "ref": "aa", "label": "rent"}]}});
        let local = json!({"labels": {"savings": [
            {"type": "tx", "ref": "aa", "label": "rent"},
            {"type": "addr", "ref": "bc1q", "label": "from Bob"},
        ]}});
        let remote = json!({"labels": {"savings": [{"type": "tx", "ref": "aa", "label": "rent (May)"}]}});
        let m = merge(Some(&base), &local, &remote);
        assert_eq!(
            m["labels"]["savings"],
            json!([
                {"type": "tx", "ref": "aa", "label": "rent (May)"},
                {"type": "addr", "ref": "bc1q", "label": "from Bob"},
            ])
        );
    }

    #[test]
    fn without_a_base_it_is_a_union() {
        let local = json!({"accounts": [{"id": "a", "name": "A"}], "prefs": {"unit": "sats"}});
        let remote = json!({"accounts": [{"id": "b", "name": "B"}], "prefs": {"currency": "CAD"}});
        let m = merge(None, &local, &remote);
        assert_eq!(m["accounts"].as_array().unwrap().len(), 2);
        assert_eq!(m["prefs"], json!({"currency": "CAD", "unit": "sats"}));
    }

    #[test]
    fn counters_never_go_backwards() {
        let base = json!({"nextMultisigAccount": 1});
        let m = merge(Some(&base), &json!({"nextMultisigAccount": 3}), &json!({"nextMultisigAccount": 2}));
        assert_eq!(m["nextMultisigAccount"], 3);
    }
}

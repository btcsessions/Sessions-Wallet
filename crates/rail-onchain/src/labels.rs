//! Labels and frozen coins, Sparrow style.
//!
//! Kept in a JSON file next to the account's wallet database, as a list of
//! BIP329 records: `{"type": "addr" | "tx" | "output", "ref": …, "label": …}`,
//! with `"spendable": false` on frozen outputs.
//!
//! A coin's effective label is its own, else its transaction's, else its
//! address's, so change from a labelled send carries the send's label.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bdk_wallet::bitcoin::{OutPoint, Txid};
use serde::{Deserialize, Serialize};
use sessions_core::{Error, LabelKind, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    spendable: Option<bool>,
}

#[derive(Debug, Default)]
pub(crate) struct Labels {
    path: Option<PathBuf>,
    addr: BTreeMap<String, String>,
    tx: BTreeMap<Txid, String>,
    output: BTreeMap<OutPoint, String>,
    frozen: BTreeSet<OutPoint>,
}

/// `foo.db` → `foo.labels.json`.
pub(crate) fn path_for(db_path: &Path) -> PathBuf {
    db_path.with_extension("labels.json")
}

fn storage(e: impl std::fmt::Display) -> Error {
    Error::Storage(format!("labels: {e}"))
}

/// Trimmed, or `None` when empty.
pub(crate) fn clean(label: Option<String>) -> Option<String> {
    label
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

impl Labels {
    /// Load from `path`; a missing file is an empty store.
    pub(crate) fn load(path: PathBuf) -> Result<Self> {
        let mut me = Labels {
            path: Some(path.clone()),
            ..Default::default()
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(me),
            Err(e) => return Err(storage(e)),
        };
        let records: Vec<Record> = serde_json::from_slice(&bytes).map_err(storage)?;
        for r in records {
            let label = clean(r.label);
            match r.kind.as_str() {
                "addr" => {
                    if let Some(l) = label {
                        me.addr.insert(r.reference, l);
                    }
                }
                "tx" => {
                    if let (Ok(t), Some(l)) = (r.reference.parse(), label) {
                        me.tx.insert(t, l);
                    }
                }
                "output" => {
                    let Ok(op) = r.reference.parse::<OutPoint>() else {
                        continue;
                    };
                    if let Some(l) = label {
                        me.output.insert(op, l);
                    }
                    if r.spendable == Some(false) {
                        me.frozen.insert(op);
                    }
                }
                // Other BIP329 types (pubkey, xpub, input) aren't used yet.
                _ => {}
            }
        }
        Ok(me)
    }

    fn records(&self) -> Vec<Record> {
        let mut out = Vec::new();
        for (a, l) in &self.addr {
            out.push(Record {
                kind: "addr".into(),
                reference: a.clone(),
                label: Some(l.clone()),
                spendable: None,
            });
        }
        for (t, l) in &self.tx {
            out.push(Record {
                kind: "tx".into(),
                reference: t.to_string(),
                label: Some(l.clone()),
                spendable: None,
            });
        }
        let outputs: BTreeSet<&OutPoint> = self.output.keys().chain(&self.frozen).collect();
        for op in outputs {
            out.push(Record {
                kind: "output".into(),
                reference: op.to_string(),
                label: self.output.get(op).cloned(),
                spendable: self.frozen.contains(op).then_some(false),
            });
        }
        out
    }

    /// Write atomically (temp file + rename).
    fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let json = serde_json::to_vec_pretty(&self.records()).map_err(storage)?;
        let tmp = path.with_extension("labels.json.tmp");
        std::fs::write(&tmp, json).map_err(storage)?;
        std::fs::rename(&tmp, path).map_err(storage)
    }

    /// `reference` must already be normalised by the caller.
    pub(crate) fn set(
        &mut self,
        kind: LabelKind,
        reference: &str,
        label: Option<String>,
    ) -> Result<()> {
        let label = clean(label);
        match kind {
            LabelKind::Address => set_or_remove(&mut self.addr, reference.to_string(), label),
            LabelKind::Tx => set_or_remove(&mut self.tx, parse_txid(reference)?, label),
            LabelKind::Output => set_or_remove(&mut self.output, parse_outpoint(reference)?, label),
        }
        self.save()
    }

    pub(crate) fn set_frozen(&mut self, op: OutPoint, frozen: bool) -> Result<()> {
        let changed = if frozen {
            self.frozen.insert(op)
        } else {
            self.frozen.remove(&op)
        };
        if changed {
            self.save()?;
        }
        Ok(())
    }

    pub(crate) fn is_frozen(&self, op: &OutPoint) -> bool {
        self.frozen.contains(op)
    }

    pub(crate) fn frozen(&self) -> &BTreeSet<OutPoint> {
        &self.frozen
    }

    pub(crate) fn address(&self, address: &str) -> Option<&str> {
        self.addr.get(address).map(String::as_str)
    }

    pub(crate) fn tx(&self, txid: &Txid) -> Option<&str> {
        self.tx.get(txid).map(String::as_str)
    }

    /// Own label, else the transaction's, else the address's.
    pub(crate) fn effective_output(&self, op: &OutPoint, address: Option<&str>) -> Option<String> {
        self.output
            .get(op)
            .map(String::as_str)
            .or_else(|| self.tx(&op.txid))
            .or_else(|| address.and_then(|a| self.address(a)))
            .map(str::to_string)
    }
}

/// Labels for quoted transactions, applied once they're broadcast. Keyed by
/// the unsigned txid of the quote. Abandoned quotes fall off the end.
#[derive(Debug, Default)]
pub(crate) struct Pending(std::collections::VecDeque<(Txid, String)>);

impl Pending {
    const MAX: usize = 64;

    pub(crate) fn remember(&mut self, txid: Txid, label: String) {
        self.0.retain(|(t, _)| *t != txid);
        if self.0.len() >= Self::MAX {
            self.0.pop_front();
        }
        self.0.push_back((txid, label));
    }

    pub(crate) fn take(&mut self, txid: &Txid) -> Option<String> {
        let i = self.0.iter().position(|(t, _)| t == txid)?;
        self.0.remove(i).map(|(_, l)| l)
    }
}

fn set_or_remove<K: Ord>(map: &mut BTreeMap<K, String>, key: K, label: Option<String>) {
    match label {
        Some(l) => {
            map.insert(key, l);
        }
        None => {
            map.remove(&key);
        }
    }
}

pub(crate) fn parse_txid(s: &str) -> Result<Txid> {
    s.trim()
        .parse()
        .map_err(|_| Error::Invalid("That isn't a transaction id".into()))
}

pub(crate) fn parse_outpoint(s: &str) -> Result<OutPoint> {
    s.trim()
        .parse()
        .map_err(|_| Error::Invalid(format!("That isn't a coin reference (txid:vout): {s}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_as_bip329_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.labels.json");
        let op: OutPoint = format!("{}:1", "11".repeat(32)).parse().unwrap();
        let mut l = Labels::load(path.clone()).unwrap();
        l.set(LabelKind::Address, "bc1qexample", Some(" Alice ".into()))
            .unwrap();
        l.set(LabelKind::Tx, &op.txid.to_string(), Some("Rent".into()))
            .unwrap();
        l.set_frozen(op, true).unwrap();

        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let recs = json.as_array().unwrap();
        assert!(recs
            .iter()
            .any(|r| r["type"] == "addr" && r["ref"] == "bc1qexample" && r["label"] == "Alice"));
        assert!(recs.iter().any(|r| r["type"] == "output"
            && r["ref"] == op.to_string().as_str()
            && r["spendable"] == false
            && r.get("label").is_none()));

        let mut l = Labels::load(path.clone()).unwrap();
        assert!(l.is_frozen(&op));
        assert_eq!(
            l.effective_output(&op, Some("bc1qexample")).unwrap(),
            "Rent"
        );
        l.set(LabelKind::Tx, &op.txid.to_string(), Some("  ".into()))
            .unwrap();
        assert_eq!(
            l.effective_output(&op, Some("bc1qexample")).unwrap(),
            "Alice"
        );
        l.set(LabelKind::Output, &op.to_string(), Some("Coin".into()))
            .unwrap();
        assert_eq!(
            l.effective_output(&op, Some("bc1qexample")).unwrap(),
            "Coin"
        );
        assert!(l.set(LabelKind::Output, "nope", Some("x".into())).is_err());
    }
}

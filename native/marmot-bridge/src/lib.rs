//! A JSON-over-C bridge to MarmotKit for the desktop wallet.
//!
//! Two exported functions:
//!
//! * `marmot_bridge_call(request) -> response`: `request` is
//!   `{"method": "...", "args": {...}}`; the response is `{"ok": value}` or
//!   `{"err": "message"}`. Calls block the calling thread (run them off the
//!   UI thread).
//! * `marmot_bridge_free(response)`: frees a response.
//!
//! The methods and their JSON match the Android plugin (MarmotPlugin.kt), so
//! the wallet's Rust side is the same on both.
//!
//! The account key is kept in memory only (a host secret store) and dropped
//! on `stop`.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char};
use std::sync::{Arc, Mutex, OnceLock};

use marmot_uniffi::{
    CreateGroupOptionsFfi, Marmot, MarmotKitError, MarmotOptions, RelayPolicyFfi, SecretStore,
    marmot_deployment_neutral,
};
use serde::Deserialize;
use serde_json::{Value, json};
use zeroize::Zeroizing;

#[derive(Default)]
struct MemorySecrets {
    by_account: Mutex<HashMap<String, Zeroizing<String>>>,
    labels: Mutex<HashMap<String, String>>,
}

impl SecretStore for MemorySecrets {
    fn has_secret_for_label(&self, label: String) -> Result<bool, MarmotKitError> {
        let id = self.labels.lock().unwrap().get(&label).cloned();
        Ok(id.is_some_and(|id| self.by_account.lock().unwrap().contains_key(&id)))
    }

    fn has_secret_for_account_id(&self, account_id_hex: String) -> Result<bool, MarmotKitError> {
        Ok(self.by_account.lock().unwrap().contains_key(&account_id_hex))
    }

    fn write_secret(&self, label: String, account_id_hex: String, secret_key_hex: String) -> Result<(), MarmotKitError> {
        self.labels.lock().unwrap().insert(label, account_id_hex.clone());
        self.by_account
            .lock()
            .unwrap()
            .insert(account_id_hex, Zeroizing::new(secret_key_hex));
        Ok(())
    }

    fn load_secret(&self, label: String, account_id_hex: String) -> Result<String, MarmotKitError> {
        let id = if account_id_hex.is_empty() {
            self.labels.lock().unwrap().get(&label).cloned().unwrap_or_default()
        } else {
            account_id_hex
        };
        self.by_account
            .lock()
            .unwrap()
            .get(&id)
            .map(|s| s.to_string())
            .ok_or_else(|| MarmotKitError::SecretNotFound { details: "no key for this account".into() })
    }

    fn remove_secret(&self, label: String, _account_id_hex: String) -> Result<(), MarmotKitError> {
        // The wallet owns the key; MarmotKit forgetting an account doesn't erase it.
        self.labels.lock().unwrap().remove(&label);
        Ok(())
    }
}

struct Session {
    marmot: Arc<Marmot>,
    account: String,
    root: String,
}

struct Bridge {
    rt: tokio::runtime::Runtime,
    secrets: Arc<MemorySecrets>,
    session: Mutex<Option<Session>>,
}

fn bridge() -> &'static Bridge {
    static B: OnceLock<Bridge> = OnceLock::new();
    B.get_or_init(|| Bridge {
        rt: tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("marmot-bridge")
            .build()
            .expect("tokio runtime"),
        secrets: Arc::new(MemorySecrets::default()),
        session: Mutex::new(None),
    })
}

fn msg(e: MarmotKitError) -> String {
    let s = e.to_string();
    s.strip_prefix("details=").unwrap_or(&s).to_string()
}

const BECH32: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

fn polymod(values: &[u8]) -> u32 {
    const G: [u32; 5] = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
    let mut chk: u32 = 1;
    for v in values {
        let b = chk >> 25;
        chk = ((chk & 0x1ffffff) << 5) ^ u32::from(*v);
        for (i, g) in G.iter().enumerate() {
            if (b >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

/// `nsec1…` for a 32-byte secret (hex).
fn nsec(secret_hex: &str) -> Result<Zeroizing<String>, String> {
    let bytes = (0..secret_hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(secret_hex.get(i..i + 2).unwrap_or("zz"), 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|_| "bad key".to_string())?;
    if bytes.len() != 32 {
        return Err("bad key".into());
    }
    let mut words = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in &bytes {
        acc = (acc << 8) | u32::from(*b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            words.push(((acc >> bits) & 31) as u8);
        }
    }
    if bits > 0 {
        words.push(((acc << (5 - bits)) & 31) as u8);
    }
    let hrp = b"nsec";
    let mut values: Vec<u8> = hrp.iter().map(|c| c >> 5).collect();
    values.push(0);
    values.extend(hrp.iter().map(|c| c & 31));
    values.extend(&words);
    values.extend([0u8; 6]);
    let m = polymod(&values) ^ 1;
    let mut out = String::from("nsec1");
    for w in words.iter().copied().chain((0..6).map(|i| ((m >> (5 * (5 - i))) & 31) as u8)) {
        out.push(BECH32[w as usize] as char);
    }
    Ok(Zeroizing::new(out))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartArgs {
    root: String,
    relays: Vec<String>,
    secret_hex: Zeroizing<String>,
    pubkey_hex: String,
    #[serde(default)]
    allow_loopback: bool,
}

fn args<T: for<'de> Deserialize<'de>>(v: &Value) -> Result<T, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("bad arguments: {e}"))
}

fn str_arg(v: &Value, k: &str) -> Result<String, String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing {k}"))
}

fn strs_arg(v: &Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn current(b: &Bridge) -> Result<(Arc<Marmot>, String), String> {
    let s = b.session.lock().unwrap();
    let s = s.as_ref().ok_or("Group messaging isn't started")?;
    Ok((s.marmot.clone(), s.account.clone()))
}

fn dispatch(method: &str, a: &Value) -> Result<Value, String> {
    let b = bridge();
    match method {
        "start" => {
            let s: StartArgs = args(a)?;
            {
                let cur = b.session.lock().unwrap();
                if let Some(c) = cur.as_ref() {
                    if c.account == s.pubkey_hex && c.root == s.root {
                        return Ok(json!({ "account": s.pubkey_hex, "running": true }));
                    }
                }
            }
            if let Some(old) = b.session.lock().unwrap().take() {
                let _ = b.rt.block_on(old.marmot.shutdown_and_close());
            }
            b.secrets
                .by_account
                .lock()
                .unwrap()
                .insert(s.pubkey_hex.clone(), s.secret_hex.clone());
            let mut options = MarmotOptions::for_deployment(marmot_deployment_neutral());
            options.client_name = Some("Sessions Wallet".into());
            options.secret_store = Some(b.secrets.clone() as Arc<dyn SecretStore>);
            if s.allow_loopback {
                options.relay_policy = Some(RelayPolicyFfi::AllowLoopback);
            }
            let m = Marmot::new_with_configuration(s.root.clone(), s.relays.clone(), options).map_err(msg)?;
            b.rt.block_on(async {
                m.start().await?;
                let known = m.list_accounts()?.iter().any(|x| x.account_id_hex == s.pubkey_hex);
                if !known {
                    let n = nsec(&s.secret_hex).map_err(|d| MarmotKitError::Runtime { details: d })?;
                    m.login(n.to_string(), s.relays.clone(), s.relays.clone(), s.relays.clone()).await?;
                } else {
                    let _ = m.sign_in_account(s.pubkey_hex.clone()).await;
                }
                Ok::<_, MarmotKitError>(())
            })
            .map_err(msg)?;
            *b.session.lock().unwrap() = Some(Session {
                marmot: m,
                account: s.pubkey_hex.clone(),
                root: s.root,
            });
            Ok(json!({ "account": s.pubkey_hex, "running": true }))
        }
        "stop" => {
            if let Some(old) = b.session.lock().unwrap().take() {
                let _ = b.rt.block_on(old.marmot.shutdown_and_close());
            }
            b.secrets.by_account.lock().unwrap().clear();
            b.secrets.labels.lock().unwrap().clear();
            Ok(json!({}))
        }
        "createGroup" => {
            let (m, acct) = current(b)?;
            let name = str_arg(a, "name")?;
            let id = b
                .rt
                .block_on(m.create_group_with_options(
                    acct,
                    name,
                    vec![],
                    CreateGroupOptionsFfi {
                        description: None,
                        initial_image: None,
                        disappearing_message_secs: 0,
                    },
                ))
                .map_err(msg)?;
            Ok(json!({ "groupId": id }))
        }
        "invite" | "remove" => {
            let (m, acct) = current(b)?;
            let g = str_arg(a, "groupId")?;
            let members = strs_arg(a, "members");
            if method == "invite" {
                b.rt.block_on(m.invite_members(acct, g, members)).map_err(msg)?;
            } else {
                b.rt.block_on(m.remove_members(acct, g, members)).map_err(msg)?;
            }
            Ok(json!({}))
        }
        "accept" => {
            let (m, acct) = current(b)?;
            b.rt.block_on(m.accept_group_invite(acct, str_arg(a, "groupId")?)).map_err(msg)?;
            Ok(json!({}))
        }
        "decline" => {
            let (m, acct) = current(b)?;
            b.rt.block_on(m.decline_group_invite(acct, str_arg(a, "groupId")?)).map_err(msg)?;
            Ok(json!({}))
        }
        "leave" => {
            let (m, acct) = current(b)?;
            let g = str_arg(a, "groupId")?;
            // Fails when we're the only member or admin; the wipe is what matters.
            let _ = b.rt.block_on(m.leave_group(acct.clone(), g.clone()));
            b.rt.block_on(m.delete_group_local(acct, g)).map_err(msg)?;
            Ok(json!({}))
        }
        "groups" => {
            let (m, acct) = current(b)?;
            let rows = m.chat_list(acct.clone(), true).map_err(msg)?;
            let mut items = Vec::new();
            for r in rows {
                let mut o = json!({
                    "groupId": r.group_id_hex,
                    "name": if r.group_name.is_empty() { r.title.clone() } else { r.group_name.clone() },
                    "pending": r.pending_confirmation,
                    "unread": r.unread_count,
                });
                if r.pending_confirmation {
                    if let Ok(d) = b.rt.block_on(m.group_details(acct.clone(), r.group_id_hex.clone())) {
                        if let Some(w) = d.group.welcomer_account_id_hex {
                            o["welcomer"] = json!(w);
                        }
                    }
                }
                items.push(o);
            }
            Ok(json!({ "items": items }))
        }
        "members" => {
            let (m, acct) = current(b)?;
            let g = str_arg(a, "groupId")?;
            let members: Vec<String> = b
                .rt
                .block_on(m.group_members(acct.clone(), g.clone()))
                .map_err(msg)?
                .into_iter()
                .map(|x| x.member_id_hex)
                .collect();
            let admins = b
                .rt
                .block_on(m.group_details(acct, g))
                .map(|d| d.group.admins)
                .unwrap_or_default();
            Ok(json!({ "members": members, "admins": admins }))
        }
        "send" => {
            let (m, acct) = current(b)?;
            let kind = a.get("kind").and_then(Value::as_u64).ok_or("missing kind")?;
            b.rt.block_on(m.send_custom_event(acct, str_arg(a, "groupId")?, kind, vec![], str_arg(a, "content")?))
                .map_err(msg)?;
            Ok(json!({}))
        }
        "sendText" => {
            let (m, acct) = current(b)?;
            b.rt.block_on(m.send_text(acct, str_arg(a, "groupId")?, str_arg(a, "text")?))
                .map_err(msg)?;
            Ok(json!({}))
        }
        "messages" => {
            let (m, acct) = current(b)?;
            let kinds: Vec<u64> = a
                .get("kinds")
                .and_then(Value::as_array)
                .map(|k| k.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default();
            let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(500) as u32;
            let mut list = m
                .messages(acct, Some(str_arg(a, "groupId")?), Some(limit), (!kinds.is_empty()).then_some(kinds))
                .map_err(msg)?;
            list.sort_by_key(|r| r.recorded_at);
            let items: Vec<Value> = list
                .into_iter()
                .map(|r| {
                    json!({
                        "id": r.message_id_hex,
                        "sender": r.sender,
                        "kind": r.kind,
                        "content": r.plaintext,
                        "createdAt": r.recorded_at,
                    })
                })
                .collect();
            Ok(json!({ "items": items }))
        }
        other => Err(format!("unknown method {other}")),
    }
}

fn respond(v: Result<Value, String>) -> *mut c_char {
    let body = match v {
        Ok(v) => json!({ "ok": v }),
        Err(e) => json!({ "err": e }),
    };
    CString::new(body.to_string())
        .unwrap_or_else(|_| CString::new(r#"{"err":"response had a NUL byte"}"#).unwrap())
        .into_raw()
}

/// # Safety
/// `request` must be a valid NUL-terminated UTF-8 string. The result must be
/// freed with `marmot_bridge_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn marmot_bridge_call(request: *const c_char) -> *mut c_char {
    if request.is_null() {
        return respond(Err("null request".into()));
    }
    // SAFETY: the caller passes a valid C string.
    let req = unsafe { CStr::from_ptr(request) }.to_string_lossy().into_owned();
    let req = Zeroizing::new(req);
    let result = std::panic::catch_unwind(|| {
        let v: Value = serde_json::from_str(&req).map_err(|e| format!("bad request: {e}"))?;
        let method = v.get("method").and_then(Value::as_str).ok_or("missing method")?;
        dispatch(method, v.get("args").unwrap_or(&Value::Null))
    })
    .unwrap_or_else(|_| Err("group messaging crashed".into()));
    respond(result)
}

/// # Safety
/// `response` must come from `marmot_bridge_call` and be freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn marmot_bridge_free(response: *mut c_char) {
    if !response.is_null() {
        // SAFETY: allocated by CString::into_raw in respond().
        drop(unsafe { CString::from_raw(response) });
    }
}

/// ABI version of this bridge; bump when the JSON contract changes.
#[unsafe(no_mangle)]
pub extern "C" fn marmot_bridge_version() -> u32 {
    1
}

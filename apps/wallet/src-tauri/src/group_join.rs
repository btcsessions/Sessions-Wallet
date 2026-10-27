//! Join codes for group vaults (see `sessions_group::join`): the creator
//! publishes a beacon and approves requests; a joiner looks the code up and
//! asks. Codes and requests made here are kept in `groups.json` so they
//! survive a restart.

use serde::{Deserialize, Serialize};
use sessions_backup::keys::NostrKey;
use sessions_backup::relay::Relays;
use sessions_core::{Error, Result};
use sessions_group::join::{self, Beacon, JoinCode, Joiner};
use tauri::State;

use crate::state::AppState;

/// Relays for beacons and join requests (and the default for vault groups).
pub const GROUP_RELAYS: &[&str] = &[
    "wss://relay.primal.net",
    "wss://relay.damus.io",
    "wss://nos.lol",
];

pub fn relays() -> Vec<String> {
    GROUP_RELAYS.iter().map(|s| s.to_string()).collect()
}

/// A code this wallet made (creator side).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenCode {
    /// The code (`K7QM2XPA9TRD`). Kept only until it expires.
    pub code: String,
    /// The draft vault it's for.
    pub draft: String,
    pub name: String,
    pub created: u64,
    pub expires: u64,
    /// Requests the creator turned down (by wallet key).
    #[serde(default)]
    pub ignored: Vec<String>,
}

/// A vault this wallet asked to join (joiner side): an invite from `host`
/// is expected and accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Requested {
    pub host: String,
    pub name: String,
    pub at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinStore {
    #[serde(default)]
    pub codes: Vec<OpenCode>,
    #[serde(default)]
    pub requested: Vec<Requested>,
}

fn path(state: &AppState) -> std::path::PathBuf {
    state.paths.root.join("groups-join.json")
}

pub fn load(state: &AppState) -> JoinStore {
    let mut s: JoinStore = std::fs::read(path(state))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let now = now();
    s.codes.retain(|c| c.expires > now);
    // An invite can take a while (the creator approves when they're back).
    s.requested.retain(|r| r.at + 14 * 24 * 3600 > now);
    s
}

pub fn store(state: &AppState, s: &JoinStore) -> Result<()> {
    let p = path(state);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(s).map_err(|e| Error::Storage(e.to_string()))?)
        .map_err(|e| Error::Storage(e.to_string()))?;
    std::fs::rename(&tmp, &p).map_err(|e| Error::Storage(e.to_string()))
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The wallet's Nostr key (for signing join requests, reading requests).
pub async fn wallet_key(state: &AppState) -> Result<NostrKey> {
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(Error::Locked)?;
    let master = u.seed.master_xpriv(u.config.network)?;
    let sk = crate::nostr_id::wallet_secret(&master, u.config.nostr.index)?;
    NostrKey::from_secret_bytes(&sk.secret_bytes())
}

fn published(results: Vec<(String, Result<()>)>) -> Result<()> {
    if results.iter().any(|(_, r)| r.is_ok()) {
        return Ok(());
    }
    let why = results
        .into_iter()
        .find_map(|(_, r)| r.err())
        .map(|e| e.to_string())
        .unwrap_or_default();
    Err(Error::Invalid(format!(
        "Couldn't reach the Nostr relays. Check your connection and try again ({why})"
    )))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeView {
    /// `K7QM-2XPA-9TRD`.
    pub code: String,
    /// `sessionswallet://join/K7QM2XPA9TRD`: for the QR and texting.
    pub link: String,
    pub expires: u64,
}

fn view(code: &JoinCode, expires: u64) -> CodeView {
    CodeView {
        code: code.display(),
        link: code.link(),
        expires,
    }
}

/// Make (or return the open) join code for a draft vault, and publish its beacon.
pub async fn open_code(state: &AppState, draft: &str, name: &str) -> Result<CodeView> {
    let mut s = load(state);
    if let Some(c) = s.codes.iter().find(|c| c.draft == draft && c.expires > now() + 3600) {
        return Ok(view(&JoinCode::parse(&c.code)?, c.expires));
    }
    let key = wallet_key(state).await?;
    let code = JoinCode::generate();
    let now = now();
    let expires = now + join::CODE_LIFETIME_SECS;
    let beacon = Beacon {
        host: key.public_hex(),
        relays: relays(),
        name: name.to_string(),
        expires,
    };
    let event = join::beacon_event(&code, &beacon, now)?;
    published(Relays::default().publish(&relays(), &[event]).await)?;
    s.codes.retain(|c| c.draft != draft);
    s.codes.push(OpenCode {
        code: code.display().replace('-', ""),
        draft: draft.to_string(),
        name: name.to_string(),
        created: now,
        expires,
        ignored: Vec::new(),
    });
    store(state, &s)?;
    Ok(view(&code, expires))
}

/// Requests for a draft's code that aren't approved or ignored yet.
pub async fn requests(state: &AppState, draft: &str, members: &[String]) -> Result<Vec<Joiner>> {
    let s = load(state);
    let Some(open) = s.codes.iter().find(|c| c.draft == draft) else {
        return Ok(Vec::new());
    };
    let code = JoinCode::parse(&open.code)?;
    let key = wallet_key(state).await?;
    let filter = serde_json::json!({
        "kinds": [join::KIND_JOIN_REQUEST],
        "#p": [key.public_hex()],
        "since": open.created.saturating_sub(600),
        "limit": 200,
    });
    let now = now();
    let mut out: Vec<Joiner> = Vec::new();
    for (_, r) in Relays::default().query(&relays(), filter).await {
        for e in r.unwrap_or_default() {
            if let Ok(j) = join::open_request(&code, &key, &e, now) {
                if !members.contains(&j.pubkey)
                    && !open.ignored.contains(&j.pubkey)
                    && !out.iter().any(|o| o.pubkey == j.pubkey)
                {
                    out.push(j);
                }
            }
        }
    }
    out.sort_by_key(|j| j.at);
    Ok(out)
}

pub fn ignore(state: &AppState, draft: &str, pubkey: &str) -> Result<()> {
    let mut s = load(state);
    if let Some(c) = s.codes.iter_mut().find(|c| c.draft == draft) {
        if !c.ignored.iter().any(|p| p == pubkey) {
            c.ignored.push(pubkey.to_string());
        }
    }
    store(state, &s)
}

pub fn close_code(state: &AppState, draft: &str) -> Result<()> {
    let mut s = load(state);
    s.codes.retain(|c| c.draft != draft);
    store(state, &s)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    pub name: String,
    pub host: String,
    pub host_npub: String,
}

async fn find(code: &JoinCode) -> Result<Beacon> {
    let filter = serde_json::json!({
        "kinds": [join::KIND_BEACON],
        "#d": [code.beacon_tag()],
        "limit": 10,
    });
    let events: Vec<_> = Relays::default()
        .query(&relays(), filter)
        .await
        .into_iter()
        .flat_map(|(_, r)| r.unwrap_or_default())
        .collect();
    join::open_beacon(code, &events, now()).ok_or_else(|| {
        Error::Invalid("No vault found for that code. Check it, or ask for a new one (codes last a day)".into())
    })
}

/// Joiner: what a code is for.
#[tauri::command]
pub async fn group_join_lookup(state: State<'_, AppState>, code: String) -> Result<Found> {
    state.check_screen()?;
    let code = JoinCode::parse(&code)?;
    let b = find(&code).await?;
    if b.host == wallet_key(&state).await?.public_hex() {
        return Err(Error::Invalid("That's your own vault's code".into()));
    }
    Ok(Found {
        name: b.name,
        host_npub: crate::nostr_id::npub(&b.host)?,
        host: b.host,
    })
}

/// Joiner: ask to join. The creator approves; the invite then arrives.
#[tauri::command]
pub async fn group_join_send(state: State<'_, AppState>, code: String, name: String) -> Result<()> {
    state.check_screen()?;
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 40 {
        return Err(Error::Invalid("Add your name (up to 40 characters) so they know it's you".into()));
    }
    let code = JoinCode::parse(&code)?;
    let b = find(&code).await?;
    let key = wallet_key(&state).await?;
    let event = join::request_event(&code, &b.host, &key, name, now())?;
    let mut targets = b.relays.clone();
    targets.retain(|r| r.starts_with("wss://"));
    targets.truncate(5);
    for r in relays() {
        if !targets.contains(&r) {
            targets.push(r);
        }
    }
    published(Relays::default().publish(&targets, &[event]).await)?;
    let mut s = load(&state);
    s.requested.retain(|r| r.host != b.host);
    s.requested.push(Requested {
        host: b.host,
        name: b.name,
        at: now(),
    });
    store(&state, &s)
}

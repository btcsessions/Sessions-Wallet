//! The always-up-to-date encrypted backup of the wallet's state (accounts,
//! hardware wallets, vaults, labels, frozen coins, layout, settings), on
//! Nostr relays. See `sessions_backup` for the format.
//!
//! * Every change to the config (and every label or freeze) calls
//!   [`Backup::touch`]; a background loop ([`run`]) uploads on the
//!   [`Scheduler`]'s timing, skipping uploads when nothing changed.
//! * At unlock the backup is checked: uploaded at once if the last one is
//!   over a day old, otherwise compared with what was last uploaded.
//! * A just-restored wallet never uploads before it has looked for its old
//!   backup (`restore_pending`), so a fresh, empty state can't replace it.
//!   The UI calls `restore_state` right after restoring; if the relays
//!   couldn't be reached the loop keeps trying.
//!
//! What's not in the backup: keys (the seed restores those), the Breez API
//! key, ecash proofs (the mint restores them from the seed, NUT-13) and
//! Spark/Lightning state (Breez restores it from the seed).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as SyncMutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sessions_backup::keys::StateKeys;
use sessions_backup::nostr::Event;
use sessions_backup::relay::Relays;
use sessions_backup::schedule::{Policy, Scheduler};
use sessions_backup::state::{self as snapshot, Snapshot};
use sessions_backup::KIND_APP_DATA;
use sessions_core::{Error, Result};

use crate::config::{write_atomic, BackupFlags, Config, SeedOrigin};
use crate::state::AppState;

/// Upload even when nothing changed if the last upload is older than this,
/// so relays that pruned it get it back.
const REFRESH_SECS: u64 = 24 * 3600;
/// While unlocked, look for changes nothing reported this often.
const CHECK_EVERY: Duration = Duration::from_secs(10 * 60);
/// A "no backup here" answer needs this many relays to say so.
pub const MIN_ANSWERS: usize = 2;
/// The UI's prefs are small; refuse anything silly.
pub const MAX_PREFS_BYTES: usize = 64 * 1024;

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `backup_status` and the `backup://status` event.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupStatus {
    pub passkey: bool,
    pub phrase: bool,
    pub seed_origin: SeedOrigin,
    /// Unix seconds of the last backup that reached a relay.
    pub last_backup_at: Option<u64>,
    /// Why the last attempt failed (or a note, e.g. labels left out).
    pub last_error: Option<String>,
    /// Changes not backed up yet.
    pub pending: bool,
    /// Relays that took the last backup.
    pub relays: Vec<String>,
    /// Unix seconds of the last "Test my backup" that passed on this device.
    pub last_tested_at: Option<u64>,
}

/// `restore_state`, `restore_with_passkey` and the `backup://restored` event.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    pub found: bool,
    pub version: Option<u64>,
    /// Unix seconds when the restored backup was made.
    pub backed_up_at: Option<u64>,
    pub accounts: u32,
    pub labels: u32,
    pub error: Option<String>,
}

/// Bookkeeping in `backup.json` (not part of the backup).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Saved {
    last_backup_at: Option<u64>,
    last_error: Option<String>,
    relays: Vec<String>,
    /// Version of the last snapshot uploaded or restored.
    version: u64,
    /// Content hash of the last upload.
    hash: Option<String>,
    last_created_at: u64,
    /// The seed envelope (sealed phrase under a passkey), re-published with
    /// every backup so relays keep it, and carried inside the backup.
    envelope: Option<Event>,
    /// Restored wallet that hasn't found (or ruled out) its old backup yet.
    #[serde(default)]
    restore_pending: bool,
    /// This device, among the wallet's linked devices.
    #[serde(default)]
    device_id: Option<String>,
    /// The last "use this device's setup" applied (or sent) here.
    #[serde(default)]
    replace_token: Option<String>,
    /// When this device last pulled the other devices' changes (Unix secs).
    #[serde(default)]
    last_sync_at: Option<u64>,
}

type Emit = Arc<dyn Fn(&str, Value) + Send + Sync>;

struct Inner {
    keys: Option<Arc<StateKeys>>,
    sched: Scheduler,
    saved: Saved,
    flags: BackupFlags,
    relay_urls: Vec<String>,
    /// Upload even if unchanged (Back up now, stale backup).
    force_upload: bool,
    last_check_ms: u64,
    last_emitted: Option<BackupStatus>,
    /// When the relays were last checked for other devices' changes.
    last_pull_ms: Option<u64>,
    /// A "use this device's setup" waiting to go out with the next upload.
    send_token: Option<String>,
}

pub struct Backup {
    path: PathBuf,
    inner: SyncMutex<Inner>,
    wake: tokio::sync::Notify,
    emit: OnceLock<Emit>,
    relays: Relays,
    origin: tokio::time::Instant,
    /// One restore at a time (the UI and the loop may both try).
    restoring: tokio::sync::Mutex<()>,
    /// False in tests: never touch the network.
    online: bool,
}

impl Backup {
    /// Bookkeeping from `<root>/backup.json`; flags and relays from the
    /// config on disk, until unlock. `online: false` never uses the network.
    pub fn new(root: &std::path::Path, online: bool) -> Self {
        let path = root.join("backup.json");
        let saved: Saved = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let config = Config::load(&root.join("config.json")).ok().flatten();
        Backup {
            path,
            inner: SyncMutex::new(Inner {
                keys: None,
                sched: Scheduler::new(Policy::default()),
                saved,
                flags: config
                    .as_ref()
                    .map(|c| c.backup.clone())
                    .unwrap_or_else(BackupFlags::new_phrase),
                relay_urls: config
                    .map(|c| c.backup_relays)
                    .unwrap_or_else(sessions_backup::default_relays),
                force_upload: false,
                last_check_ms: 0,
                last_emitted: None,
                last_pull_ms: None,
                send_token: None,
            }),
            wake: tokio::sync::Notify::new(),
            emit: OnceLock::new(),
            relays: Relays::default(),
            origin: tokio::time::Instant::now(),
            restoring: tokio::sync::Mutex::new(()),
            online,
        }
    }

    /// Events to the UI (`backup://status`, `backup://restored`).
    pub fn set_emitter(&self, emit: Emit) {
        let _ = self.emit.set(emit);
    }

    pub fn relays(&self) -> &Relays {
        &self.relays
    }

    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn status_of(i: &Inner) -> BackupStatus {
        BackupStatus {
            passkey: i.flags.passkey,
            phrase: i.flags.phrase,
            seed_origin: i.flags.seed_origin,
            last_backup_at: i.saved.last_backup_at,
            last_error: i.saved.last_error.clone(),
            pending: i.sched.pending() || i.saved.restore_pending,
            relays: i.saved.relays.clone(),
            last_tested_at: i.flags.last_tested_at,
        }
    }

    pub fn status(&self) -> BackupStatus {
        Self::status_of(&self.lock())
    }

    /// Tell the UI if the status changed since last time.
    fn changed(&self) {
        let status = {
            let mut i = self.lock();
            let s = Self::status_of(&i);
            if i.last_emitted.as_ref() == Some(&s) {
                return;
            }
            i.last_emitted = Some(s.clone());
            s
        };
        if let Some(emit) = self.emit.get() {
            emit(
                "backup://status",
                serde_json::to_value(&status).unwrap_or_default(),
            );
        }
    }

    fn persist(&self, saved: &Saved) {
        match serde_json::to_vec_pretty(saved) {
            Ok(bytes) => {
                if let Err(e) = write_atomic(&self.path, &bytes) {
                    tracing::warn!("backup: couldn't save backup.json: {e}");
                }
            }
            Err(e) => tracing::warn!("backup: {e}"),
        }
    }

    fn update_saved(&self, f: impl FnOnce(&mut Saved)) {
        let saved = {
            let mut i = self.lock();
            f(&mut i.saved);
            i.saved.clone()
        };
        self.persist(&saved);
    }

    /// Something in the backed-up state changed.
    pub fn touch(&self) {
        let now = self.now_ms();
        self.lock().sched.touch(now);
        self.wake.notify_one();
        self.changed();
    }

    /// The config was saved: pick up flags and relays, and back it up.
    pub fn config_saved(&self, config: &Config) {
        {
            let mut i = self.lock();
            i.flags = config.backup.clone();
            i.relay_urls = config.backup_relays.clone();
        }
        self.touch();
    }

    /// A new wallet on this device: forget the previous one's bookkeeping.
    /// `restored`: hold uploads until its old backup is found or ruled out.
    pub fn reset(&self, restored: bool) {
        self.update_saved(|s| {
            *s = Saved {
                restore_pending: restored,
                device_id: s.device_id.clone(),
                ..Saved::default()
            }
        });
        let _ = std::fs::remove_file(self.base_path());
        let mut i = self.lock();
        i.sched = Scheduler::new(Policy::default());
        i.force_upload = false;
    }

    /// The backup flags changed in a way that isn't backed up (the last
    /// backup test): update the status without scheduling an upload.
    pub fn flags_changed(&self, config: &Config) {
        self.lock().flags = config.backup.clone();
        self.changed();
    }

    /// Version of the snapshot last uploaded (or restored) here.
    pub fn last_version(&self) -> Option<u64> {
        Some(self.lock().saved.version).filter(|v| *v > 0)
    }

    pub fn is_online(&self) -> bool {
        self.online
    }

    pub fn set_envelope(&self, envelope: Event) {
        self.update_saved(|s| s.envelope = Some(envelope));
    }

    /// Stop re-publishing the passkey envelope (passkey backup removed).
    pub fn clear_envelope(&self) {
        self.update_saved(|s| s.envelope = None);
    }

    /// Unlocked: keep the state keys, and check the backup soon.
    pub fn unlocked(&self, keys: StateKeys, config: &Config) {
        let now = self.now_ms();
        {
            let mut i = self.lock();
            i.keys = Some(Arc::new(keys));
            i.flags = config.backup.clone();
            i.relay_urls = config.backup_relays.clone();
            i.last_check_ms = now;
            let stale = i
                .saved
                .last_backup_at
                .is_none_or(|t| now_secs().saturating_sub(t) > REFRESH_SECS);
            if i.saved.restore_pending {
                // Just restored: the UI looks for the old backup first
                // (`restore_state`); the loop only steps in if it doesn't.
                i.sched.touch(now);
            } else if stale {
                i.force_upload = true;
                i.sched.force(now);
            } else {
                // Uploads only if the content changed since last time.
                i.sched.touch(now);
            }
        }
        self.wake.notify_one();
        self.changed();
    }

    pub fn locked(&self) {
        self.lock().keys = None;
    }

    /// Relays to write to, and to read from (the user's plus the defaults).
    pub fn urls(&self) -> (Vec<String>, Vec<String>) {
        let write = self.lock().relay_urls.clone();
        let mut read: Vec<String> = write.clone();
        for r in sessions_backup::default_relays() {
            if !read.contains(&r) {
                read.push(r);
            }
        }
        (write, read)
    }

    /// The upload: unchanged content is skipped unless forced or stale.
    async fn upload(&self, keys: &StateKeys, data: Value) {
        let now_s = now_secs();
        let hash = snapshot::content_hash(&data);
        let (skip, version, created_at, envelope) = {
            let mut i = self.lock();
            let forced = std::mem::take(&mut i.force_upload);
            let stale = i
                .saved
                .last_backup_at
                .is_none_or(|t| now_s.saturating_sub(t) > REFRESH_SECS);
            let unchanged = i.saved.hash.as_deref() == Some(hash.as_str());
            (
                unchanged && !forced && !stale,
                (i.saved.version + 1).max(now_s),
                (i.saved.last_created_at + 1).max(now_s),
                i.saved.envelope.clone(),
            )
        };
        if skip {
            let now = self.now_ms();
            self.lock().sched.succeeded(now, false);
            self.changed();
            return;
        }
        let (urls, _) = self.urls();
        let sent = data.clone();
        let result = self.publish(keys, version, created_at, data, &urls).await;
        // Keep the envelope alive on the relays; its fate doesn't decide anything.
        if let Some(env) = envelope {
            let _ = self.relays.publish(&urls, &[env]).await;
        }
        let now = self.now_ms();
        match result {
            Ok((accepted, note)) => {
                self.update_saved(|s| {
                    s.last_backup_at = Some(now_secs());
                    s.last_error = note;
                    s.relays = accepted;
                    s.version = version;
                    s.hash = Some(hash);
                    s.last_created_at = created_at;
                });
                self.set_base(&sent);
                self.lock().sched.succeeded(now, true);
            }
            Err(e) => {
                tracing::warn!("backup: {e}");
                self.update_saved(|s| s.last_error = Some(e));
                self.lock().sched.failed(now);
            }
        }
        self.changed();
    }

    /// Seal, publish; returns the relays that took it and a note (labels
    /// left out), or why none did.
    async fn publish(
        &self,
        keys: &StateKeys,
        version: u64,
        created_at: u64,
        mut data: Value,
        urls: &[String],
    ) -> std::result::Result<(Vec<String>, Option<String>), String> {
        let mut snap = Snapshot {
            version,
            created_at,
            data: data.take(),
        };
        let mut note = None;
        let events = match snapshot::encode(keys, &snap, created_at).map_err(|e| e.to_string())? {
            Ok(events) => events,
            Err(too_large) => {
                tracing::warn!(
                    "backup: {} bytes with labels, leaving labels out",
                    too_large.0
                );
                if let Some(o) = snap.data.as_object_mut() {
                    o.insert("labels".into(), Value::Object(Default::default()));
                    o.insert("labelsOmitted".into(), Value::Bool(true));
                }
                note = Some("Too many labels for the online backup: they weren't included".into());
                snapshot::encode(keys, &snap, created_at)
                    .map_err(|e| e.to_string())?
                    .map_err(|_| "The wallet's settings are too large to back up".to_string())?
            }
        };
        let results = self.relays.publish(urls, &events).await;
        let accepted: Vec<String> = results
            .iter()
            .filter(|(_, r)| r.is_ok())
            .map(|(u, _)| u.clone())
            .collect();
        if accepted.is_empty() {
            let why = results
                .iter()
                .find_map(|(_, r)| r.as_ref().err().map(|e| e.to_string()))
                .unwrap_or_else(|| "no relays configured".into());
            return Err(format!("Couldn't reach the backup servers ({why})"));
        }
        Ok((accepted, note))
    }
}

/// The state to back up, from the unlocked wallet and the label files.
pub async fn gather(state: &AppState) -> Result<Value> {
    let config = {
        let guard = state.inner.lock().await;
        guard.as_ref().ok_or(Error::Locked)?.config.clone()
    };
    let mut cfg = serde_json::to_value(&config).map_err(|e| Error::Storage(e.to_string()))?;
    if let Some(o) = cfg.as_object_mut() {
        // Personal API key: not part of the backup.
        o.remove("breezApiKey");
        // When this device last tested the backup: local, and testing must
        // not count as a change to back up.
        if let Some(b) = o.get_mut("backup").and_then(Value::as_object_mut) {
            b.remove("lastTestedAt");
        }
    }
    let mut labels = serde_json::Map::new();
    for a in &config.accounts {
        let path = state.paths.labels(a.id());
        match std::fs::read(&path) {
            Ok(bytes) => {
                if let Ok(v @ Value::Array(_)) = serde_json::from_slice::<Value>(&bytes) {
                    labels.insert(a.id().0.clone(), v);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Storage(format!("labels: {e}"))),
        }
    }
    let envelope = state.backup.lock().saved.envelope.clone();
    let mut data = serde_json::json!({
        "format": 1,
        "config": cfg,
        "labels": labels,
        "envelope": envelope,
    });
    add_sync_fields(state, &config, &mut data);
    Ok(data)
}

/// One pass of the loop: restore first if a restored wallet hasn't yet,
/// else upload if due.
async fn tick(state: &AppState) {
    let b = &state.backup;
    let (pending_restore, keys) = {
        let i = b.lock();
        (i.saved.restore_pending, i.keys.clone())
    };
    let Some(keys) = keys else { return };
    if !pending_restore && b.pull_due() {
        if let Err(e) = pull(state, &keys, false).await {
            tracing::debug!("sync: {e}");
        }
    }
    let now = b.now_ms();
    {
        let mut i = b.lock();
        if now.saturating_sub(i.last_check_ms) >= CHECK_EVERY.as_millis() as u64 {
            i.last_check_ms = now;
            i.sched.touch(now);
        }
        if !i.sched.start(now) {
            return;
        }
    }
    if pending_restore {
        let (r, applied) = restore(state).await;
        let now = b.now_ms();
        let mut i = b.lock();
        if i.saved.restore_pending {
            // Still couldn't reach the relays: back off and try again.
            i.sched.failed(now);
        } else {
            i.sched.cancel();
            i.sched.touch(now);
        }
        drop(i);
        if applied {
            if let Some(emit) = b.emit.get() {
                emit(
                    "backup://restored",
                    serde_json::to_value(&r).unwrap_or_default(),
                );
            }
        }
        b.changed();
        return;
    }
    // Take in the other devices' changes first, so this upload can't undo them.
    let _ = pull(state, &keys, false).await;
    match gather(state).await {
        Ok(data) => b.upload(&keys, data).await,
        Err(e) => {
            tracing::debug!("backup: not now: {e}");
            b.lock().sched.cancel();
        }
    }
}

/// The background loop (running app only).
pub async fn run(state: &AppState) {
    let b = &state.backup;
    loop {
        tick(state).await;
        let wait = {
            let i = b.lock();
            let now = b.now_ms();
            let pull = b.pull_every(&i);
            match (i.keys.is_some(), i.sched.next_due()) {
                (true, Some(due)) => Duration::from_millis(due.saturating_sub(now)).min(pull),
                (true, None) => CHECK_EVERY.min(pull),
                // Locked: wait for unlock to wake us.
                (false, _) => Duration::from_secs(3600),
            }
        };
        tokio::select! {
            _ = b.wake.notified() => {}
            _ = tokio::time::sleep(wait.max(Duration::from_millis(200))) => {}
        }
    }
}

/// Back up now, whatever the timers say, and wait for the result.
pub async fn backup_now(state: &AppState) -> Result<BackupStatus> {
    let b = &state.backup;
    if !b.online {
        return Err(Error::Unsupported("back up while offline".into()));
    }
    let keys = b.lock().keys.clone().ok_or(Error::Locked)?;
    if b.lock().saved.restore_pending {
        let (r, _) = restore(state).await;
        if b.lock().saved.restore_pending {
            return Err(Error::Invalid(r.error.unwrap_or_else(|| {
                "Couldn't reach the backup servers. Try again".into()
            })));
        }
    }
    let now = b.now_ms();
    let go = {
        let mut i = b.lock();
        let go = i.sched.start_now(now);
        if go {
            i.force_upload = true;
        }
        go
    };
    if go {
        match gather(state).await {
            Ok(data) => b.upload(&keys, data).await,
            Err(e) => {
                b.lock().sched.cancel();
                return Err(e);
            }
        }
    }
    Ok(b.status())
}

/// Flush a pending backup as the wallet locks (keys go with the lock).
pub async fn flush_on_lock(state: &AppState) {
    let b = state.backup.clone();
    if !b.online {
        return;
    }
    let (keys, go) = {
        let mut i = b.lock();
        let keys = i.keys.clone();
        let go = keys.is_some()
            && !i.saved.restore_pending
            && i.sched.pending()
            && i.sched.start_now(b.now_ms());
        (keys, go)
    };
    let (Some(keys), true) = (keys, go) else {
        return;
    };
    match gather(state).await {
        Ok(data) => {
            tokio::spawn(async move { b.upload(&keys, data).await });
        }
        Err(_) => b.lock().sched.cancel(),
    }
}

/// Find this seed's state backup on the relays and apply it. Never fails:
/// what happened is in the result.
pub async fn restore_state(state: &AppState) -> RestoreResult {
    restore(state).await.0
}

/// Accounts and labels in a snapshot.
pub(crate) fn counts(snap: &Snapshot) -> (u32, u32) {
    let accounts = snap.data["config"]["accounts"]
        .as_array()
        .map_or(0, |a| a.len() as u32);
    let labels = snap.data["labels"].as_object().map_or(0, |m| {
        m.values()
            .filter_map(Value::as_array)
            .map(|a| a.len() as u32)
            .sum()
    });
    (accounts, labels)
}

/// [`restore_state`], and whether this call applied the backup.
async fn restore(state: &AppState) -> (RestoreResult, bool) {
    let b = &state.backup;
    if !b.online {
        return (
            RestoreResult {
                error: Some("offline".into()),
                ..Default::default()
            },
            false,
        );
    }
    let _one = b.restoring.lock().await;
    let keys = {
        let guard = state.inner.lock().await;
        let Some(u) = guard.as_ref() else {
            return (
                RestoreResult {
                    error: Some(Error::Locked.to_string()),
                    ..Default::default()
                },
                false,
            );
        };
        match u
            .seed
            .master_xpriv(u.config.network)
            .and_then(|m| StateKeys::from_master(&m))
        {
            Ok(k) => k,
            Err(e) => {
                return (
                    RestoreResult {
                        error: Some(e.to_string()),
                        ..Default::default()
                    },
                    false,
                )
            }
        }
    };
    let (_, urls) = b.urls();
    let answers = b
        .relays
        .fetch(&urls, &keys.nostr.public_hex(), KIND_APP_DATA)
        .await;
    let reached = answers.iter().filter(|(_, r)| r.is_ok()).count();
    let events: Vec<Event> = answers
        .into_iter()
        .filter_map(|(_, r)| r.ok())
        .flatten()
        .collect();
    let Some(snap) = snapshot::decode(&keys, &events) else {
        if reached >= MIN_ANSWERS.min(urls.len()) {
            // Nothing there: a wallet that was never backed up.
            b.update_saved(|s| s.restore_pending = false);
            b.changed();
            return (RestoreResult::default(), false);
        }
        return (
            RestoreResult {
                error: Some("Couldn't reach the backup servers".into()),
                ..Default::default()
            },
            false,
        );
    };
    let found = |error: Option<String>| {
        let (accounts, labels) = counts(&snap);
        RestoreResult {
            found: true,
            version: Some(snap.version),
            backed_up_at: Some(snap.created_at),
            accounts,
            labels,
            error,
        }
    };
    let local_version = b.lock().saved.version;
    if snap.version <= local_version && !b.lock().saved.restore_pending {
        // Already applied (e.g. the backup loop got there first), or this
        // device is newer.
        return (found(None), false);
    }
    match apply(state, &snap).await {
        Ok((accounts, labels)) => (
            RestoreResult {
                accounts,
                labels,
                ..found(None)
            },
            true,
        ),
        Err(e) => {
            // Uploads stay held: replacing a backup this version can't read
            // would lose it.
            let msg = format!("Couldn't restore the backup: {e}");
            b.update_saved(|s| s.last_error = Some(msg.clone()));
            b.changed();
            (found(Some(msg)), false)
        }
    }
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Replace this wallet's config and labels with the snapshot's, keeping
/// what's local (Breez key), then reopen the accounts. Returns (accounts, labels).
pub(crate) async fn apply(state: &AppState, snap: &Snapshot) -> Result<(u32, u32)> {
    let mut restored: Config = serde_json::from_value(
        snap.data
            .get("config")
            .cloned()
            .ok_or_else(|| Error::Storage("backup has no settings".into()))?,
    )
    .map_err(|e| Error::Storage(format!("backup settings: {e}")))?;

    let mut guard = state.inner.lock().await;
    let u = guard.as_mut().ok_or(Error::Locked)?;
    if restored.network != u.config.network {
        return Err(Error::Network(
            "the backup is for a different bitcoin network".into(),
        ));
    }
    // Local only.
    restored.breez_api_key = u.config.breez_api_key.clone();
    // What this device knows adds to what the backup says.
    let (local, saved) = (&u.config.backup, &restored.backup);
    restored.backup = BackupFlags {
        passkey: local.passkey || saved.passkey,
        phrase: local.phrase || saved.phrase,
        seed_origin: saved.seed_origin,
        credential_id: local.credential_id.clone().or(saved.credential_id.clone()),
        last_tested_at: local.last_tested_at,
    };
    if restored.backup_relays.is_empty() {
        restored.backup_relays = sessions_backup::default_relays();
    }
    // Each kind of device (phone, computer) keeps its own home screen.
    if let Some(layout) = snap
        .data
        .pointer(&format!("/layouts/{DEVICE_KIND}"))
        .and_then(|l| serde_json::from_value(l.clone()).ok())
    {
        restored.layout = layout;
    }

    let ids: BTreeSet<String> = restored.accounts.iter().map(|a| a.id().0.clone()).collect();
    let mut labels = 0u32;
    if let Some(map) = snap.data.get("labels").and_then(Value::as_object) {
        for (id, records) in map {
            if !safe_id(id) || !ids.contains(id) {
                continue;
            }
            let Some(list) = records.as_array() else {
                continue;
            };
            labels += list.len() as u32;
            let bytes =
                serde_json::to_vec_pretty(records).map_err(|e| Error::Storage(e.to_string()))?;
            write_atomic(&state.paths.labels(&id.as_str().into()), &bytes)?;
        }
    }
    let accounts = restored.accounts.len() as u32;
    restored.save(&state.paths.config())?;

    let envelope = snap
        .data
        .get("envelope")
        .and_then(|e| serde_json::from_value::<Event>(e.clone()).ok())
        .filter(Event::verify);
    state.backup.set_base(&snap.data);
    state.backup.update_saved(|s| {
        s.version = snap.version;
        s.last_sync_at = Some(now_secs());
        s.last_created_at = s.last_created_at.max(snap.created_at);
        s.last_backup_at = Some(snap.created_at);
        s.hash = None;
        s.restore_pending = false;
        if envelope.is_some() {
            s.envelope = envelope;
        }
    });

    // Reopen every account from the restored config.
    let seed = guard.take().map(|u| u.seed).ok_or(Error::Locked)?;
    drop(guard);
    state.unlock_with(seed).await?;
    Ok((accounts, labels))
}


// --- sync between linked devices ---------------------------------------------
//
// Every device on the same seed reads and writes the same backup, so the
// backup is also how linked devices stay in step. Before each upload, and
// every [`PULL_LINKED`] while more than one device is linked, a device
// fetches the newest snapshot; if another device wrote it, the two are
// merged item by item against the last snapshot this device saw (the
// "base", see `sessions_backup::merge`) and the result applied here, then
// uploaded if this device added anything.
//
// "Use this device's setup" stamps the upload with a new `replaceToken`;
// other devices replace their setup with it once instead of merging.
// Each kind of device keeps its own home-screen layout (`layouts`).

/// "phone" or "desktop": which home-screen layout this device uses.
pub const DEVICE_KIND: &str = if cfg!(any(target_os = "android", target_os = "ios")) {
    "phone"
} else {
    "desktop"
};
/// How often to look for the other devices' changes, once linked.
const PULL_LINKED: Duration = Duration::from_secs(2 * 60);
/// … and when this is the only device (picks up a new link).
const PULL_ALONE: Duration = Duration::from_secs(15 * 60);
/// Devices not seen for this long drop off the list.
const DEVICE_STALE_SECS: u64 = 90 * 24 * 3600;

impl Backup {
    fn base_path(&self) -> PathBuf {
        self.path.with_file_name("sync-base.json")
    }

    /// The last snapshot this device uploaded or took in.
    fn base(&self) -> Option<Value> {
        std::fs::read(self.base_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
    }

    fn set_base(&self, data: &Value) {
        if let Ok(bytes) = serde_json::to_vec(data) {
            let _ = write_atomic(&self.base_path(), &bytes);
        }
    }

    fn device_id(&self) -> String {
        if let Some(id) = self.lock().saved.device_id.clone() {
            return id;
        }
        let id = random_hex(8);
        self.update_saved(|s| s.device_id = Some(id.clone()));
        id
    }

    fn linked(&self) -> bool {
        self.base()
            .and_then(|b| b.get("devices").and_then(Value::as_object).map(|d| d.len()))
            .is_some_and(|n| n > 1)
    }

    fn pull_every(&self, i: &Inner) -> Duration {
        if i.keys.is_none() || i.saved.restore_pending {
            return Duration::from_secs(3600);
        }
        if self.linked() {
            PULL_LINKED
        } else {
            PULL_ALONE
        }
    }

    fn pull_due(&self) -> bool {
        let now = self.now_ms();
        let i = self.lock();
        let every = self.pull_every(&i).as_millis() as u64;
        i.last_pull_ms.is_none_or(|t| now.saturating_sub(t) >= every)
    }
}

fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Layouts per kind of device, the linked-device list, and the replace
/// token, added to what [`gather`] collects.
fn add_sync_fields(state: &AppState, config: &Config, data: &mut Value) {
    let b = &state.backup;
    let base = b.base();
    let from_base = |k: &str| {
        base.as_ref()
            .and_then(|v| v.get(k))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    };
    let mut layouts = from_base("layouts");
    layouts.insert(
        DEVICE_KIND.into(),
        serde_json::to_value(&config.layout).unwrap_or(Value::Null),
    );
    let mut devices = from_base("devices");
    let today = now_secs() / 86_400 * 86_400;
    devices.insert(
        b.device_id(),
        serde_json::json!({ "name": crate::link::device_name(), "kind": DEVICE_KIND, "seenAt": today }),
    );
    devices.retain(|_, d| d["seenAt"].as_u64().is_some_and(|t| today.saturating_sub(t) < DEVICE_STALE_SECS));
    let token = b.lock().send_token.clone().or_else(|| {
        base.as_ref()
            .and_then(|v| v.get("replaceToken"))
            .and_then(Value::as_str)
            .map(String::from)
    });
    if let Some(o) = data.as_object_mut() {
        o.insert("layouts".into(), Value::Object(layouts));
        o.insert("devices".into(), Value::Object(devices));
        if let Some(t) = token {
            o.insert("replaceToken".into(), Value::String(t));
        }
    }
}

/// Take in another device's changes, if there are any. `take_theirs`:
/// replace this device's setup with theirs instead of merging. Returns
/// whether anything here changed.
async fn pull(state: &AppState, keys: &StateKeys, take_theirs: bool) -> Result<bool> {
    let b = &state.backup;
    if !b.online {
        return Ok(false);
    }
    b.lock().last_pull_ms = Some(b.now_ms());
    let _one = b.restoring.lock().await;
    let (urls, _) = b.urls();
    let answers = b
        .relays
        .fetch(&urls, &keys.nostr.public_hex(), KIND_APP_DATA)
        .await;
    if answers.iter().all(|(_, r)| r.is_err()) {
        return Err(Error::Invalid("Couldn't reach the backup servers".into()));
    }
    let events: Vec<Event> = answers
        .into_iter()
        .filter_map(|(_, r)| r.ok())
        .flatten()
        .collect();
    let Some(remote) = snapshot::decode(keys, &events) else {
        return Ok(false);
    };
    let (local_version, applied) = {
        let i = b.lock();
        (i.saved.version, i.saved.replace_token.clone())
    };
    if remote.version <= local_version && !take_theirs {
        b.update_saved(|s| s.last_sync_at = Some(now_secs()));
        return Ok(false);
    }
    let local = gather(state).await?;
    let token = remote
        .data
        .get("replaceToken")
        .and_then(Value::as_str)
        .map(String::from);
    let replace = take_theirs || (token.is_some() && token != applied);
    let mut merged = if replace {
        remote.data.clone()
    } else {
        sessions_backup::merge::merge(b.base().as_ref(), &local, &remote.data)
    };
    if let Some(o) = merged.as_object_mut() {
        // This device's own home screen stays as it is.
        if let Some(mine) = local.pointer(&format!("/layouts/{DEVICE_KIND}")) {
            if let Some(l) = o.get_mut("layouts").and_then(Value::as_object_mut) {
                l.insert(DEVICE_KIND.into(), mine.clone());
            }
        }
        // A backup that had to leave labels out doesn't delete ours.
        if o.remove("labelsOmitted").is_some() {
            if let Some(l) = local.get("labels") {
                o.insert("labels".into(), l.clone());
            }
        }
    }
    let changed = merged != local;
    if changed {
        apply(
            state,
            &Snapshot {
                version: remote.version,
                created_at: remote.created_at,
                data: merged.clone(),
            },
        )
        .await?;
    }
    // What's on the relays now is the common ground.
    b.set_base(&remote.data);
    let now_local = gather(state).await?;
    let in_step = now_local == remote.data;
    b.update_saved(|s| {
        s.version = s.version.max(remote.version);
        s.last_sync_at = Some(now_secs());
        if replace {
            s.replace_token = token.clone();
        }
        // Nothing of ours to add: don't upload the same thing back.
        if in_step {
            s.hash = Some(snapshot::content_hash(&now_local));
        }
    });
    if !in_step {
        b.touch();
    }
    if changed {
        if let Some(emit) = b.emit.get() {
            emit("backup://restored", serde_json::json!({ "found": true, "accounts": 0, "labels": 0 }));
        }
    }
    b.changed();
    Ok(changed)
}

/// A device linked to this wallet.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkedDevice {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub seen_at: Option<u64>,
    pub this_device: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub devices: Vec<LinkedDevice>,
    pub last_sync_at: Option<u64>,
}

pub fn sync_status(state: &AppState) -> SyncStatus {
    let b = &state.backup;
    let me = b.device_id();
    let mut devices: Vec<LinkedDevice> = b
        .base()
        .and_then(|v| v.get("devices").and_then(Value::as_object).cloned())
        .unwrap_or_default()
        .into_iter()
        .map(|(id, d)| LinkedDevice {
            this_device: id == me,
            name: d["name"].as_str().unwrap_or("Device").to_string(),
            kind: d["kind"].as_str().unwrap_or("").to_string(),
            seen_at: d["seenAt"].as_u64(),
            id,
        })
        .collect();
    if !devices.iter().any(|d| d.this_device) {
        devices.push(LinkedDevice {
            id: me,
            name: crate::link::device_name().into(),
            kind: DEVICE_KIND.into(),
            seen_at: Some(now_secs()),
            this_device: true,
        });
    }
    devices.sort_by_key(|d| (!d.this_device, d.name.clone()));
    SyncStatus {
        devices,
        last_sync_at: b.lock().saved.last_sync_at,
    }
}

/// Which way a resync goes.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncDirection {
    /// Merge both ways (the normal sync, now).
    Both,
    /// This device's setup replaces the others'.
    SendMine,
    /// The newest setup from another device replaces this one's.
    TakeTheirs,
}

/// Sync now: on opening the app, or "Resync" in Settings.
pub async fn sync_now(state: &AppState, direction: SyncDirection) -> Result<SyncStatus> {
    let b = &state.backup;
    if !b.online {
        return Err(Error::Unsupported("sync while offline".into()));
    }
    let keys = b.lock().keys.clone().ok_or(Error::Locked)?;
    if b.lock().saved.restore_pending {
        let _ = restore(state).await;
    }
    match direction {
        SyncDirection::Both => {
            pull(state, &keys, false).await?;
            if b.lock().sched.pending() {
                backup_now(state).await?;
            }
        }
        SyncDirection::TakeTheirs => {
            pull(state, &keys, true).await?;
        }
        SyncDirection::SendMine => {
            let token = random_hex(8);
            {
                let mut i = b.lock();
                i.send_token = Some(token.clone());
            }
            b.update_saved(|s| s.replace_token = Some(token));
            let r = backup_now(state).await;
            b.lock().send_token = None;
            r?;
        }
    }
    Ok(sync_status(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AccountSpec;
    use rail_onchain::descriptors;
    use sessions_core::keystore::Seed;
    use sessions_core::{LabelKind, Network};

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const XPUB: &str = "xpub6CatWdiZiodmUeTDp8LT5or8nmbKNcuyvz7WyksVFkKB4RHwCD3XyuvPEbvqAQY3rAPshWcMLoP2fMFMKHPJ4ZeZXYVUhLv1VMrjPC7PW6V";
    const TXID: &str = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";

    fn keys() -> StateKeys {
        let seed = Seed::from_phrase(ABANDON).unwrap();
        StateKeys::from_master(&seed.master_xpriv(Network::Bitcoin).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn state_round_trips_to_a_restored_wallet() {
        // The old phone: accounts, a rename, a watch-only, a label, layout,
        // prefs, a custom server and a Breez key.
        let dir_a = tempfile::tempdir().unwrap();
        let a = AppState::new(dir_a.path().to_path_buf(), None);
        a.create(Seed::from_phrase(ABANDON).unwrap(), "123456")
            .await
            .unwrap();
        let savings = a
            .add_account(AccountSpec::Savings {
                id: "savings".into(),
                name: "On-Chain".into(),
                account_index: 0,
                payjoin_receive: false,
            })
            .await
            .unwrap();
        a.rename_account(&savings, "Rainy day").await.unwrap();
        a.add_account(AccountSpec::WatchOnly {
            id: "watch-1".into(),
            name: "Dad's".into(),
            descriptors: descriptors::import(
                &format!("[73c5da0a/84h/0h/0h]{XPUB}"),
                Network::Bitcoin,
            )
            .unwrap(),
        })
        .await
        .unwrap();
        {
            let mut g = a.inner.lock().await;
            let u = g.as_mut().unwrap();
            u.wallet
                .set_label(&savings, LabelKind::Tx, TXID, Some("rent".into()))
                .await
                .unwrap();
            u.config.layout.advanced = true;
            u.config.prefs =
                serde_json::json!({ "theme": "sessions", "currencies": ["CAD", "EUR"] });
            u.config.electrum_url = Some("ssl://electrum.example:50002".into());
            u.config.breez_api_key = Some("my-secret-breez-key".into());
            u.config.backup.last_tested_at = Some(1_700_000_123);
            a.save_config(&u.config).unwrap();
        }
        assert!(a.backup.status().pending);

        let data = gather(&a).await.unwrap();
        let text = data.to_string();
        assert!(!text.contains("my-secret-breez-key"));
        // Local bookkeeping, and testing isn't a change to back up.
        assert!(!text.contains("lastTestedAt"));
        assert!(!text.contains("abandon") && !text.contains("xprv"));
        assert!(text.contains("rent") && text.contains("Rainy day"));

        // Through the real encoding, as it would go via the relays.
        let snap = Snapshot {
            version: 42,
            created_at: 1_700_000_000,
            data,
        };
        let events = snapshot::encode(&keys(), &snap, 1_700_000_000)
            .unwrap()
            .unwrap();
        let back = snapshot::decode(&keys(), &events).unwrap();
        assert_eq!(counts(&back), (3, 1));

        // The new phone: restored from the phrase, holding uploads.
        let dir_b = tempfile::tempdir().unwrap();
        let b = AppState::new(dir_b.path().to_path_buf(), None);
        b.create_with(
            Seed::from_phrase(ABANDON).unwrap(),
            "654321",
            BackupFlags {
                passkey: false,
                phrase: true,
                seed_origin: SeedOrigin::Phrase,
                credential_id: None,
                last_tested_at: None,
            },
            true,
        )
        .await
        .unwrap();
        assert!(b.backup.lock().saved.restore_pending);
        {
            let mut g = b.inner.lock().await;
            g.as_mut().unwrap().config.breez_api_key = Some("phone-b-key".into());
        }

        let (accounts, labels) = apply(&b, &back).await.unwrap();
        assert_eq!((accounts, labels), (3, 1));
        assert!(!b.backup.lock().saved.restore_pending);
        assert_eq!(b.backup.lock().saved.version, 42);

        let g = b.inner.lock().await;
        let u = g.as_ref().unwrap();
        let info = u
            .wallet
            .account(&savings)
            .await
            .unwrap()
            .info()
            .await
            .unwrap();
        assert_eq!(info.name, "Rainy day");
        assert!(u.wallet.account(&"watch-1".into()).await.is_ok());
        assert!(u.config.layout.advanced);
        assert_eq!(u.config.prefs["currencies"][1], "EUR");
        assert_eq!(
            u.config.electrum_url.as_deref(),
            Some("ssl://electrum.example:50002")
        );
        // Local key kept, the old phone's never travels.
        assert_eq!(u.config.breez_api_key.as_deref(), Some("phone-b-key"));
        assert!(u.config.backup.phrase);
        // The label is back on the reopened account.
        let restored = std::fs::read_to_string(b.paths.labels(&savings)).unwrap();
        assert!(restored.contains("rent") && restored.contains(TXID));
    }

    #[test]
    fn ids_are_checked_before_becoming_paths() {
        assert!(safe_id("hw-3") && safe_id("savings") && safe_id("vault_1"));
        assert!(!safe_id("../config") && !safe_id("a/b") && !safe_id("") && !safe_id("x.y"));
    }
}

//! Push, not poll: on-chain updates pushed by the chain server.
//!
//! While the wallet is unlocked, one connection stays open to the chain
//! server: Electrum subscriptions (the default), or the websocket of a
//! mempool server (an Esplora URL like mempool.space or a self-hosted
//! mempool on Umbrel). It
//!
//! * listens for new blocks and then light-syncs every on-chain account
//!   (confirmations, spends from another device);
//! * tracks where new payments are expected: each account's handed-out but
//!   unpaid receive addresses, and the address on a Receive screen (see
//!   [`Monitor::watch`]). A transaction to one of them is fetched and added
//!   to its account right away, usually within a second or two of being
//!   broadcast.
//!
//! Accounts report changes through their change hook (the `wallet://changed`
//! event). Plain Esplora servers have no websocket: those keep polling.
//!
//! Addresses outside the on-chain accounts can be watched too (see
//! [`Watcher::watch_external`]): the Spending account's bitcoin deposit
//! address, whose owner is told about payments to it and about new blocks.
//!
//! Electrum (`blockchain.headers.subscribe`, `blockchain.scripthash.subscribe`):
//! up to [`MAX_SCRIPTS_ELECTRUM`] scripts, far more than mempool's 10. A
//! status change means the script's history changed: its history is fetched
//! and new transactions added to the account. `server.ping` every
//! [`PING_EVERY`] keeps the connection (and NAT mappings) alive.
//!
//! mempool's websocket API (backend `websocket-handler.ts`):
//! * `{"action":"want","data":["blocks"]}` → `{"blocks":[…]}` now and
//!   `{"block":{…}}` on every new block.
//! * `{"track-addresses":[…]}` / `{"track-scriptpubkeys":["<hex>",…]}`
//!   (each replaces the previous list; an empty list stops tracking) →
//!   `{"multi-address-transactions":{"<addr>":{"mempool":[tx…],
//!   "confirmed":[tx…],"removed":[tx…]}}}` and the same under
//!   `multi-scriptpubkey-transactions` keyed by script hex.
//! * Too many: `{"track-addresses-error":"… a maximum of 10 addresses"}` and
//!   that list is dropped. mempool.space allows 10 of each (checked
//!   2026-10-22), so up to 20 watched scripts per connection.
//! * `{"action":"ping"}` → `{"pong":true}`. The server sends no keepalives.

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rail_onchain::electrum::{self as el, Servers};
use rail_onchain::push::{Notification, PushConn};
use rail_onchain::{ChainServer, OnchainAccount, Txid};
use serde_json::{json, Value};
use sessions_core::{AccountId, Rail};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

/// Addresses / scripts mempool.space tracks per connection (each list).
const MAX_ADDRESSES: usize = 10;
const MAX_SCRIPTS: usize = 10;
/// Unpaid receive addresses watched per account.
const PER_ACCOUNT: usize = 5;
/// The same over Electrum, which allows many more subscriptions.
const PER_ACCOUNT_ELECTRUM: usize = 20;
/// Scripts subscribed to over Electrum, in all.
const MAX_SCRIPTS_ELECTRUM: usize = 100;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const PING_EVERY: Duration = Duration::from_secs(30);
/// No message (not even a pong) for this long: the connection is dead.
const STALE_AFTER: Duration = Duration::from_secs(75);
/// Re-check the watch set this often (addresses handed out elsewhere).
const REFRESH_EVERY: Duration = Duration::from_secs(60);
const MIN_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// The websocket of a mempool server, from its Esplora API base URL:
/// `https://HOST[/prefix]/api` → `wss://HOST[/prefix]/api/v1/ws`. `None`
/// for servers that aren't mempool (no `/api` path, or Blockstream's).
pub fn ws_url(esplora: &str) -> Option<String> {
    let s = esplora.trim().trim_end_matches('/');
    let (scheme, rest) = match s.strip_prefix("https://") {
        Some(r) => ("wss://", r),
        None => ("ws://", s.strip_prefix("http://")?),
    };
    let host = rest.split('/').next().unwrap_or("").to_ascii_lowercase();
    if !rest.ends_with("/api") || host.is_empty() || host.ends_with("blockstream.info") {
        return None;
    }
    Some(format!("{scheme}{rest}/v1/ws"))
}

/// Something the server told us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A new block was mined.
    Block,
    /// Transactions touching a tracked address (or script, by hex).
    Txs {
        key: String,
        mempool: Vec<String>,
        confirmed: Vec<String>,
        removed: Vec<String>,
    },
    /// The server refused a tracking list (`scripts`: the script list),
    /// usually because it was longer than `max`.
    TrackError {
        scripts: bool,
        max: Option<usize>,
    },
    Pong,
}

fn txids(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("txid").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// "… supports tracking a maximum of 10 addresses" → 10.
fn max_in(message: &str) -> Option<usize> {
    let tail = message.split("maximum of").nth(1)?;
    tail.split_whitespace().next()?.parse().ok()
}

/// Everything of interest in one websocket message (one message can carry
/// several keys).
pub fn parse(text: &str) -> Vec<Event> {
    let Ok(Value::Object(msg)) = serde_json::from_str::<Value>(text) else {
        return vec![];
    };
    let mut out = Vec::new();
    for (k, v) in &msg {
        match k.as_str() {
            "block" => out.push(Event::Block),
            "pong" => out.push(Event::Pong),
            "multi-address-transactions" | "multi-scriptpubkey-transactions" => {
                if let Some(map) = v.as_object() {
                    for (key, txs) in map {
                        let ev = Event::Txs {
                            key: key.clone(),
                            mempool: txids(txs.get("mempool")),
                            confirmed: txids(txs.get("confirmed")),
                            removed: txids(txs.get("removed")),
                        };
                        if !matches!(&ev, Event::Txs { mempool, confirmed, removed, .. }
                            if mempool.is_empty() && confirmed.is_empty() && removed.is_empty())
                        {
                            out.push(ev);
                        }
                    }
                }
            }
            "track-addresses-error" | "track-scriptpubkeys-error" => out.push(Event::TrackError {
                scripts: k == "track-scriptpubkeys-error",
                max: v.as_str().and_then(max_in),
            }),
            _ => {}
        }
    }
    out
}

/// What to subscribe to, and which account each tracked key belongs to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchPlan {
    /// Sent as `track-addresses`.
    pub addresses: Vec<String>,
    /// Sent as `track-scriptpubkeys` (hex), once the address list is full.
    pub scripts: Vec<String>,
    /// Address or script hex → account.
    pub owner: HashMap<String, AccountId>,
}

/// Pick what to watch within the server's limits: addresses on a Receive
/// screen first, then each account's unpaid addresses in turn (so every
/// account gets its next address watched before any gets a second).
pub fn plan_watch(
    shown: &BTreeMap<AccountId, String>,
    candidates: &[(AccountId, Vec<String>)],
    max_addresses: usize,
    max_scripts: usize,
) -> WatchPlan {
    let mut order: Vec<(AccountId, String)> =
        shown.iter().map(|(a, s)| (a.clone(), s.clone())).collect();
    let longest = candidates.iter().map(|(_, c)| c.len()).max().unwrap_or(0);
    for i in 0..longest {
        for (a, c) in candidates {
            if let Some(addr) = c.get(i) {
                order.push((a.clone(), addr.clone()));
            }
        }
    }
    let mut plan = WatchPlan::default();
    let mut seen = std::collections::HashSet::new();
    for (account, address) in order {
        if !seen.insert(address.clone()) {
            continue;
        }
        if plan.addresses.len() < max_addresses {
            plan.owner.insert(address.clone(), account);
            plan.addresses.push(address);
        } else if plan.scripts.len() < max_scripts {
            let Ok(a) = bitcoin::Address::from_str(&address) else {
                continue;
            };
            let hex = a.assume_checked().script_pubkey().to_hex_string();
            plan.owner.insert(hex.clone(), account);
            plan.scripts.push(hex);
        } else {
            break;
        }
    }
    plan
}

enum Cmd {
    /// The watch set may have changed (address handed out, payment found,
    /// account added or removed).
    Refresh,
    /// A Receive screen shows this address.
    Watch(AccountId, String),
}

type Accounts = Arc<std::sync::RwLock<Vec<Arc<OnchainAccount>>>>;

/// What the chain server reported, for an externally watched address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// A transaction to (or from) the address appeared or confirmed.
    Payment,
    /// A new block.
    Block,
}

/// Told about [`Activity`] on an externally watched address. Called on the
/// monitor's task: it must not block (spawn the work).
pub type ActivityHook = Arc<dyn Fn(Activity) + Send + Sync>;

/// Externally watched addresses, by owner: (address, hook).
type Extras = Arc<std::sync::RwLock<BTreeMap<AccountId, (String, ActivityHook)>>>;

fn extras_list(extras: &Extras) -> Vec<(AccountId, String, ActivityHook)> {
    extras
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(id, (a, h))| (id.clone(), a.clone(), h.clone()))
        .collect()
}

/// Tell every external watcher about `activity`.
fn tell_extras(extras: &Extras, activity: Activity) {
    for (_, _, hook) in extras_list(extras) {
        hook(activity);
    }
}

/// `owner`'s external watch, if it has one: tell it about a payment.
fn tell_extra(extras: &Extras, owner: &AccountId) -> bool {
    let hook = extras
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(owner)
        .map(|(_, h)| h.clone());
    match hook {
        Some(h) => {
            h(Activity::Payment);
            true
        }
        None => false,
    }
}

/// The external watches first (always watched), then the accounts'.
fn with_extras(
    extras: &Extras,
    accounts: Vec<(AccountId, Vec<String>)>,
) -> Vec<(AccountId, Vec<String>)> {
    extras_list(extras)
        .into_iter()
        .map(|(id, address, _)| (id, vec![address]))
        .chain(accounts)
        .collect()
}

/// The running monitor. Dropping it (locking the wallet) closes the socket.
pub struct Monitor {
    tx: mpsc::UnboundedSender<Cmd>,
    accounts: Accounts,
    extras: Extras,
    task: Option<tokio::task::JoinHandle<()>>,
}

/// Adds external watches to a running monitor; usable from other tasks.
/// Does nothing once the monitor is gone.
#[derive(Clone)]
pub struct Watcher {
    tx: mpsc::UnboundedSender<Cmd>,
    extras: Extras,
}

impl Watcher {
    /// Watch `address` for `owner` (not an on-chain account), replacing any
    /// address it watched before: `hook` hears about payments to it and
    /// about new blocks.
    pub fn watch_external(&self, owner: AccountId, address: String, hook: ActivityHook) {
        self.extras
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(owner, (address, hook));
        let _ = self.tx.send(Cmd::Refresh);
    }

    pub fn unwatch_external(&self, owner: &AccountId) {
        if self
            .extras
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(owner)
            .is_some()
        {
            let _ = self.tx.send(Cmd::Refresh);
        }
    }
}

impl Monitor {
    /// Start for the wallet's chain server. Without push (plain Esplora) it
    /// only kicks off first scans (see [`Monitor::set_accounts`]).
    pub fn start(server: &ChainServer, accounts: Vec<Arc<OnchainAccount>>) -> Self {
        let shared: Accounts = Default::default();
        let extras: Extras = Default::default();
        let (tx, rx) = mpsc::unbounded_channel();
        let task = match (server, tokio::runtime::Handle::try_current()) {
            (ChainServer::Electrum(list), Ok(rt)) => match el::pool_for(list) {
                // The pool's server list: push and requests agree on which
                // server works.
                Ok(pool) => Some(rt.spawn(run_electrum(
                    pool.servers().clone(),
                    shared.clone(),
                    extras.clone(),
                    tx.clone(),
                    rx,
                ))),
                Err(e) => {
                    tracing::warn!("no push updates: {e}");
                    None
                }
            },
            (ChainServer::Esplora(url), Ok(rt)) => ws_url(url)
                .map(|ws| rt.spawn(run(ws, shared.clone(), extras.clone(), tx.clone(), rx))),
            _ => None,
        };
        let m = Monitor {
            tx,
            accounts: shared,
            extras,
            task,
        };
        m.set_accounts(accounts);
        m
    }

    /// The on-chain accounts to look after. Accounts whose history hasn't
    /// loaded yet start their first scan now (in the background), so a
    /// Receive screen opened right after adding an account already has it
    /// under way.
    pub fn set_accounts(&self, accounts: Vec<Arc<OnchainAccount>>) {
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            for a in &accounts {
                let a = a.clone();
                rt.spawn(async move {
                    if a.scanning().await {
                        let _ = a.sync().await;
                    }
                });
            }
        }
        *self.accounts.write().unwrap_or_else(|e| e.into_inner()) = accounts;
        let _ = self.tx.send(Cmd::Refresh);
    }

    /// Watch `address` of `account` (shown on a Receive screen) from now on.
    pub fn watch(&self, account: AccountId, address: String) {
        let _ = self.tx.send(Cmd::Watch(account, address));
    }

    /// For adding external watches from another task.
    pub fn watcher(&self) -> Watcher {
        Watcher {
            tx: self.tx.clone(),
            extras: self.extras.clone(),
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

fn list(accounts: &Accounts) -> Vec<Arc<OnchainAccount>> {
    accounts.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Light-sync every account (after a block, or a reconnect that may have
/// missed something). Skipped while the previous round is still going.
fn sync_all(accounts: &Accounts, tx: &mpsc::UnboundedSender<Cmd>, busy: &Arc<AtomicBool>) {
    if busy.swap(true, Ordering::SeqCst) {
        return;
    }
    let (list, tx, busy) = (list(accounts), tx.clone(), busy.clone());
    tokio::spawn(async move {
        for a in list {
            if let Err(e) = a.sync().await {
                tracing::debug!(account = %a.id(), "sync after block: {e}");
            }
        }
        busy.store(false, Ordering::SeqCst);
        let _ = tx.send(Cmd::Refresh);
    });
}

enum ConnectError {
    /// The server answered but has no websocket there: stop trying.
    Refused(u16),
    Other(String),
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(url: &str) -> Result<Ws, ConnectError> {
    use tokio_tungstenite::tungstenite::Error;
    match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url)).await {
        Err(_) => Err(ConnectError::Other("timed out".into())),
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(Error::Http(resp))) if resp.status().is_client_error() => {
            Err(ConnectError::Refused(resp.status().as_u16()))
        }
        Ok(Err(e)) => Err(ConnectError::Other(e.to_string())),
    }
}

async fn run(
    url: String,
    accounts: Accounts,
    extras: Extras,
    tx: mpsc::UnboundedSender<Cmd>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let mut shown = BTreeMap::new();
    let busy = Arc::new(AtomicBool::new(false));
    let mut backoff = MIN_BACKOFF;
    let mut first = true;
    loop {
        let started = Instant::now();
        match connect(&url).await {
            Ok(ws) => {
                tracing::info!("chain server websocket connected ({url})");
                if !first {
                    // Anything that happened while we were disconnected.
                    sync_all(&accounts, &tx, &busy);
                }
                first = false;
                let mut s = Session {
                    accounts: &accounts,
                    extras: &extras,
                    tx: &tx,
                    busy: &busy,
                    shown: &mut shown,
                    plan: WatchPlan::default(),
                    limits: (MAX_ADDRESSES, MAX_SCRIPTS),
                };
                match s.run(ws, &mut rx).await {
                    Ok(()) => return,
                    Err(why) => tracing::info!("chain server websocket dropped: {why}"),
                }
                if started.elapsed() > MAX_BACKOFF {
                    backoff = MIN_BACKOFF;
                }
            }
            Err(ConnectError::Refused(status)) => {
                tracing::info!("{url} has no websocket (HTTP {status}); polling only");
                return;
            }
            Err(ConnectError::Other(e)) => tracing::debug!("websocket {url}: {e}"),
        }
        if !wait_to_reconnect(backoff, &mut rx, &mut shown).await {
            return;
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Wait before reconnecting, but go at once when a Receive screen wants an
/// address watched. `false` when the monitor was dropped.
async fn wait_to_reconnect(
    backoff: Duration,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    shown: &mut BTreeMap<AccountId, String>,
) -> bool {
    let wait = tokio::time::sleep(backoff);
    tokio::pin!(wait);
    loop {
        tokio::select! {
            _ = &mut wait => return true,
            cmd = rx.recv() => match cmd {
                None => return false,
                Some(Cmd::Watch(a, address)) => {
                    shown.insert(a, address);
                    return true;
                }
                Some(Cmd::Refresh) => {}
            },
        }
    }
}

struct Session<'a> {
    accounts: &'a Accounts,
    extras: &'a Extras,
    tx: &'a mpsc::UnboundedSender<Cmd>,
    busy: &'a Arc<AtomicBool>,
    shown: &'a mut BTreeMap<AccountId, String>,
    /// What the server is tracking for us now.
    plan: WatchPlan,
    /// Server limits (address list, script list).
    limits: (usize, usize),
}

impl Session<'_> {
    /// `Ok` when the monitor was dropped, `Err` when the connection was lost.
    async fn run(&mut self, ws: Ws, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Result<(), String> {
        let (mut sink, mut stream) = ws.split();
        let send = |v: Value| Message::text(v.to_string());
        sink.send(send(json!({"action": "want", "data": ["blocks"]})))
            .await
            .map_err(|e| e.to_string())?;
        for m in self.resubscribe().await {
            sink.send(m).await.map_err(|e| e.to_string())?;
        }
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        let mut refresh = tokio::time::interval(REFRESH_EVERY);
        refresh.tick().await;
        let mut last_heard = Instant::now();
        loop {
            let out: Vec<Message> = tokio::select! {
                msg = stream.next() => {
                    last_heard = Instant::now();
                    match msg {
                        Some(Ok(Message::Text(t))) => {
                            let mut resub = false;
                            for ev in parse(t.as_str()) {
                                resub |= self.handle(ev);
                            }
                            if resub { self.resubscribe().await } else { vec![] }
                        }
                        Some(Ok(Message::Close(_))) | None => return Err("closed by server".into()),
                        Some(Err(e)) => return Err(e.to_string()),
                        Some(Ok(_)) => vec![],
                    }
                }
                cmd = rx.recv() => match cmd {
                    None => return Ok(()),
                    Some(Cmd::Watch(a, address)) => {
                        self.shown.insert(a, address);
                        self.resubscribe().await
                    }
                    Some(Cmd::Refresh) => self.resubscribe().await,
                },
                _ = ping.tick() => {
                    if last_heard.elapsed() > STALE_AFTER {
                        return Err("no reply from server".into());
                    }
                    vec![send(json!({"action": "ping"}))]
                }
                _ = refresh.tick() => self.resubscribe().await,
            };
            for m in out {
                sink.send(m).await.map_err(|e| e.to_string())?;
            }
        }
    }

    /// React to one event. `true` when the subscription must be resent.
    fn handle(&mut self, ev: Event) -> bool {
        match ev {
            Event::Block => {
                sync_all(self.accounts, self.tx, self.busy);
                tell_extras(self.extras, Activity::Block);
                false
            }
            Event::Pong => false,
            Event::TrackError { scripts, max } => {
                let limit = if scripts {
                    &mut self.limits.1
                } else {
                    &mut self.limits.0
                };
                // Use what the server allows; give up on that list if it
                // won't say (or keeps refusing).
                *limit = max.filter(|m| *m < *limit).unwrap_or(0);
                tracing::info!(
                    "server tracks at most {} {}",
                    limit,
                    if scripts { "scripts" } else { "addresses" }
                );
                // The server dropped that list: make sure it's resent.
                if scripts {
                    self.plan.scripts.clear();
                } else {
                    self.plan.addresses.clear();
                }
                true
            }
            Event::Txs {
                key,
                mempool,
                confirmed,
                removed,
            } => {
                let Some(owner) = self.plan.owner.get(&key) else {
                    return false;
                };
                let Some(account) = list(self.accounts).into_iter().find(|a| a.id() == owner)
                else {
                    tell_extra(self.extras, owner);
                    return false;
                };
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let ids: Vec<Txid> = mempool
                        .iter()
                        .chain(&confirmed)
                        .filter_map(|t| Txid::from_str(t).ok())
                        .collect();
                    if !ids.is_empty() {
                        if let Err(e) = account.apply_txids(ids).await {
                            tracing::warn!(account = %account.id(), "pushed transaction: {e}");
                        }
                    }
                    // Confirmations and replacements come from a sync.
                    if !confirmed.is_empty() || !removed.is_empty() {
                        let _ = account.sync().await;
                    }
                    let _ = tx.send(Cmd::Refresh);
                });
                false
            }
        }
    }

    /// Recompute the watch set; the messages to send if it changed.
    async fn resubscribe(&mut self) -> Vec<Message> {
        let accounts = list(self.accounts);
        self.shown
            .retain(|id, _| accounts.iter().any(|a| a.id() == id));
        let mut candidates = Vec::with_capacity(accounts.len());
        for a in &accounts {
            candidates.push((a.id().clone(), a.watch_addresses(PER_ACCOUNT).await));
        }
        let candidates = with_extras(self.extras, candidates);
        let plan = plan_watch(self.shown, &candidates, self.limits.0, self.limits.1);
        let mut out = Vec::new();
        if plan.addresses != self.plan.addresses {
            out.push(Message::text(
                json!({ "track-addresses": plan.addresses }).to_string(),
            ));
        }
        if plan.scripts != self.plan.scripts {
            out.push(Message::text(
                json!({ "track-scriptpubkeys": plan.scripts }).to_string(),
            ));
        }
        self.plan = plan;
        out
    }
}

// ---------------------------------------------------------------------------
// Electrum
// ---------------------------------------------------------------------------

/// The scripts subscribed to over Electrum: who owns each one and its last
/// known status (a hash of its history; `None` while it has none).
#[derive(Debug, Default)]
pub struct ScriptWatch {
    subs: HashMap<String, (AccountId, Option<String>)>,
}

impl ScriptWatch {
    /// Bring the subscriptions in line with `wanted` (script hash → owner).
    /// Forgets the ones no longer wanted and returns them (to unsubscribe),
    /// with the new ones (to subscribe). Owners of kept ones are updated.
    pub fn diff(&mut self, wanted: &HashMap<String, AccountId>) -> (Vec<String>, Vec<String>) {
        let gone: Vec<String> = self
            .subs
            .keys()
            .filter(|h| !wanted.contains_key(*h))
            .cloned()
            .collect();
        for h in &gone {
            self.subs.remove(h);
        }
        let mut new = Vec::new();
        for (h, account) in wanted {
            match self.subs.get_mut(h) {
                Some(sub) => sub.0 = account.clone(),
                None => new.push(h.clone()),
            }
        }
        new.sort();
        (new, gone)
    }

    /// Subscribed to `hash`: its status now. `true` when it already has a
    /// history, which for an address we expected to be unused means a
    /// payment arrived (worth a look).
    pub fn subscribed(&mut self, hash: String, account: AccountId, status: Option<String>) -> bool {
        let has_history = status.is_some();
        self.subs.insert(hash, (account, status));
        has_history
    }

    /// The server says `hash` has a new status: its owner when that's a
    /// real change of a script we watch.
    pub fn update(&mut self, hash: &str, status: Option<String>) -> Option<AccountId> {
        let sub = self.subs.get_mut(hash)?;
        if sub.1 == status {
            return None;
        }
        sub.1 = status;
        Some(sub.0.clone())
    }

    #[cfg(test)]
    pub fn owner(&self, hash: &str) -> Option<&AccountId> {
        self.subs.get(hash).map(|s| &s.0)
    }

    pub fn len(&self) -> usize {
        self.subs.len()
    }
}

/// Script hash → owner for a watch plan's addresses.
fn script_hashes(plan: &WatchPlan) -> HashMap<String, AccountId> {
    plan.addresses
        .iter()
        .filter_map(|a| {
            let spk = bitcoin::Address::from_str(a)
                .ok()?
                .assume_checked()
                .script_pubkey();
            Some((el::script_hash(&spk), plan.owner.get(a)?.clone()))
        })
        .collect()
}

async fn run_electrum(
    servers: Arc<Servers>,
    accounts: Accounts,
    extras: Extras,
    tx: mpsc::UnboundedSender<Cmd>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let mut shown = BTreeMap::new();
    let busy = Arc::new(AtomicBool::new(false));
    let mut backoff = MIN_BACKOFF;
    let mut first = true;
    loop {
        // Nothing to watch: no connection.
        while list(&accounts).is_empty() && extras_list(&extras).is_empty() {
            match rx.recv().await {
                None => return,
                Some(Cmd::Watch(a, address)) => {
                    shown.insert(a, address);
                }
                Some(Cmd::Refresh) => {}
            }
        }
        let started = Instant::now();
        match PushConn::connect(&servers).await {
            Ok((conn, notes)) => {
                tracing::info!("push updates from Electrum server {}", conn.server());
                if !first {
                    // Anything that happened while we were disconnected.
                    sync_all(&accounts, &tx, &busy);
                }
                first = false;
                let mut s = ElectrumSession {
                    accounts: &accounts,
                    extras: &extras,
                    tx: &tx,
                    busy: &busy,
                    shown: &mut shown,
                    watch: ScriptWatch::default(),
                    tip: None,
                };
                match s.run(&conn, notes, &mut rx).await {
                    Ok(()) => return,
                    Err(why) => tracing::info!("Electrum push connection dropped: {why}"),
                }
                if started.elapsed() > MAX_BACKOFF {
                    backoff = MIN_BACKOFF;
                }
            }
            Err(e) => tracing::info!("no Electrum server for push updates: {e}"),
        }
        if !wait_to_reconnect(backoff, &mut rx, &mut shown).await {
            return;
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

struct ElectrumSession<'a> {
    accounts: &'a Accounts,
    extras: &'a Extras,
    tx: &'a mpsc::UnboundedSender<Cmd>,
    busy: &'a Arc<AtomicBool>,
    shown: &'a mut BTreeMap<AccountId, String>,
    watch: ScriptWatch,
    tip: Option<u32>,
}

impl ElectrumSession<'_> {
    /// `Ok` when the monitor was dropped, `Err` when the connection was lost.
    async fn run(
        &mut self,
        conn: &PushConn,
        mut notes: mpsc::UnboundedReceiver<Notification>,
        rx: &mut mpsc::UnboundedReceiver<Cmd>,
    ) -> Result<(), String> {
        self.tip = Some(conn.subscribe_headers().await?);
        self.resubscribe(conn).await?;
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        let mut refresh = tokio::time::interval(REFRESH_EVERY);
        refresh.tick().await;
        loop {
            tokio::select! {
                n = notes.recv() => match n {
                    None => return Err("closed by server".into()),
                    Some(Notification::Header { height }) => {
                        if self.tip != Some(height) {
                            self.tip = Some(height);
                            sync_all(self.accounts, self.tx, self.busy);
                            tell_extras(self.extras, Activity::Block);
                        }
                    }
                    Some(Notification::Script { hash, status }) => {
                        if let Some(account) = self.watch.update(&hash, status) {
                            self.changed(conn, &hash, account).await?;
                        }
                    }
                },
                cmd = rx.recv() => match cmd {
                    None => return Ok(()),
                    Some(Cmd::Watch(a, address)) => {
                        self.shown.insert(a, address);
                        self.resubscribe(conn).await?;
                    }
                    Some(Cmd::Refresh) => self.resubscribe(conn).await?,
                },
                _ = ping.tick() => conn.ping().await?,
                _ = refresh.tick() => self.resubscribe(conn).await?,
            }
        }
    }

    /// A watched script's history changed: add its new transactions to the
    /// account (in the background), then refresh the watch set.
    async fn changed(
        &mut self,
        conn: &PushConn,
        hash: &str,
        owner: AccountId,
    ) -> Result<(), String> {
        // An external watch: its owner looks for itself.
        if tell_extra(self.extras, &owner) {
            return Ok(());
        }
        let history = conn.history(hash).await?;
        let Some(account) = list(self.accounts).into_iter().find(|a| *a.id() == owner) else {
            return Ok(());
        };
        let ids: Vec<Txid> = history
            .iter()
            .filter_map(|h| Txid::from_str(&h.txid).ok())
            .collect();
        let confirmed = history.iter().any(|h| h.height > 0);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if !ids.is_empty() {
                if let Err(e) = account.apply_txids(ids).await {
                    tracing::warn!(account = %account.id(), "pushed transaction: {e}");
                }
            }
            // Confirmations come from a sync.
            if confirmed {
                let _ = account.sync().await;
            }
            let _ = tx.send(Cmd::Refresh);
        });
        Ok(())
    }

    /// Recompute the watch set and (un)subscribe the difference.
    async fn resubscribe(&mut self, conn: &PushConn) -> Result<(), String> {
        let accounts = list(self.accounts);
        self.shown
            .retain(|id, _| accounts.iter().any(|a| a.id() == id));
        let mut candidates = Vec::with_capacity(accounts.len());
        for a in &accounts {
            candidates.push((
                a.id().clone(),
                a.watch_addresses(PER_ACCOUNT_ELECTRUM).await,
            ));
        }
        let candidates = with_extras(self.extras, candidates);
        let plan = plan_watch(self.shown, &candidates, MAX_SCRIPTS_ELECTRUM, 0);
        let wanted = script_hashes(&plan);
        let (new, gone) = self.watch.diff(&wanted);
        if !gone.is_empty() {
            conn.unsubscribe_scripts(&gone).await;
        }
        if new.is_empty() {
            return Ok(());
        }
        let statuses = conn.subscribe_scripts(&new).await;
        let mut active = Vec::new();
        for (hash, status) in new.into_iter().zip(statuses) {
            let status = status?;
            let account = wanted[&hash].clone();
            if self.watch.subscribed(hash.clone(), account.clone(), status) {
                active.push((hash, account));
            }
        }
        tracing::debug!("watching {} scripts over Electrum", self.watch.len());
        // Paid while we weren't watching.
        for (hash, account) in active {
            self.changed(conn, &hash, account).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_url_from_esplora_url() {
        assert_eq!(
            ws_url("https://mempool.space/api").as_deref(),
            Some("wss://mempool.space/api/v1/ws")
        );
        assert_eq!(
            ws_url(" https://mempool.space/testnet/api/ ").as_deref(),
            Some("wss://mempool.space/testnet/api/v1/ws")
        );
        assert_eq!(
            ws_url("http://umbrel.local:3006/api").as_deref(),
            Some("ws://umbrel.local:3006/api/v1/ws")
        );
        // Plain Esplora: no websocket.
        assert_eq!(ws_url("https://blockstream.info/api"), None);
        assert_eq!(ws_url("http://umbrel.local:3002"), None);
        assert_eq!(ws_url("http://127.0.0.1:3002"), None);
        assert_eq!(ws_url("mempool.space/api"), None);
    }

    #[test]
    fn parses_multi_address_and_script_transactions() {
        // Shape as sent by mempool.space (trimmed).
        let msg = r#"{"multi-address-transactions": {"bc1qvdv5k5pmk86z8z9x9zuv6ll9s2mlcq3ngkhn5c": {"mempool": [{"txid": "88210f9924483ac584615153b87316869216f8796a416261bd9e3c03819ffe04", "version": 2, "vin": [], "vout": [], "status": {"confirmed": false}}], "confirmed": [], "removed": []}},
                      "multi-scriptpubkey-transactions": {"001463594b503bb1f42388a628b8cd7fe582b7fc0233": {"mempool": [], "confirmed": [{"txid": "0fb402503ee792b7035548da9129702c728d282d818bb38a4193e6d819890a01"}], "removed": [{"txid": "d7b2c6129f55a582f27aa4a6e9b389d100b1e122a747b862ff198e1be9c8c605"}]}}}"#;
        let mut evs = parse(msg);
        evs.sort_by_key(|e| format!("{e:?}"));
        assert_eq!(
            evs,
            vec![
                Event::Txs {
                    key: "001463594b503bb1f42388a628b8cd7fe582b7fc0233".into(),
                    mempool: vec![],
                    confirmed: vec![
                        "0fb402503ee792b7035548da9129702c728d282d818bb38a4193e6d819890a01".into()
                    ],
                    removed: vec![
                        "d7b2c6129f55a582f27aa4a6e9b389d100b1e122a747b862ff198e1be9c8c605".into()
                    ],
                },
                Event::Txs {
                    key: "bc1qvdv5k5pmk86z8z9x9zuv6ll9s2mlcq3ngkhn5c".into(),
                    mempool: vec![
                        "88210f9924483ac584615153b87316869216f8796a416261bd9e3c03819ffe04".into()
                    ],
                    confirmed: vec![],
                    removed: vec![],
                },
            ]
        );
    }

    #[test]
    fn parses_blocks_errors_and_pong() {
        assert_eq!(
            parse(r#"{"block": {"id": "00", "height": 972593}, "mempoolInfo": {}}"#),
            vec![Event::Block]
        );
        // The list sent on subscribing isn't a new block.
        assert!(parse(r#"{"blocks": [{"id": "00", "height": 972592}]}"#).is_empty());
        assert_eq!(
            parse(
                r#"{"track-addresses-error": "too many addresses requested, this connection supports tracking a maximum of 10 addresses"}"#
            ),
            vec![Event::TrackError {
                scripts: false,
                max: Some(10)
            }]
        );
        assert_eq!(
            parse(r#"{"track-scriptpubkeys-error": "nope"}"#),
            vec![Event::TrackError {
                scripts: true,
                max: None
            }]
        );
        assert_eq!(parse(r#"{"pong": true}"#), vec![Event::Pong]);
        assert!(parse("not json").is_empty());
        assert!(parse(r#"{"conversions": {"USD": 81799}}"#).is_empty());
        // Nothing in any list: nothing to do.
        assert!(parse(
            r#"{"multi-address-transactions": {"bc1q": {"mempool": [], "confirmed": [], "removed": []}}}"#
        )
        .is_empty());
    }

    fn id(s: &str) -> AccountId {
        AccountId(s.into())
    }

    #[test]
    fn watch_set_shown_first_then_round_robin_within_limits() {
        // Real mainnet addresses (any valid ones will do).
        let a_addrs: Vec<String> = [
            "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu",
            "bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g",
            "bc1qp59yckz4ae5c4efgw2s5wfyvrz0ala7rgvuz8z",
        ]
        .map(String::from)
        .to_vec();
        let b_addrs: Vec<String> = ["bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq"]
            .map(String::from)
            .to_vec();
        let candidates = vec![(id("a"), a_addrs.clone()), (id("b"), b_addrs.clone())];
        let mut shown = BTreeMap::new();
        // Shown on a Receive screen: also a candidate, watched once.
        shown.insert(id("a"), a_addrs[1].clone());

        let plan = plan_watch(&shown, &candidates, 10, 10);
        assert_eq!(
            plan.addresses,
            vec![
                a_addrs[1].clone(),
                a_addrs[0].clone(),
                b_addrs[0].clone(),
                a_addrs[2].clone()
            ]
        );
        assert!(plan.scripts.is_empty());
        assert_eq!(plan.owner[&b_addrs[0]], id("b"));

        // Address list full: the rest go in as scripts (hex), still owned.
        let plan = plan_watch(&shown, &candidates, 2, 1);
        assert_eq!(plan.addresses, vec![a_addrs[1].clone(), a_addrs[0].clone()]);
        assert_eq!(plan.scripts.len(), 1);
        assert!(plan.scripts[0].starts_with("0014") && plan.scripts[0].len() == 44);
        assert_eq!(plan.owner[&plan.scripts[0]], id("b"));

        // Nothing allowed: nothing watched.
        let plan = plan_watch(&shown, &candidates, 0, 0);
        assert!(plan.addresses.is_empty() && plan.scripts.is_empty() && plan.owner.is_empty());
    }

    #[test]
    fn script_hex_matches_mempools_format() {
        let mut shown = BTreeMap::new();
        shown.insert(
            id("x"),
            "bc1qvdv5k5pmk86z8z9x9zuv6ll9s2mlcq3ngkhn5c".to_string(),
        );
        let plan = plan_watch(&shown, &[], 0, 10);
        // Seen in mempool.space's multi-scriptpubkey-transactions for this address.
        assert_eq!(
            plan.scripts,
            vec!["001463594b503bb1f42388a628b8cd7fe582b7fc0233".to_string()]
        );
    }

    #[test]
    fn electrum_watch_set_diff_and_status_changes() {
        let mut w = ScriptWatch::default();
        let wanted: HashMap<String, AccountId> =
            [("h1".to_string(), id("a")), ("h2".to_string(), id("b"))].into();
        let (new, gone) = w.diff(&wanted);
        assert_eq!(new, vec!["h1".to_string(), "h2".to_string()]);
        assert!(gone.is_empty());
        // h1 unused (no history); h2 already has history: worth a look.
        assert!(!w.subscribed("h1".into(), id("a"), None));
        assert!(w.subscribed("h2".into(), id("b"), Some("s0".into())));
        assert_eq!(w.len(), 2);

        // Same status again (e.g. a duplicate notification): nothing to do.
        assert_eq!(w.update("h2", Some("s0".into())), None);
        // A payment to h1.
        assert_eq!(w.update("h1", Some("s1".into())), Some(id("a")));
        assert_eq!(w.update("h1", Some("s1".into())), None);
        // Replaced / dropped from the mempool: back to no history is a change too.
        assert_eq!(w.update("h1", None), Some(id("a")));
        // Not watched (e.g. after unsubscribing on a server that keeps sending).
        assert_eq!(w.update("h9", Some("x".into())), None);

        // h1 got used: no longer wanted; h3 is the next address.
        let wanted: HashMap<String, AccountId> =
            [("h2".to_string(), id("b")), ("h3".to_string(), id("a"))].into();
        let (new, gone) = w.diff(&wanted);
        assert_eq!(new, vec!["h3".to_string()]);
        assert_eq!(gone, vec!["h1".to_string()]);
        assert_eq!(w.owner("h1"), None);
        assert_eq!(w.owner("h2"), Some(&id("b")));
        assert_eq!(w.update("h1", Some("s2".into())), None);
    }

    #[test]
    fn external_watches_are_planned_first_and_told() {
        use std::sync::Mutex;
        let extras: Extras = Default::default();
        let heard: Arc<Mutex<Vec<Activity>>> = Default::default();
        let h = heard.clone();
        let (tx, _rx) = mpsc::unbounded_channel();
        let watcher = Watcher {
            tx,
            extras: extras.clone(),
        };
        // A Spending deposit address (taproot).
        let deposit = "bc1p5d7rjq7g6rdk2yhzks9smlaqtedr4dekq08ge8ztwac72sfr9rusxg3297".to_string();
        watcher.watch_external(
            id("spending"),
            deposit.clone(),
            Arc::new(move |a| h.lock().unwrap().push(a)),
        );
        let onchain = vec![(
            id("savings"),
            vec!["bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu".to_string()],
        )];
        let candidates = with_extras(&extras, onchain);
        // Even with room for one script only, the deposit address is it.
        let plan = plan_watch(&BTreeMap::new(), &candidates, 1, 0);
        assert_eq!(plan.addresses, vec![deposit.clone()]);
        let hashes = script_hashes(&plan);
        assert_eq!(hashes.values().collect::<Vec<_>>(), vec![&id("spending")]);

        // A payment to it reaches its owner; an on-chain account isn't one.
        assert!(tell_extra(&extras, &id("spending")));
        assert!(!tell_extra(&extras, &id("savings")));
        tell_extras(&extras, Activity::Block);
        assert_eq!(
            *heard.lock().unwrap(),
            vec![Activity::Payment, Activity::Block]
        );
        watcher.unwatch_external(&id("spending"));
        assert!(extras_list(&extras).is_empty());
    }

    #[test]
    fn electrum_watch_plan_uses_script_hashes_and_its_own_limit() {
        let addrs: Vec<String> = (0..3)
            .map(|_| "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu".to_string())
            .chain(["bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g".to_string()])
            .collect();
        let plan = plan_watch(
            &BTreeMap::new(),
            &[(id("a"), addrs)],
            MAX_SCRIPTS_ELECTRUM,
            0,
        );
        let hashes = script_hashes(&plan);
        // Duplicates collapse; each maps to its Electrum script hash.
        assert_eq!(hashes.len(), 2);
        let spk = bitcoin::Address::from_str("bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu")
            .unwrap()
            .assume_checked()
            .script_pubkey();
        assert_eq!(hashes[&el::script_hash(&spk)], id("a"));
        // The cap holds across accounts.
        let many: Vec<(AccountId, Vec<String>)> = (0..10)
            .map(|i| {
                (
                    id(&format!("acct{i}")),
                    (0..20)
                        .map(|j| {
                            let spk = bitcoin::ScriptBuf::new_p2wpkh(
                                &bitcoin::hashes::Hash::from_byte_array([(i * 20 + j) as u8; 20]),
                            );
                            bitcoin::Address::from_script(&spk, bitcoin::Network::Bitcoin)
                                .unwrap()
                                .to_string()
                        })
                        .collect(),
                )
            })
            .collect();
        let plan = plan_watch(&BTreeMap::new(), &many, MAX_SCRIPTS_ELECTRUM, 0);
        assert_eq!(plan.addresses.len(), MAX_SCRIPTS_ELECTRUM);
        assert_eq!(script_hashes(&plan).len(), MAX_SCRIPTS_ELECTRUM);
        // Round robin: every account got its first addresses watched.
        for i in 0..10 {
            assert!(plan.owner.values().any(|a| a.0 == format!("acct{i}")));
        }
    }
}

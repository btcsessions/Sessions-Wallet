//! The relay side: one long-lived WebSocket per relay (the union of every
//! active connection's relays), a `REQ` for kind-23194 requests addressed
//! to our wallet keys, info events published on connect and on change,
//! responses and notifications published to each connection's relays,
//! keepalive pings and reconnects with backoff.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde_json::{json, Value};
use sessions_backup::nostr::Event;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Instant};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::connection::Connection;
use crate::handler::{normalize_relay, ConnectionStore, Outcome, WalletService};
use crate::protocol::{NotificationType, Transaction};
use crate::wallet::NwcWallet;
use crate::{now, KIND_REQUEST};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const SUB_ID: &str = "nwc";
/// Requests handled at once (a payment can take a while).
const MAX_CONCURRENT_REQUESTS: usize = 16;
/// Events kept for a relay while it's unreachable.
const MAX_QUEUED: usize = 256;

#[derive(Clone)]
pub struct ServiceConfig {
    /// TLS for `wss://` relays. Default: rustls (ring) with the bundled web
    /// roots, as the rest of the app.
    pub tls: Arc<rustls::ClientConfig>,
    /// Ping a quiet relay this often.
    pub ping_interval: Duration,
    /// Reconnect when nothing (not even a pong) arrived for this long.
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
    pub min_backoff: Duration,
    pub max_backoff: Duration,
    /// Ask relays for requests up to this many seconds old on (re)connect,
    /// for relays that keep ephemeral events briefly. Duplicates are dropped.
    pub since_margin_secs: u64,
    /// Recheck connections (expiry) this often.
    pub refresh_interval: Duration,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        ServiceConfig {
            tls: default_tls(),
            ping_interval: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(75),
            connect_timeout: Duration::from_secs(15),
            min_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            since_margin_secs: 60,
            refresh_interval: Duration::from_secs(60),
        }
    }
}

fn default_tls() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| sessions_backup::relay::tls_with_roots(sessions_backup::relay::web_roots()))
        .clone()
}

/// What a relay connection is doing (for the UI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayStatus {
    Connecting,
    Connected,
    /// Waiting to retry after `error`.
    Disconnected {
        error: String,
    },
}

type Statuses = Arc<Mutex<HashMap<String, RelayStatus>>>;

enum Command {
    /// Connections changed: resubscribe and republish info events.
    Refresh,
    Publish(Vec<(Vec<String>, Event)>),
    Incoming(Box<Event>),
}

enum RelayCmd {
    Update {
        pubkeys: Vec<String>,
        infos: Vec<Event>,
    },
    Publish(Event),
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The running service. Dropping it stops listening (requests already being
/// handled finish, so a payment is never cut off halfway).
pub struct ServiceHandle<W: NwcWallet> {
    service: Arc<WalletService<W>>,
    tx: mpsc::UnboundedSender<Command>,
    statuses: Statuses,
    _task: AbortOnDrop,
}

/// Start the service on the current Tokio runtime.
///
/// `connections` are the persisted ones; `store` gets every connection the
/// service changes (budget usage, last used) to persist again.
pub fn start<W: NwcWallet>(
    wallet: Arc<W>,
    connections: Vec<Connection>,
    store: Arc<dyn ConnectionStore>,
    config: ServiceConfig,
) -> ServiceHandle<W> {
    let service = Arc::new(WalletService::new(wallet, store));
    let rejected = service.set_connections(connections);
    if !rejected.is_empty() {
        tracing::warn!(count = rejected.len(), "nwc: some connections are invalid");
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let statuses: Statuses = Arc::default();
    let sup = Supervisor {
        service: service.clone(),
        config,
        tx: tx.clone(),
        workers: HashMap::new(),
        statuses: statuses.clone(),
        permits: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
        last_subs: HashMap::new(),
    };
    let task = tokio::spawn(sup.run(rx));
    let _ = tx.send(Command::Refresh);
    ServiceHandle {
        service,
        tx,
        statuses,
        _task: AbortOnDrop(task),
    }
}

impl<W: NwcWallet> ServiceHandle<W> {
    /// Replace the connections (after an add, edit or remove). Returns the
    /// ids skipped as invalid. See [`WalletService::set_connections`].
    pub fn set_connections(&self, connections: Vec<Connection>) -> Vec<String> {
        let rejected = self.service.set_connections(connections);
        let _ = self.tx.send(Command::Refresh);
        rejected
    }

    pub fn connections(&self) -> Vec<Connection> {
        self.service.connections()
    }

    pub fn reset_usage(&self, id: &str) -> Option<Connection> {
        self.service.reset_usage(id)
    }

    /// Tell connections with the notifications permission about a payment
    /// the wallet received (any payment, however it arrived).
    pub fn notify_payment_received(&self, tx: &Transaction) {
        self.notify(NotificationType::PaymentReceived, tx)
    }

    /// Same, for a payment the wallet sent.
    pub fn notify_payment_sent(&self, tx: &Transaction) {
        self.notify(NotificationType::PaymentSent, tx)
    }

    pub fn notify(&self, kind: NotificationType, tx: &Transaction) {
        let events = self.service.notification_events(kind, tx, now());
        if !events.is_empty() {
            let _ = self.tx.send(Command::Publish(events));
        }
    }

    /// Relay URL → what its connection is doing.
    pub fn relay_status(&self) -> Vec<(String, RelayStatus)> {
        let mut v: Vec<_> = self
            .statuses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// The request handler, e.g. to read connections.
    pub fn service(&self) -> &Arc<WalletService<W>> {
        &self.service
    }

    /// Stop now (same as dropping).
    pub fn stop(self) {}
}

struct Worker {
    tx: mpsc::Sender<RelayCmd>,
    _task: AbortOnDrop,
}

struct Supervisor<W: NwcWallet> {
    service: Arc<WalletService<W>>,
    config: ServiceConfig,
    tx: mpsc::UnboundedSender<Command>,
    workers: HashMap<String, Worker>,
    statuses: Statuses,
    permits: Arc<Semaphore>,
    last_subs: HashMap<String, Vec<String>>,
}

impl<W: NwcWallet> Supervisor<W> {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Command>) {
        let mut tick = tokio::time::interval(self.config.refresh_interval);
        tick.tick().await;
        loop {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    None => break,
                    Some(Command::Refresh) => self.refresh(true),
                    Some(Command::Publish(events)) => self.publish(events),
                    Some(Command::Incoming(ev)) => self.incoming(*ev),
                },
                _ = tick.tick() => self.refresh(false),
            }
        }
    }

    /// Bring workers in line with the connections. `force`: republish info
    /// events and resend filters even if nothing changed.
    fn refresh(&mut self, force: bool) {
        let now = now();
        let subs = self.service.subscriptions(now);
        if !force && subs == self.last_subs {
            return;
        }
        self.workers.retain(|url, _| subs.contains_key(url));
        {
            let mut st = self.statuses.lock().unwrap_or_else(|e| e.into_inner());
            st.retain(|url, _| subs.contains_key(url));
        }
        let mut infos: HashMap<String, Vec<Event>> = HashMap::new();
        for (relays, ev) in self.service.info_events(now) {
            for r in relays {
                infos
                    .entry(normalize_relay(&r))
                    .or_default()
                    .push(ev.clone());
            }
        }
        for (url, pubkeys) in &subs {
            let worker = self.workers.entry(url.clone()).or_insert_with(|| {
                let (tx, rx) = mpsc::channel(MAX_QUEUED);
                let task = tokio::spawn(relay_worker(
                    url.clone(),
                    self.config.clone(),
                    rx,
                    self.tx.clone(),
                    self.statuses.clone(),
                ));
                Worker {
                    tx,
                    _task: AbortOnDrop(task),
                }
            });
            let cmd = RelayCmd::Update {
                pubkeys: pubkeys.clone(),
                infos: infos.remove(url).unwrap_or_default(),
            };
            if worker.tx.try_send(cmd).is_err() {
                tracing::warn!(relay = %url, "nwc: relay queue full");
            }
        }
        self.last_subs = subs;
    }

    fn publish(&mut self, events: Vec<(Vec<String>, Event)>) {
        for (relays, ev) in events {
            for r in relays {
                if let Some(w) = self.workers.get(&normalize_relay(&r)) {
                    if w.tx.try_send(RelayCmd::Publish(ev.clone())).is_err() {
                        tracing::warn!(relay = %r, "nwc: relay queue full, dropping event");
                    }
                }
            }
        }
    }

    fn incoming(&mut self, ev: Event) {
        let service = self.service.clone();
        let tx = self.tx.clone();
        let permits = self.permits.clone();
        // Not aborted on stop: a payment in progress runs to completion.
        tokio::spawn(async move {
            let Ok(_permit) = permits.acquire_owned().await else {
                return;
            };
            if let Outcome::Respond { relays, event } = service.handle_event(&ev).await {
                let _ = tx.send(Command::Publish(vec![(relays, event)]));
            }
        });
    }
}

fn set_status(statuses: &Statuses, url: &str, s: RelayStatus) {
    statuses
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(url.to_string(), s);
}

enum End {
    /// The supervisor dropped us.
    Stop,
    /// Reconnect after a pause.
    Retry(String),
}

struct WorkerState {
    pubkeys: Vec<String>,
    infos: Vec<Event>,
    queue: VecDeque<Event>,
}

impl WorkerState {
    fn apply(&mut self, cmd: RelayCmd) {
        match cmd {
            RelayCmd::Update { pubkeys, infos } => {
                self.pubkeys = pubkeys;
                self.infos = infos;
            }
            RelayCmd::Publish(ev) => {
                if self.queue.len() >= MAX_QUEUED {
                    self.queue.pop_front();
                }
                self.queue.push_back(ev);
            }
        }
    }
}

async fn relay_worker(
    url: String,
    config: ServiceConfig,
    mut rx: mpsc::Receiver<RelayCmd>,
    out: mpsc::UnboundedSender<Command>,
    statuses: Statuses,
) {
    let mut st = WorkerState {
        pubkeys: Vec::new(),
        infos: Vec::new(),
        queue: VecDeque::new(),
    };
    let mut backoff = config.min_backoff;
    loop {
        set_status(&statuses, &url, RelayStatus::Connecting);
        let error = match connect(&url, &config).await {
            Ok(ws) => {
                tracing::info!(relay = %url, "nwc: connected");
                set_status(&statuses, &url, RelayStatus::Connected);
                let started = Instant::now();
                match session(ws, &url, &config, &mut st, &mut rx, &out).await {
                    End::Stop => return,
                    End::Retry(e) => {
                        // A connection that held for a while resets the backoff.
                        if started.elapsed() > config.max_backoff {
                            backoff = config.min_backoff;
                        }
                        e
                    }
                }
            }
            Err(e) => e,
        };
        tracing::info!(relay = %url, "nwc: disconnected: {error}");
        set_status(&statuses, &url, RelayStatus::Disconnected { error });
        // Back off with jitter, still taking commands meanwhile.
        let jitter = rand::thread_rng().gen_range(0.8..1.2);
        let wait = backoff.mul_f64(jitter);
        backoff = (backoff * 2).min(config.max_backoff);
        let until = Instant::now() + wait;
        loop {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(c) => st.apply(c),
                    None => return,
                },
                _ = tokio::time::sleep_until(until) => break,
            }
        }
    }
}

async fn connect(url: &str, config: &ServiceConfig) -> Result<Ws, String> {
    if !url.starts_with("wss://") && !url.starts_with("ws://") {
        return Err(format!("not a relay address: {url}"));
    }
    // list_transactions answers can be large-ish; requests are small.
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(1 << 20))
        .max_frame_size(Some(1 << 20));
    let (ws, _) = timeout(
        config.connect_timeout,
        tokio_tungstenite::connect_async_tls_with_config(
            url,
            Some(ws_config),
            true,
            Some(Connector::Rustls(config.tls.clone())),
        ),
    )
    .await
    .map_err(|_| "timed out connecting".to_string())?
    .map_err(|e| e.to_string())?;
    Ok(ws)
}

fn req_message(pubkeys: &[String], since: u64) -> String {
    json!([
        "REQ",
        SUB_ID,
        { "kinds": [KIND_REQUEST], "#p": pubkeys, "since": since }
    ])
    .to_string()
}

async fn send_event(ws: &mut Ws, ev: &Event) -> Result<(), String> {
    ws.send(Message::text(json!(["EVENT", ev]).to_string()))
        .await
        .map_err(|e| e.to_string())
}

async fn session(
    mut ws: Ws,
    url: &str,
    config: &ServiceConfig,
    st: &mut WorkerState,
    rx: &mut mpsc::Receiver<RelayCmd>,
    out: &mpsc::UnboundedSender<Command>,
) -> End {
    let r = async {
        let since = now().saturating_sub(config.since_margin_secs);
        if !st.pubkeys.is_empty() {
            ws.send(Message::text(req_message(&st.pubkeys, since)))
                .await
                .map_err(|e| e.to_string())?;
        }
        for ev in st.infos.clone() {
            send_event(&mut ws, &ev).await?;
        }
        while let Some(ev) = st.queue.pop_front() {
            send_event(&mut ws, &ev).await?;
        }
        let mut ping = tokio::time::interval(config.ping_interval);
        ping.tick().await;
        let mut last_rx = Instant::now();
        loop {
            tokio::select! {
                msg = ws.next() => {
                    let msg = match msg {
                        None => return Err("connection closed".to_string()),
                        Some(Err(e)) => return Err(e.to_string()),
                        Some(Ok(m)) => m,
                    };
                    last_rx = Instant::now();
                    match msg {
                        Message::Text(t) => {
                            if let Some(e) = on_text(url, t.as_str(), out) {
                                return Err(e);
                            }
                        }
                        Message::Close(_) => return Err("closed by relay".to_string()),
                        _ => {}
                    }
                }
                cmd = rx.recv() => match cmd {
                    None => return Ok(()),
                    Some(RelayCmd::Update { pubkeys, infos }) => {
                        if pubkeys != st.pubkeys && !pubkeys.is_empty() {
                            let since = now().saturating_sub(config.since_margin_secs);
                            ws.send(Message::text(req_message(&pubkeys, since)))
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                        st.pubkeys = pubkeys;
                        for ev in &infos {
                            send_event(&mut ws, ev).await?;
                        }
                        st.infos = infos;
                    }
                    Some(RelayCmd::Publish(ev)) => {
                        if let Err(e) = send_event(&mut ws, &ev).await {
                            st.queue.push_back(ev);
                            return Err(e);
                        }
                    }
                },
                _ = ping.tick() => {
                    if last_rx.elapsed() > config.idle_timeout {
                        return Err("no answer to pings".to_string());
                    }
                    ws.send(Message::Ping(Vec::new().into()))
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
        }
    }
    .await;
    let _ = timeout(Duration::from_secs(2), ws.close(None)).await;
    match r {
        Ok(()) => End::Stop,
        Err(e) => End::Retry(e),
    }
}

/// Handle one relay message. `Some(error)`: reconnect.
fn on_text(url: &str, text: &str, out: &mpsc::UnboundedSender<Command>) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    match v.get(0).and_then(Value::as_str) {
        Some("EVENT") if v.get(1).and_then(Value::as_str) == Some(SUB_ID) => {
            if let Some(ev) = v
                .get(2)
                .and_then(|e| serde_json::from_value::<Event>(e.clone()).ok())
            {
                if ev.kind == KIND_REQUEST {
                    let _ = out.send(Command::Incoming(Box::new(ev)));
                }
            }
        }
        Some("OK") => {
            if v.get(2).and_then(Value::as_bool) == Some(false) {
                let why = v.get(3).and_then(Value::as_str).unwrap_or("");
                if !why.starts_with("duplicate") {
                    tracing::info!(relay = %url, "nwc: relay refused an event: {why}");
                }
            }
        }
        Some("CLOSED") if v.get(1).and_then(Value::as_str) == Some(SUB_ID) => {
            let why = v.get(2).and_then(Value::as_str).unwrap_or("");
            return Some(format!("subscription closed: {why}"));
        }
        Some("NOTICE") => tracing::debug!(relay = %url, "nwc: notice: {v}"),
        _ => {}
    }
    None
}

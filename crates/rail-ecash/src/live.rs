//! The mint's websocket (NUT-17): instead of the wallet asking the mint over
//! and over, the mint says when this device's ecash is spent (by another
//! linked device) and when a quote is paid.
//!
//! One connection per account, opened when the account opens and kept up
//! while it's open. When it drops, it reconnects after 30 s, doubling to 10
//! min, and never polls in between: the sync's slow checks cover the gap.
//! Mints without NUT-17 are left to those checks.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use cdk::nuts::nut17::WsCommand;
use cdk::nuts::{PublicKey, State};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::Connector;

use crate::repair::now_secs;
use crate::EcashAccount;

/// Wait this long before reconnecting, doubling up to [`MAX_BACKOFF`].
const MIN_BACKOFF: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(10 * 60);
/// A connection that stayed up this long resets the wait.
const STABLE: Duration = Duration::from_secs(2 * 60);
/// Look for new ecash and quotes to watch this often (local, no requests).
const REFRESH: Duration = Duration::from_secs(20);
/// Ping this often; a connection silent for [`SILENT`] is dead.
const PING: Duration = Duration::from_secs(60);
const SILENT: Duration = Duration::from_secs(150);
/// Filters per subscription.
const CHUNK: usize = 100;

/// What the websocket knows, shared with the account.
#[derive(Debug, Default)]
pub(crate) struct Live {
    started: AtomicBool,
    connected: AtomicBool,
    /// Every proof this device holds is subscribed (so spent ones are
    /// reported as they happen).
    covering: AtomicBool,
    /// A token this device handed out was spent (the sync checks sends).
    pub(crate) spent_seen: AtomicBool,
    /// Quotes the mint said are paid (true once asked about since).
    paid: Mutex<HashMap<String, bool>>,
    /// Quotes to watch besides the unissued ones (a private payment's).
    watch: Mutex<HashSet<String>>,
    /// Something to watch changed.
    wake: tokio::sync::Notify,
    /// Tests: connect here instead of the mint's address.
    url_override: Mutex<Option<String>>,
}

impl Live {
    pub(crate) fn connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub(crate) fn covering(&self) -> bool {
        self.connected() && self.covering.load(Ordering::SeqCst)
    }

    pub(crate) fn paid(&self, quote: &str) -> bool {
        self.paid.lock().unwrap_or_else(|e| e.into_inner()).contains_key(quote)
    }

    /// The mint said `quote` is paid and nobody has asked about it since
    /// (and now someone is).
    pub(crate) fn take_fresh(&self, quote: &str) -> bool {
        let mut paid = self.paid.lock().unwrap_or_else(|e| e.into_inner());
        match paid.get_mut(quote) {
            Some(asked) if !*asked => {
                *asked = true;
                true
            }
            _ => false,
        }
    }

    /// Tests: as if the mint had said `quote` is paid.
    #[cfg(test)]
    pub(crate) fn mark_paid(&self, quote: &str) {
        self.paid.lock().unwrap().insert(quote.to_string(), false);
    }

    /// Done with `quote` (collected, or forgotten).
    pub(crate) fn forget(&self, quote: &str) {
        self.paid.lock().unwrap_or_else(|e| e.into_inner()).remove(quote);
        self.watch.lock().unwrap_or_else(|e| e.into_inner()).remove(quote);
    }

    fn down(&self) {
        self.connected.store(false, Ordering::SeqCst);
        self.covering.store(false, Ordering::SeqCst);
    }
}

impl EcashAccount {
    /// Hear about quote `id`'s payment over the websocket.
    pub(crate) fn watch_quote(&self, id: &str) {
        let new = self
            .live
            .watch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string());
        if new {
            self.live.wake.notify_one();
        }
    }

    /// Keep the mint's websocket open while this account is open. Call once
    /// it's in an `Arc` (again is harmless). Needs a tokio runtime.
    pub fn start_live(self: &Arc<Self>) {
        if tokio::runtime::Handle::try_current().is_err()
            || self.live.started.swap(true, Ordering::SeqCst)
        {
            return;
        }
        let url = self
            .live
            .url_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .or_else(|| ws_url(&self.wallet.mint_url.to_string()));
        let Some(url) = url else { return };
        let weak = Arc::downgrade(self);
        let live = self.live.clone();
        tokio::spawn(run(weak, live, url));
    }

    #[cfg(test)]
    pub(crate) fn set_ws_url(&self, url: &str) {
        *self.live.url_override.lock().unwrap() = Some(url.to_string());
    }

    /// Does the mint push proof states over a websocket? From its info,
    /// which CDK caches (no request when the cache is fresh).
    async fn mint_has_ws(&self) -> std::result::Result<bool, String> {
        let info = self.wallet.load_mint_info().await.map_err(|e| e.to_string())?;
        Ok(info
            .nuts
            .nut17
            .supported
            .iter()
            .any(|m| m.commands.contains(&WsCommand::ProofState)))
    }

    /// The proofs to watch: this device's ecash and the tokens it handed out.
    async fn watched_ys(&self) -> Vec<PublicKey> {
        self.wallet
            .get_proofs_by_states(vec![State::Unspent, State::PendingSpent])
            .await
            .map(|ps| ps.iter().filter_map(|p| p.y().ok()).collect())
            .unwrap_or_default()
    }

    /// The quotes to watch: unexpired, uncollected ones this device made,
    /// and any it was asked to watch.
    async fn watched_quotes(&self) -> Vec<String> {
        let now = now_secs();
        let mut out: HashSet<String> = self
            .wallet
            .get_unissued_mint_quotes()
            .await
            .map(|qs| {
                qs.into_iter()
                    .filter(|q| q.secret_key.is_some() && (q.expiry == 0 || q.expiry > now))
                    .map(|q| q.id)
                    .collect()
            })
            .unwrap_or_default();
        out.extend(self.live.watch.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned());
        out.into_iter().collect()
    }

    /// The mint says proof `y` is spent. Ecash this device holds as
    /// spendable was spent by another linked device: drop it. A token this
    /// device handed out was claimed: the sync marks it done.
    async fn spent_at_mint(&self, y: &str) {
        let Ok(y) = PublicKey::from_hex(y) else { return };
        let Ok(found) = self.wallet.localstore.get_proofs_by_ys(vec![y]).await else {
            return;
        };
        for p in found {
            match p.state {
                State::Unspent => {
                    match self.wallet.localstore.update_proofs(vec![], vec![y]).await {
                        Ok(()) => tracing::info!(
                            account = %self.id,
                            sats = p.proof.amount.to_u64(),
                            "ecash: dropped ecash spent on another device"
                        ),
                        Err(e) => tracing::warn!(account = %self.id, "ecash: drop spent ecash: {e}"),
                    }
                }
                State::PendingSpent => self.live.spent_seen.store(true, Ordering::SeqCst),
                // Reserved or pending: this device is spending it right now.
                _ => {}
            }
        }
    }

    /// One message from the mint.
    async fn on_message(&self, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return };
        if let Some(err) = v.get("error") {
            // A subscription it refused (too many?): the slow checks cover it.
            tracing::warn!(account = %self.id, "ecash: websocket: {err}");
            self.live.covering.store(false, Ordering::SeqCst);
            return;
        }
        let Some(p) = v.pointer("/params/payload") else { return };
        let s = |k: &str| p.get(k).and_then(Value::as_str);
        if let (Some(y), Some(state)) = (s("Y"), s("state")) {
            if state.eq_ignore_ascii_case("SPENT") {
                self.spent_at_mint(y).await;
            }
        } else if let Some(quote) = s("quote") {
            let n = |k: &str| p.get(k).and_then(Value::as_u64).unwrap_or(0);
            let paid = matches!(s("state").map(str::to_ascii_uppercase).as_deref(), Some("PAID" | "ISSUED"))
                || n("amount_paid") > n("amount_issued");
            if paid {
                self.live
                    .paid
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(quote.to_string(), false);
            }
        }
    }
}

/// `https://mint.example/path` → `wss://mint.example/path/v1/ws`.
fn ws_url(mint_url: &str) -> Option<String> {
    let base = mint_url.trim_end_matches('/');
    let rest = base
        .strip_prefix("https://")
        .map(|r| format!("wss://{r}"))
        .or_else(|| base.strip_prefix("http://").map(|r| format!("ws://{r}")))?;
    Some(format!("{rest}/v1/ws"))
}

fn tls() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            Arc::new(
                rustls::ClientConfig::builder_with_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_safe_default_protocol_versions()
                .expect("ring supports the default TLS versions")
                .with_root_certificates(rustls::RootCertStore {
                    roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
                })
                .with_no_client_auth(),
            )
        })
        .clone()
}

/// How a connection ended.
enum End {
    /// It was up, then closed or went quiet.
    Dropped,
    /// Couldn't connect: the reason.
    Failed(String),
    /// The mint doesn't do websockets after all, or the account closed.
    Stop,
}

async fn run(weak: Weak<EcashAccount>, live: Arc<Live>, url: String) {
    let mut backoff = MIN_BACKOFF;
    loop {
        // Only for mints that say they push proof states.
        let Some(acct) = weak.upgrade() else { return };
        match acct.mint_has_ws().await {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(account = %acct.id, "ecash: the mint has no websocket; checking now and then instead");
                return;
            }
            Err(e) => tracing::info!(account = %acct.id, "ecash: mint info: {e}"),
        }
        drop(acct);
        let started = Instant::now();
        let end = session(&weak, &live, &url).await;
        live.down();
        match end {
            End::Stop => return,
            End::Failed(e) => {
                let Some(acct) = weak.upgrade() else { return };
                tracing::info!(account = %acct.id, "ecash: websocket: {e}");
                acct.note_contact::<()>(&Err(sessions_core::Error::Network(e)));
            }
            End::Dropped => {}
        }
        if started.elapsed() > STABLE {
            backoff = MIN_BACKOFF;
        }
        // Wait, but stop as soon as the account closes.
        let until = Instant::now() + backoff;
        while Instant::now() < until {
            tokio::time::sleep(REFRESH.min(until - Instant::now())).await;
            if weak.strong_count() == 0 {
                return;
            }
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// One connection, kept up until it drops or the account closes.
async fn session(weak: &Weak<EcashAccount>, live: &Live, url: &str) -> End {
    let config = WebSocketConfig::default()
        .max_message_size(Some(1 << 20))
        .max_frame_size(Some(1 << 20));
    let connect = tokio_tungstenite::connect_async_tls_with_config(
        url,
        Some(config),
        false,
        Some(Connector::Rustls(tls())),
    );
    let mut ws = match tokio::time::timeout(Duration::from_secs(15), connect).await {
        Err(_) => return End::Failed("timed out connecting to the mint".into()),
        Ok(Err(tokio_tungstenite::tungstenite::Error::Http(r))) => {
            // It answered, but not as a websocket.
            if let Some(a) = weak.upgrade() {
                a.note_contact::<()>(&Ok(()));
                tracing::info!(account = %a.id, "ecash: the mint refused a websocket ({})", r.status());
            }
            return End::Stop;
        }
        Ok(Err(e)) => return End::Failed(e.to_string()),
        Ok(Ok((ws, _))) => ws,
    };
    {
        let Some(a) = weak.upgrade() else { return End::Stop };
        a.note_contact::<()>(&Ok(()));
        tracing::info!(account = %a.id, "ecash: websocket connected");
    }
    live.connected.store(true, Ordering::SeqCst);

    let mut ys: HashSet<PublicKey> = HashSet::new();
    let mut quotes: HashSet<String> = HashSet::new();
    let mut req = 0usize;
    let mut heard = Instant::now();
    let mut pinged = Instant::now();
    let mut tick = tokio::time::interval(REFRESH);
    loop {
        // Subscribe whatever's new (local reads only).
        {
            let Some(a) = weak.upgrade() else {
                let _ = ws.close(None).await;
                return End::Stop;
            };
            let mut msgs = Vec::new();
            let new_ys: Vec<String> = a
                .watched_ys()
                .await
                .into_iter()
                .filter(|y| ys.insert(*y))
                .map(|y| y.to_hex())
                .collect();
            for chunk in new_ys.chunks(CHUNK) {
                req += 1;
                msgs.push(subscribe(req, "proof_state", chunk));
            }
            let new_quotes: Vec<String> = a
                .watched_quotes()
                .await
                .into_iter()
                .filter(|q| quotes.insert(q.clone()))
                .collect();
            for chunk in new_quotes.chunks(CHUNK) {
                req += 1;
                msgs.push(subscribe(req, "bolt11_mint_quote", chunk));
            }
            drop(a);
            for m in msgs {
                if let Err(e) = ws.send(Message::text(m)).await {
                    tracing::info!("ecash: websocket send: {e}");
                    return End::Dropped;
                }
            }
            if !new_ys.is_empty() || !new_quotes.is_empty() {
                live.covering.store(true, Ordering::SeqCst);
            }
        }
        if pinged.elapsed() >= PING {
            pinged = Instant::now();
            if ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                return End::Dropped;
            }
        }
        if heard.elapsed() > SILENT {
            tracing::info!("ecash: websocket went quiet; reconnecting");
            return End::Dropped;
        }
        tokio::select! {
            m = ws.next() => match m {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return End::Dropped,
                Some(Ok(m)) => {
                    heard = Instant::now();
                    if let Message::Text(t) = m {
                        let Some(a) = weak.upgrade() else { return End::Stop };
                        a.on_message(t.as_str()).await;
                    }
                }
            },
            _ = tick.tick() => {}
            _ = live.wake.notified() => {}
        }
    }
}

/// A NUT-17 subscribe request.
fn subscribe(id: usize, kind: &str, filters: &[String]) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "subscribe",
        "params": { "kind": kind, "subId": format!("{kind}-{id}"), "filters": filters },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::{subscribe, ws_url};
    use cdk::nuts::nut17::ws::{NotificationInner, WsMethodRequest, WsNotification, WsRequest};
    use cdk::nuts::nut17::{Kind, NotificationPayload, Params};
    use cdk::nuts::{MintQuoteBolt11Response, MintQuoteState, ProofState, State};

    /// Our requests are what CDK's own client sends, and the mint's
    /// notifications (built with CDK's types, as cdk-mintd does) carry the
    /// fields we read.
    #[test]
    fn speaks_cdk_nut17() {
        let ours: serde_json::Value = serde_json::from_str(&subscribe(7, "proof_state", &["02ab".into()])).unwrap();
        let theirs: WsRequest<String> = (
            WsMethodRequest::Subscribe(Params {
                kind: Kind::ProofState,
                filters: vec!["02ab".into()],
                id: "proof_state-7".into(),
            }),
            7,
        )
            .into();
        assert_eq!(ours, serde_json::to_value(&theirs).unwrap());
        let q: serde_json::Value =
            serde_json::from_str(&subscribe(8, "bolt11_mint_quote", &["q1".into()])).unwrap();
        assert_eq!(q["params"]["kind"], serde_json::to_value(Kind::Bolt11MintQuote).unwrap());

        let y = cdk::dhke::hash_to_curve(b"y").unwrap();
        let spent = WsNotification {
            jsonrpc: "2.0".into(),
            method: "subscribe".into(),
            params: NotificationInner {
                sub_id: "s".to_string(),
                payload: NotificationPayload::<String>::ProofState(ProofState { y, state: State::Spent, witness: None }),
            },
        };
        let v = serde_json::to_value(&spent).unwrap();
        assert_eq!(v.pointer("/params/payload/Y").and_then(|x| x.as_str()), Some(y.to_hex().as_str()));
        assert_eq!(v.pointer("/params/payload/state").and_then(|x| x.as_str()), Some("SPENT"));

        let paid = WsNotification {
            jsonrpc: "2.0".into(),
            method: "subscribe".into(),
            params: NotificationInner {
                sub_id: "s".to_string(),
                payload: NotificationPayload::<String>::MintQuoteBolt11Response(MintQuoteBolt11Response {
                    quote: "q1".into(),
                    request: "lnbc1".into(),
                    amount: Some(30.into()),
                    unit: None,
                    method: cdk::nuts::PaymentMethod::BOLT11,
                    amount_paid: 30.into(),
                    amount_issued: 0.into(),
                    updated_at: 0,
                    state: MintQuoteState::Paid,
                    expiry: None,
                    pubkey: None,
                }),
            },
        };
        let v = serde_json::to_value(&paid).unwrap();
        assert_eq!(v.pointer("/params/payload/quote").and_then(|x| x.as_str()), Some("q1"));
        assert_eq!(v.pointer("/params/payload/state").and_then(|x| x.as_str()), Some("PAID"));
        assert_eq!(v.pointer("/params/payload/Y"), None);
    }

    #[test]
    fn websocket_address() {
        assert_eq!(
            ws_url("https://mint.minibits.cash/Bitcoin").as_deref(),
            Some("wss://mint.minibits.cash/Bitcoin/v1/ws")
        );
        assert_eq!(ws_url("http://localhost:3338/").as_deref(), Some("ws://localhost:3338/v1/ws"));
        assert_eq!(ws_url("ftp://x"), None);
    }
}


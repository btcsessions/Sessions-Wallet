//! A minimal Nostr relay client: publish events, fetch one author's events.
//! One short-lived WebSocket per relay per operation, all relays in parallel.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use sessions_core::{Error, Result};
use tokio::time::{timeout_at, Instant};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::nostr::Event;

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// How long one relay gets for a whole publish or fetch.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(12);

/// rustls with ring and the bundled web roots (as the rest of the app).
fn default_tls() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG.get_or_init(|| tls_with_roots(web_roots())).clone()
}

/// The bundled web roots (Mozilla's, via webpki-roots).
pub fn web_roots() -> rustls::RootCertStore {
    rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    }
}

/// A TLS config trusting exactly these roots.
pub fn tls_with_roots(roots: rustls::RootCertStore) -> Arc<rustls::ClientConfig> {
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
}

#[derive(Clone)]
pub struct Relays {
    tls: Arc<rustls::ClientConfig>,
    timeout: Duration,
}

impl Default for Relays {
    fn default() -> Self {
        Relays {
            tls: default_tls(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// What one relay said.
pub type PerRelay<T> = Vec<(String, Result<T>)>;

fn net(e: impl std::fmt::Display) -> Error {
    Error::rail("relay", e)
}

impl Relays {
    /// With a custom TLS config (tests behind an intercepting proxy).
    pub fn with_tls(tls: Arc<rustls::ClientConfig>) -> Self {
        Relays {
            tls,
            ..Relays::default()
        }
    }

    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    async fn connect(&self, url: &str, deadline: Instant) -> Result<Ws> {
        if !url.starts_with("wss://") && !url.starts_with("ws://") {
            return Err(Error::Invalid(format!("not a relay address: {url}")));
        }
        // Our events are at most ~45 KB; a fetch gets a few dozen of them.
        let config = WebSocketConfig::default()
            .max_message_size(Some(1 << 20))
            .max_frame_size(Some(1 << 20));
        let (ws, _) = timeout_at(
            deadline,
            tokio_tungstenite::connect_async_tls_with_config(
                url,
                Some(config),
                true,
                Some(Connector::Rustls(self.tls.clone())),
            ),
        )
        .await
        .map_err(|_| net("timed out connecting"))?
        .map_err(net)?;
        Ok(ws)
    }

    /// Send every event to every relay. Per relay: `Ok` once it accepted all
    /// of them, else the first refusal or failure.
    pub async fn publish(&self, urls: &[String], events: &[Event]) -> PerRelay<()> {
        let tasks = urls.iter().map(|url| async move {
            let r = self.publish_one(url, events).await;
            if let Err(e) = &r {
                tracing::info!(relay = %url, "backup: publish failed: {e}");
            }
            (url.clone(), r)
        });
        futures_util::future::join_all(tasks).await
    }

    async fn publish_one(&self, url: &str, events: &[Event]) -> Result<()> {
        let deadline = Instant::now() + self.timeout;
        let mut ws = self.connect(url, deadline).await?;
        let r = async {
            for e in events {
                let msg = serde_json::json!(["EVENT", e]).to_string();
                ws.send(Message::text(msg)).await.map_err(net)?;
            }
            let mut waiting: std::collections::HashSet<&str> =
                events.iter().map(|e| e.id.as_str()).collect();
            while !waiting.is_empty() {
                let v = next_json(&mut ws, deadline).await?;
                match v.get(0).and_then(Value::as_str) {
                    Some("OK") => {
                        let id = v.get(1).and_then(Value::as_str).unwrap_or("");
                        let ok = v.get(2).and_then(Value::as_bool).unwrap_or(false);
                        let why = v.get(3).and_then(Value::as_str).unwrap_or("");
                        if !waiting.remove(id) {
                            continue;
                        }
                        // "duplicate:" comes with true; some relays say false.
                        if !ok && !why.starts_with("duplicate") {
                            return Err(net(format!("refused: {why}")));
                        }
                    }
                    Some("NOTICE") => tracing::debug!(relay = %url, "notice: {v}"),
                    _ => {}
                }
            }
            Ok(())
        }
        .await;
        let _ = ws.close(None).await;
        r
    }

    /// Every event by `author` of `kind`, signature-checked, from each relay.
    /// `Ok` only when the relay said it sent everything (EOSE), or timed out
    /// after sending some: a relay that answered with nothing is a real
    /// "nothing here", a relay that didn't answer is an error.
    pub async fn fetch(&self, urls: &[String], author: &str, kind: u16) -> PerRelay<Vec<Event>> {
        let filter = serde_json::json!({ "kinds": [kind], "authors": [author], "limit": 100 });
        let tasks = urls.iter().map(|url| {
            let filter = filter.clone();
            async move {
                let r = self.fetch_one(url, filter).await.map(|events| {
                    events
                        .into_iter()
                        .filter(|e| e.pubkey == author && e.kind == kind && e.verify())
                        .collect::<Vec<_>>()
                });
                if let Err(e) = &r {
                    tracing::info!(relay = %url, "backup: fetch failed: {e}");
                }
                (url.clone(), r)
            }
        });
        futures_util::future::join_all(tasks).await
    }

    /// Every event matching `filter` (a NIP-01 filter object), signature-
    /// checked, from each relay.
    pub async fn query(&self, urls: &[String], filter: Value) -> PerRelay<Vec<Event>> {
        let tasks = urls.iter().map(|url| {
            let filter = filter.clone();
            async move {
                let r = self
                    .fetch_one(url, filter)
                    .await
                    .map(|events| events.into_iter().filter(Event::verify).collect::<Vec<_>>());
                (url.clone(), r)
            }
        });
        futures_util::future::join_all(tasks).await
    }

    async fn fetch_one(&self, url: &str, filter: Value) -> Result<Vec<Event>> {
        let deadline = Instant::now() + self.timeout;
        let mut ws = self.connect(url, deadline).await?;
        const SUB: &str = "sw-backup";
        let r = async {
            let req = serde_json::json!(["REQ", SUB, filter]).to_string();
            ws.send(Message::text(req)).await.map_err(net)?;
            let mut events = Vec::new();
            loop {
                let v = match next_json(&mut ws, deadline).await {
                    Ok(v) => v,
                    Err(_) if !events.is_empty() => return Ok(events),
                    Err(e) => return Err(e),
                };
                match v.get(0).and_then(Value::as_str) {
                    Some("EVENT") if v.get(1).and_then(Value::as_str) == Some(SUB) => {
                        if let Some(e) = v
                            .get(2)
                            .and_then(|e| serde_json::from_value(e.clone()).ok())
                        {
                            events.push(e);
                        }
                    }
                    Some("EOSE") => return Ok(events),
                    Some("CLOSED") => {
                        let why = v.get(2).and_then(Value::as_str).unwrap_or("");
                        return Err(net(format!("closed: {why}")));
                    }
                    _ => {}
                }
            }
        }
        .await;
        let close = serde_json::json!(["CLOSE", SUB]).to_string();
        let _ = ws.send(Message::text(close)).await;
        let _ = ws.close(None).await;
        r
    }
}

/// The next JSON text message (pings are answered by tungstenite).
async fn next_json(ws: &mut Ws, deadline: Instant) -> Result<Value> {
    loop {
        let msg = timeout_at(deadline, ws.next())
            .await
            .map_err(|_| net("timed out"))?
            .ok_or_else(|| net("connection closed"))?
            .map_err(net)?;
        match msg {
            Message::Text(t) => {
                if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                    return Ok(v);
                }
            }
            Message::Close(_) => return Err(net("connection closed")),
            _ => {}
        }
    }
}

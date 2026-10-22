//! Push notifications from an Electrum server: new blocks
//! (`blockchain.headers.subscribe`) and changes to watched scripts
//! (`blockchain.scripthash.subscribe`).
//!
//! A small async JSON-RPC client of its own (tokio + rustls), separate from
//! the blocking connection BDK uses for scans: notifications arrive the
//! moment the server sends them, without polling. Requests are pipelined
//! over one connection; answers are matched by id.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::electrum::{dev_proxy, ipv4_first, tls_config, ServerUrl, Servers, CONNECT_TIMEOUT};

/// Per request: no answer within this long and the connection is given up.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest line we accept (a busy address's history can be large).
const MAX_LINE: usize = 32 * 1024 * 1024;

/// Something the server pushed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    /// A new chain tip.
    Header { height: u32 },
    /// A watched script's history changed: its new status (`None`: no history).
    Script {
        hash: String,
        status: Option<String>,
    },
}

/// Parse a notification (`{"method": …, "params": […]}`).
pub fn parse_notification(msg: &Value) -> Option<Notification> {
    let params = msg.get("params")?.as_array()?;
    match msg.get("method")?.as_str()? {
        "blockchain.headers.subscribe" => {
            let height = params.first()?.get("height")?.as_u64()?;
            Some(Notification::Header {
                height: u32::try_from(height).ok()?,
            })
        }
        "blockchain.scripthash.subscribe" => Some(Notification::Script {
            hash: params.first()?.as_str()?.to_string(),
            status: params.get(1).and_then(Value::as_str).map(str::to_string),
        }),
        _ => None,
    }
}

/// A transaction in a script's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryItem {
    pub txid: String,
    /// Block height; 0 or -1 while unconfirmed.
    pub height: i64,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;
type Writer = tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>;

/// One connection to an Electrum server. Dropping it closes the connection.
pub struct PushConn {
    server: ServerUrl,
    writer: Writer,
    pending: Pending,
    next_id: AtomicU64,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for PushConn {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

async fn tcp(server: &ServerUrl) -> std::io::Result<tokio::net::TcpStream> {
    if let Some(proxy) = dev_proxy() {
        let mut s = tokio::net::TcpStream::connect(proxy.trim()).await?;
        let target = format!("{}:{}", server.host, server.port);
        s.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
            .await?;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            use tokio::io::AsyncReadExt;
            if s.read(&mut byte).await? == 0 || head.len() > 8192 {
                return Err(std::io::Error::other("proxy closed the connection"));
            }
            head.push(byte[0]);
        }
        let status = String::from_utf8_lossy(&head);
        if status.split_whitespace().nth(1) != Some("200") {
            return Err(std::io::Error::other("proxy refused the connection"));
        }
        return Ok(s);
    }
    let mut addrs: Vec<std::net::SocketAddr> =
        tokio::net::lookup_host((server.host.as_str(), server.port))
            .await?
            .collect();
    ipv4_first(&mut addrs);
    let mut last = std::io::Error::other("no address");
    for a in addrs {
        match tokio::net::TcpStream::connect(a).await {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

async fn connect_stream(server: &ServerUrl) -> std::io::Result<Box<dyn Io>> {
    let tcp = tcp(server).await?;
    tcp.set_nodelay(true)?;
    if !server.tls {
        return Ok(Box::new(tcp));
    }
    let name = rustls::pki_types::ServerName::try_from(server.host.clone())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let tls = tokio_rustls::TlsConnector::from(tls_config())
        .connect(name, tcp)
        .await?;
    Ok(Box::new(tls))
}

impl PushConn {
    /// Connect to one server and agree on the protocol version. Pushed
    /// notifications arrive on the returned channel, which closes when the
    /// connection does.
    pub async fn open(
        server: &ServerUrl,
    ) -> Result<(PushConn, mpsc::UnboundedReceiver<Notification>), String> {
        let io = tokio::time::timeout(CONNECT_TIMEOUT, connect_stream(server))
            .await
            .map_err(|_| "timed out connecting".to_string())?
            .map_err(|e| e.to_string())?;
        let (read, write) = tokio::io::split(io);
        let pending: Pending = Default::default();
        let (tx, rx) = mpsc::unbounded_channel();
        let reader = tokio::spawn(read_loop(BufReader::new(read), pending.clone(), tx));
        let conn = PushConn {
            server: server.clone(),
            writer: tokio::sync::Mutex::new(Box::new(write)),
            pending,
            next_id: AtomicU64::new(1),
            reader,
        };
        tokio::time::timeout(
            CONNECT_TIMEOUT,
            conn.call(
                "server.version",
                json!([
                    format!("SessionsWallet {}", env!("CARGO_PKG_VERSION")),
                    "1.4"
                ]),
            ),
        )
        .await
        .map_err(|_| "timed out agreeing on the protocol".to_string())??;
        Ok((conn, rx))
    }

    /// Try the servers in order (the current one first); the first that
    /// connects becomes current.
    pub async fn connect(
        servers: &Servers,
    ) -> Result<(PushConn, mpsc::UnboundedReceiver<Notification>), String> {
        let mut last = String::from("no servers");
        for index in servers.order().collect::<Vec<_>>() {
            let server = &servers.list()[index];
            match Self::open(server).await {
                Ok(c) => {
                    servers.worked(index);
                    return Ok(c);
                }
                Err(e) => {
                    tracing::info!("Electrum server {server}: {e}");
                    last = format!("{server}: {e}");
                }
            }
        }
        Err(last)
    }

    pub fn server(&self) -> &ServerUrl {
        &self.server
    }

    /// Send several requests at once (pipelined) and wait for every answer.
    pub async fn call_many(&self, calls: &[(&str, Value)]) -> Vec<Result<Value, String>> {
        let mut waits = Vec::with_capacity(calls.len());
        let mut out = Vec::new();
        for (method, params) in calls {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = oneshot::channel();
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, tx);
            let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            out.extend_from_slice(line.to_string().as_bytes());
            out.push(b'\n');
            waits.push((id, rx));
        }
        let sent = {
            let mut w = self.writer.lock().await;
            match tokio::time::timeout(REQUEST_TIMEOUT, async {
                w.write_all(&out).await?;
                w.flush().await
            })
            .await
            {
                Ok(r) => r.map_err(|e| e.to_string()),
                Err(_) => Err("timed out sending".to_string()),
            }
        };
        let mut results = Vec::with_capacity(waits.len());
        for (id, rx) in waits {
            let r = match &sent {
                Err(e) => Err(e.clone()),
                Ok(()) => match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
                    Ok(Ok(r)) => r,
                    Ok(Err(_)) => Err("connection closed".to_string()),
                    Err(_) => Err("timed out".to_string()),
                },
            };
            if r.is_err() {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
            }
            results.push(r);
        }
        results
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.call_many(&[(method, params)])
            .await
            .pop()
            .unwrap_or_else(|| Err("no answer".into()))
    }

    pub async fn ping(&self) -> Result<(), String> {
        self.call("server.ping", json!([])).await.map(|_| ())
    }

    /// Subscribe to new blocks; returns the current tip height.
    pub async fn subscribe_headers(&self) -> Result<u32, String> {
        let v = self.call("blockchain.headers.subscribe", json!([])).await?;
        v.get("height")
            .and_then(Value::as_u64)
            .and_then(|h| u32::try_from(h).ok())
            .ok_or_else(|| format!("unexpected header: {v}"))
    }

    /// Subscribe to scripts (by Electrum script hash); returns each one's
    /// current status (`None`: no history), in order.
    pub async fn subscribe_scripts(
        &self,
        hashes: &[String],
    ) -> Vec<Result<Option<String>, String>> {
        let calls: Vec<(&str, Value)> = hashes
            .iter()
            .map(|h| ("blockchain.scripthash.subscribe", json!([h])))
            .collect();
        self.call_many(&calls)
            .await
            .into_iter()
            .map(|r| r.map(|v| v.as_str().map(str::to_string)))
            .collect()
    }

    /// Stop watching scripts. Servers without `unsubscribe` (older electrs)
    /// keep sending updates for them; those are ignored.
    pub async fn unsubscribe_scripts(&self, hashes: &[String]) {
        let calls: Vec<(&str, Value)> = hashes
            .iter()
            .map(|h| ("blockchain.scripthash.unsubscribe", json!([h])))
            .collect();
        let _ = self.call_many(&calls).await;
    }

    /// A script's history (confirmed and in the mempool).
    pub async fn history(&self, hash: &str) -> Result<Vec<HistoryItem>, String> {
        let v = self
            .call("blockchain.scripthash.get_history", json!([hash]))
            .await?;
        let items = v
            .as_array()
            .ok_or_else(|| format!("unexpected history: {v}"))?;
        Ok(items
            .iter()
            .filter_map(|i| {
                Some(HistoryItem {
                    txid: i.get("tx_hash")?.as_str()?.to_string(),
                    height: i.get("height")?.as_i64()?,
                })
            })
            .collect())
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: BufReader<R>,
    pending: Pending,
    notify: mpsc::UnboundedSender<Notification>,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) if line.len() > MAX_LINE => break,
            Ok(_) => {}
        }
        let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        // A batch answer (we don't send batches, but be lenient).
        let msgs = match msg {
            Value::Array(a) => a,
            m => vec![m],
        };
        for msg in msgs {
            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                let waiter = pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                if let Some(w) = waiter {
                    let r = match msg.get("error").filter(|e| !e.is_null()) {
                        Some(e) => Err(e
                            .get("message")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| e.to_string())),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = w.send(r);
                }
            } else if let Some(n) = parse_notification(&msg) {
                if notify.send(n).is_err() {
                    break;
                }
            }
        }
    }
    // Connection gone: fail everything still waiting (dropping the senders).
    pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[test]
    fn parses_notifications() {
        assert_eq!(
            parse_notification(&json!({
                "jsonrpc": "2.0", "method": "blockchain.headers.subscribe",
                "params": [{"height": 972615, "hex": "00"}]
            })),
            Some(Notification::Header { height: 972615 })
        );
        assert_eq!(
            parse_notification(&json!({
                "method": "blockchain.scripthash.subscribe",
                "params": ["abcd", "ef01"]
            })),
            Some(Notification::Script {
                hash: "abcd".into(),
                status: Some("ef01".into())
            })
        );
        assert_eq!(
            parse_notification(&json!({
                "method": "blockchain.scripthash.subscribe",
                "params": ["abcd", null]
            })),
            Some(Notification::Script {
                hash: "abcd".into(),
                status: None
            })
        );
        assert_eq!(parse_notification(&json!({"id": 1, "result": 5})), None);
        assert_eq!(
            parse_notification(&json!({"method": "x", "params": []})),
            None
        );
    }

    /// The genesis block header, the only block the fake chain has.
    pub(crate) const GENESIS_HEADER: &str = "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c";

    /// A tiny fake Electrum server (plain TCP) on a chain of one block where
    /// every script is unused (except script hash "used", for the push
    /// tests). `push` sends lines to every connected client.
    pub(crate) struct FakeServer {
        pub url: ServerUrl,
        pub push: tokio::sync::broadcast::Sender<String>,
        /// Methods received, in order.
        pub seen: Arc<Mutex<Vec<String>>>,
        /// Connections accepted.
        pub connections: Arc<std::sync::atomic::AtomicUsize>,
    }

    pub(crate) async fn fake_server() -> FakeServer {
        fake_server_dropping(None).await
    }

    /// Like [`fake_server`], but the first connection is cut (no answer)
    /// on its `n`th request.
    pub(crate) async fn fake_server_dropping(drop_first_at: Option<usize>) -> FakeServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (push, _) = tokio::sync::broadcast::channel::<String>(16);
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (p, s, c) = (push.clone(), seen.clone(), connections.clone());
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                let nth = c.fetch_add(1, Ordering::SeqCst);
                let mut cut_at = if nth == 0 { drop_first_at } else { None };
                let mut sub = p.subscribe();
                let s = s.clone();
                tokio::spawn(async move {
                    let (r, mut w) = sock.into_split();
                    let mut lines = BufReader::new(r).lines();
                    loop {
                        tokio::select! {
                            line = lines.next_line() => {
                                let Ok(Some(line)) = line else { return };
                                let req: Value = serde_json::from_str(&line).unwrap();
                                let method = req["method"].as_str().unwrap().to_string();
                                s.lock().unwrap().push(method.clone());
                                if let Some(n) = cut_at.as_mut() {
                                    *n -= 1;
                                    if *n == 0 { return; }
                                }
                                let result = match method.as_str() {
                                    "server.version" => json!(["Fake 1.0", "1.4"]),
                                    "server.ping" => Value::Null,
                                    "blockchain.headers.subscribe" => json!({"height": 0, "hex": GENESIS_HEADER}),
                                    "blockchain.block.header" => json!(GENESIS_HEADER),
                                    "blockchain.block.headers" => json!({"count": 1, "hex": GENESIS_HEADER, "max": 2016}),
                                    "blockchain.estimatefee" => json!(0.0001),
                                    "blockchain.scripthash.subscribe" => {
                                        if req["params"][0] == "used" { json!("status-1") } else { Value::Null }
                                    }
                                    "blockchain.scripthash.get_history" => {
                                        if req["params"][0] == "used" {
                                            json!([
                                                {"tx_hash": "aa", "height": 0},
                                                {"tx_hash": "bb", "height": 99}
                                            ])
                                        } else {
                                            json!([])
                                        }
                                    }
                                    _ => {
                                        let e = json!({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32601, "message": "unknown method"}});
                                        let _ = w.write_all(format!("{e}\n").as_bytes()).await;
                                        continue;
                                    }
                                };
                                let resp = json!({"jsonrpc": "2.0", "id": req["id"], "result": result});
                                if w.write_all(format!("{resp}\n").as_bytes()).await.is_err() { return; }
                            }
                            msg = sub.recv() => {
                                let Ok(msg) = msg else { return };
                                if msg == "close" { return; }
                                if w.write_all(format!("{msg}\n").as_bytes()).await.is_err() { return; }
                            }
                        }
                    }
                });
            }
        });
        FakeServer {
            url: ServerUrl {
                host: "127.0.0.1".into(),
                port,
                tls: false,
            },
            push,
            seen,
            connections,
        }
    }

    #[tokio::test]
    async fn subscribes_and_receives_pushes() {
        let server = fake_server().await;
        let (conn, mut rx) = PushConn::open(&server.url).await.unwrap();
        assert_eq!(conn.subscribe_headers().await.unwrap(), 0);
        let st = conn
            .subscribe_scripts(&["fresh".into(), "used".into()])
            .await;
        assert_eq!(st, vec![Ok(None), Ok(Some("status-1".into()))]);
        conn.ping().await.unwrap();
        assert_eq!(
            conn.history("used").await.unwrap(),
            vec![
                HistoryItem {
                    txid: "aa".into(),
                    height: 0
                },
                HistoryItem {
                    txid: "bb".into(),
                    height: 99
                }
            ]
        );
        assert!(conn
            .call("nope", json!([]))
            .await
            .unwrap_err()
            .contains("unknown"));

        server
            .push
            .send(r#"{"jsonrpc":"2.0","method":"blockchain.scripthash.subscribe","params":["fresh","status-2"]}"#.into())
            .unwrap();
        server
            .push
            .send(r#"{"jsonrpc":"2.0","method":"blockchain.headers.subscribe","params":[{"height":101,"hex":"00"}]}"#.into())
            .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap();
        assert_eq!(
            first,
            Some(Notification::Script {
                hash: "fresh".into(),
                status: Some("status-2".into())
            })
        );
        let second = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap();
        assert_eq!(second, Some(Notification::Header { height: 101 }));

        // The server goes away: the channel closes and calls fail fast.
        server.push.send("close".into()).unwrap();
        let end = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap();
        assert_eq!(end, None);
        assert!(conn.ping().await.is_err());
        let seen = server.seen.lock().unwrap().clone();
        assert_eq!(seen[0], "server.version");
    }

    #[tokio::test]
    async fn connect_falls_back_to_the_next_server() {
        let server = fake_server().await;
        // Nothing listens on port 9 (discard) locally.
        let dead = ServerUrl {
            host: "127.0.0.1".into(),
            port: 9,
            tls: false,
        };
        let servers = Servers::new(vec![dead, server.url.clone()]).unwrap();
        let (conn, _rx) = PushConn::connect(&servers).await.unwrap();
        assert_eq!(conn.server(), &server.url);
        // Remembered: the working server is tried first next time.
        assert_eq!(servers.current(), 1);
    }

    /// Live check (manual): `cargo test -p rail-onchain live_push -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_push() {
        let servers = Servers::new(
            crate::electrum::default_servers(sessions_core::Network::Bitcoin).unwrap(),
        )
        .unwrap();
        let t = std::time::Instant::now();
        let (conn, _rx) = PushConn::connect(&servers).await.unwrap();
        println!(
            "connected to {} in {} ms",
            conn.server(),
            t.elapsed().as_millis()
        );
        println!("tip {}", conn.subscribe_headers().await.unwrap());
        // BIP84 "abandon … about" account 0, first receive address.
        let spk = bdk_wallet::bitcoin::ScriptBuf::from_hex(
            "0014c0cebcd6c3d3ca8c75dc5ec62ebe55330ef910e2",
        )
        .unwrap();
        let hash = crate::electrum::script_hash(&spk);
        println!(
            "status {:?}",
            conn.subscribe_scripts(std::slice::from_ref(&hash)).await
        );
        println!("history {}", conn.history(&hash).await.unwrap().len());
        let t = std::time::Instant::now();
        conn.ping().await.unwrap();
        println!("ping {} ms", t.elapsed().as_millis());
    }
}

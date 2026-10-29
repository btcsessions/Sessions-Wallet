//! Electrum servers: the default way on-chain accounts talk to the chain.
//!
//! * **One connection for the whole app.** Every account shares one
//!   [`Pool`] per server list (see [`pool_for`]), and BDK batches its
//!   requests over it. A first scan is a handful of round trips, not
//!   hundreds of HTTP requests.
//! * **Server fallback.** Servers are tried in order with a short connect
//!   timeout; the one that works is remembered ([`Servers`]). A server that
//!   keeps failing calls is swapped for the next one.
//! * **Retries per call.** A timeout or dropped connection only costs the
//!   call that hit it: the connection is dropped, a new one opened (on the
//!   next server if needed) and that call tried again. One transient error
//!   never aborts a whole scan.
//! * **Keepalive.** An idle connection sends `server.ping` every
//!   [`KEEPALIVE_EVERY`], so NAT boxes and phones don't silently drop it.
//! * **Blocking.** The Electrum client is blocking: callers run it on
//!   `spawn_blocking` (see [`crate::chain`]), never on async workers.
//!
//! TLS is rustls (ring) with bundled web roots, on our own TCP stream:
//! IPv4 addresses first (a common mobile failure is an IPv6 path that
//! connects but stalls), and certificates always verified.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

pub use bdk_electrum::electrum_client;
use bdk_electrum::BdkElectrumClient;
use bdk_wallet::bitcoin::{Script, Txid};
use electrum_client::{raw_client::RawClient, Batch, ElectrumApi, Error as EError, Param};
use electrum_client::{
    BroadcastPackageRes, EstimationMode, GetBalanceRes, GetHeadersRes, GetHistoryRes, GetMerkleRes,
    ListUnspentRes, MempoolInfoRes, RawHeaderNotification, ScriptStatus, ServerFeaturesRes,
    TxidFromPosRes,
};
use sessions_core::{Error, Result};
use std::borrow::Borrow;

/// Public mainnet servers, tried in this order. All up with 100% uptime on
/// the 1209k.com Electrum monitor (2026-10-22), all with CA-signed
/// certificates (BlueWallet's also checked live with `server.version`,
/// `blockchain.headers.subscribe` and a full scan):
///
/// * Blockstream (electrs): the server BDK's own examples use.
/// * Bull Bitcoin (Fulcrum).
/// * BlueWallet (ElectrumX) on port 443, which gets through networks that
///   block unusual ports. ElectrumX slows a session down once it has asked
///   for a lot (a big first scan), so it's not first.
/// * ACINQ (ElectrumX), the operator behind Phoenix.
pub const MAINNET_SERVERS: &[&str] = &[
    "ssl://electrum.blockstream.info:50002",
    "ssl://fulcrum.bullbitcoin.com:50002",
    "ssl://electrum2.bluewallet.io:443",
    "ssl://electrum.acinq.co:50002",
];

/// Per server, to open the connection (TCP + TLS + `server.version`).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// Per call: no answer within this long and the connection is dropped.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// Tries per call (each on a fresh connection after a failure).
const ATTEMPTS: usize = 3;
/// Failed calls in a row on one server before moving to the next.
const ROTATE_AFTER: u32 = 2;
/// An idle connection is pinged this often.
pub const KEEPALIVE_EVERY: Duration = Duration::from_secs(25);
/// Transactions fetched per batch when prefetching.
const PREFETCH_BATCH: usize = 50;

/// Development only: tunnel connections through an HTTP CONNECT proxy
/// (`host:port`), e.g. in a sandbox whose only way out is such a proxy.
const PROXY_ENV: &str = "SESSIONS_ELECTRUM_HTTP_PROXY";

fn err(e: impl std::fmt::Display) -> Error {
    Error::rail("electrum", e)
}

/// An Electrum server address.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerUrl {
    pub host: String,
    pub port: u16,
    /// TLS (`ssl://`), or plain TCP (`tcp://`, for your own node on your
    /// own network).
    pub tls: bool,
}

impl std::fmt::Display for ServerUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheme = if self.tls { "ssl" } else { "tcp" };
        if self.host.contains(':') {
            write!(f, "{scheme}://[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{scheme}://{}:{}", self.host, self.port)
        }
    }
}

impl ServerUrl {
    /// Accepts `ssl://host:port`, `tcp://host:port`, `host:port` (TLS),
    /// Electrum's own `host:port:s` / `host:port:t`, and a bare host
    /// (TLS on 50002). IPv6 hosts go in brackets. Onion addresses are
    /// refused for now (no Tor yet).
    pub fn parse(s: &str) -> Result<Self> {
        let bad = |why: &str| Error::Invalid(format!("That Electrum server address {why}"));
        let s = s.trim().trim_end_matches('/');
        if s.is_empty() {
            return Err(bad("is empty"));
        }
        let (scheme, rest) = match s.split_once("://") {
            Some((scheme, rest)) => (Some(scheme.to_ascii_lowercase()), rest),
            None => (None, s),
        };
        let mut tls = match scheme.as_deref() {
            None | Some("ssl") | Some("tls") => true,
            Some("tcp") => false,
            Some(_) => return Err(bad("should start with ssl:// or tcp://")),
        };
        // Electrum's "host:port:s" / "host:port:t" (only without a scheme).
        let mut rest = rest.to_string();
        if scheme.is_none() {
            for (suffix, t) in [(":s", true), (":t", false)] {
                if let Some(r) = rest.strip_suffix(suffix) {
                    if r.rsplit_once(':')
                        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
                    {
                        rest = r.to_string();
                        tls = t;
                    }
                }
            }
        }
        let (host, port) = if let Some(r) = rest.strip_prefix('[') {
            let (host, after) = r.split_once(']').ok_or_else(|| bad("is missing a ]"))?;
            let port = match after.strip_prefix(':') {
                Some(p) => Some(p),
                None if after.is_empty() => None,
                None => return Err(bad("isn't valid")),
            };
            (host.to_string(), port)
        } else {
            match rest.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), Some(p)),
                None => (rest.clone(), None),
            }
        };
        let port = match port {
            Some(p) => p.parse::<u16>().map_err(|_| bad("has an invalid port"))?,
            None if tls => 50002,
            None => 50001,
        };
        // No scheme: 50001 (and testnet's 60001) is plain TCP by convention,
        // which is what Umbrel and Start9 show for their Electrum servers.
        if scheme.is_none() && matches!(port, 50001 | 60001) && !s.ends_with(":s") {
            tls = false;
        }
        if port == 0 {
            return Err(bad("has an invalid port"));
        }
        let host = host.trim().to_ascii_lowercase();
        if host.is_empty()
            || host.contains('/')
            || host.contains(char::is_whitespace)
            || host.contains('@')
        {
            return Err(bad("isn't valid"));
        }
        if host.ends_with(".onion") {
            return Err(Error::Invalid(
                "Onion (Tor) Electrum servers aren't supported yet".into(),
            ));
        }
        Ok(ServerUrl { host, port, tls })
    }

    /// The same server with TLS switched (for "host:port" typed without a
    /// scheme, when the guess was wrong).
    pub fn other_transport(&self) -> Self {
        ServerUrl {
            tls: !self.tls,
            ..self.clone()
        }
    }
}

/// The servers to use, in order, and which one is working.
#[derive(Debug)]
pub struct Servers {
    list: Vec<ServerUrl>,
    current: AtomicUsize,
    /// Failed calls in a row on the current server.
    fails: AtomicU32,
}

impl Servers {
    pub fn new(list: Vec<ServerUrl>) -> Result<Arc<Self>> {
        if list.is_empty() {
            return Err(err("no Electrum servers"));
        }
        Ok(Arc::new(Servers {
            list,
            current: AtomicUsize::new(0),
            fails: AtomicU32::new(0),
        }))
    }

    pub fn list(&self) -> &[ServerUrl] {
        &self.list
    }

    /// Index of the server in use (or to try first).
    pub fn current(&self) -> usize {
        self.current.load(Ordering::SeqCst) % self.list.len()
    }

    /// Server indices to try, the current one first.
    pub fn order(&self) -> impl Iterator<Item = usize> + '_ {
        let start = self.current();
        (0..self.list.len()).map(move |i| (start + i) % self.list.len())
    }

    /// `index` worked: use it from now on.
    pub fn worked(&self, index: usize) {
        if self.current.swap(index, Ordering::SeqCst) % self.list.len() != index {
            tracing::info!("using Electrum server {}", self.list[index]);
        }
        self.fails.store(0, Ordering::SeqCst);
    }

    /// A call on `index` failed (timeout, dropped connection). After
    /// [`ROTATE_AFTER`] in a row, the next server becomes current. Returns
    /// whether it moved on.
    pub fn failed(&self, index: usize) -> bool {
        if self.current() != index {
            return false; // someone already moved on
        }
        if self.fails.fetch_add(1, Ordering::SeqCst) + 1 < ROTATE_AFTER {
            return false;
        }
        self.fails.store(0, Ordering::SeqCst);
        let next = (index + 1) % self.list.len();
        let moved = self
            .current
            .compare_exchange(index, next, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if moved && next != index {
            tracing::warn!(
                "Electrum server {} keeps failing; trying {}",
                self.list[index],
                self.list[next]
            );
        }
        moved
    }
}

/// A call failed because of the connection (worth retrying elsewhere), not
/// because the server answered with an error.
pub fn is_transport(e: &EError) -> bool {
    !matches!(
        e,
        EError::Protocol(_)
            | EError::AlreadySubscribed(_)
            | EError::NotSubscribed(_)
            | EError::Message(_)
            | EError::Bitcoin(_)
            | EError::Hex(_)
    )
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Plain TCP or TLS, behind one type so one [`RawClient`] type covers both.
pub enum Stream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// rustls with ring and the bundled web roots, shared by every connection.
pub(crate) fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

/// The development proxy, if set (see [`PROXY_ENV`]).
pub(crate) fn dev_proxy() -> Option<String> {
    std::env::var(PROXY_ENV)
        .ok()
        .filter(|p| !p.trim().is_empty())
}

/// Sort IPv4 addresses before IPv6 ones (stable otherwise).
pub(crate) fn ipv4_first(addrs: &mut [SocketAddr]) {
    addrs.sort_by_key(|a| !a.is_ipv4());
}

fn timed_out(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, what.to_string())
}

/// TCP to `server`, IPv4 first, all within `timeout`.
fn tcp_connect(server: &ServerUrl, timeout: Duration) -> std::io::Result<TcpStream> {
    let deadline = Instant::now() + timeout;
    let left = || deadline.saturating_duration_since(Instant::now());
    if let Some(proxy) = dev_proxy() {
        let mut s = TcpStream::connect(proxy.trim())?;
        s.set_read_timeout(Some(timeout))?;
        let target = format!("{}:{}", server.host, server.port);
        write!(s, "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n")?;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if s.read(&mut byte)? == 0 || head.len() > 8192 {
                return Err(std::io::Error::other("proxy closed the connection"));
            }
            head.push(byte[0]);
        }
        let status = String::from_utf8_lossy(&head);
        if status.split_whitespace().nth(1) != Some("200") {
            return Err(std::io::Error::other(format!(
                "proxy refused: {}",
                status.lines().next().unwrap_or("")
            )));
        }
        return Ok(s);
    }
    let mut addrs: Vec<SocketAddr> = match (server.host.as_str(), server.port).to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(e) if server.host.ends_with(".local") => {
            // The system can't find .local names here: ask on the network.
            let found = crate::mdns::resolve(&server.host, Duration::from_secs(2));
            if found.is_empty() {
                return Err(e);
            }
            found
                .into_iter()
                .map(|ip| SocketAddr::from((ip, server.port)))
                .collect()
        }
        Err(e) => return Err(e),
    };
    ipv4_first(&mut addrs);
    let mut last = timed_out("couldn't connect in time");
    for addr in addrs {
        let budget = left();
        if budget.is_zero() {
            break;
        }
        match TcpStream::connect_timeout(&addr, budget) {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// An open connection: the client, and a handle on its socket (to change
/// timeouts, or shut it down so a stuck read returns at once).
pub struct Opened {
    pub client: RawClient<Stream>,
    pub socket: TcpStream,
}

/// Open a connection to `server` and agree on the protocol version.
pub fn open(server: &ServerUrl) -> std::result::Result<Opened, EError> {
    let tcp = tcp_connect(server, CONNECT_TIMEOUT)?;
    tcp.set_nodelay(true)?;
    // The handshake and `server.version` have to fit the connect budget too.
    tcp.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    tcp.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    let socket = tcp.try_clone()?;
    let stream = if server.tls {
        let name = rustls::pki_types::ServerName::try_from(server.host.clone())
            .map_err(|e| EError::InvalidDNSNameError(e.to_string()))?;
        let conn = rustls::ClientConnection::new(tls_config(), name)
            .map_err(|e| EError::Message(format!("TLS: {e}")))?;
        let mut tls = rustls::StreamOwned::new(conn, tcp);
        // Handshake now, so a bad certificate is a connect error.
        while tls.conn.is_handshaking() {
            tls.conn.complete_io(&mut tls.sock)?;
        }
        Stream::Tls(Box::new(tls))
    } else {
        Stream::Plain(tcp)
    };
    let client = RawClient::from(stream);
    client.protocol_version()?;
    socket.set_read_timeout(Some(CALL_TIMEOUT))?;
    socket.set_write_timeout(Some(CALL_TIMEOUT))?;
    Ok(Opened { client, socket })
}

/// Connect to `server` on its own (not the shared pool) and read the chain
/// tip: the "Test connection" check. Returns the block height.
pub fn probe(server: &ServerUrl) -> std::result::Result<usize, EError> {
    let opened = open(server)?;
    let tip = opened.client.block_headers_subscribe()?;
    let _ = opened.socket.shutdown(std::net::Shutdown::Both);
    Ok(tip.height)
}

/// What went wrong connecting to `server`, in plain words.
pub fn explain(server: &ServerUrl, e: &EError) -> String {
    let host = &server.host;
    let port = server.port;
    let text = e.to_string();
    let lower = text.to_ascii_lowercase();
    if lower.contains("lookup") || lower.contains("resolve") || lower.contains("name or service") || lower.contains("nodename") {
        if host.ends_with(".local") {
            format!("Couldn't find {host} on this network. Check the name and that you're on the same Wi-Fi, or use its IP address (like 192.168.1.20).")
        } else {
            format!("Couldn't find {host}. Check the address.")
        }
    } else if lower.contains("refused") {
        format!("{host} is there, but nothing answered on port {port}. Check the port, and that its Electrum server app (Electrs or Fulcrum) is running.")
    } else if lower.contains("timed out") || lower.contains("timeout") || lower.contains("would block") {
        format!("No answer from {host}:{port}. Check you're on the same network (or VPN) as your node.")
    } else if lower.contains("tls") || lower.contains("certificate") || lower.contains("handshake") || lower.contains("corrupt message") {
        if server.tls {
            format!("{host}:{port} didn't accept an encrypted (SSL) connection: {text}")
        } else {
            format!("{host}:{port} expects an encrypted connection: {text}")
        }
    } else {
        format!("Couldn't connect to {host}:{port}: {text}")
    }
}

// ---------------------------------------------------------------------------
// The shared connection
// ---------------------------------------------------------------------------

struct Live {
    /// Which server (index into [`Servers::list`]).
    index: usize,
    client: Arc<RawClient<Stream>>,
    socket: TcpStream,
}

/// Transactions fetched ahead of BDK asking for them one by one.
#[derive(Default)]
struct Prefetch {
    /// Raw transactions fetched in a batch, waiting to be asked for.
    stash: HashMap<Txid, Vec<u8>>,
    /// Transactions BDK already has (cached or handed out): never prefetched.
    known: HashSet<Txid>,
}

/// One reconnecting Electrum connection over a list of servers. Implements
/// [`ElectrumApi`], so BDK uses it directly; every call is retried on a
/// fresh connection (and the next server, if this one keeps failing).
///
/// It also batches transaction downloads: BDK fetches each transaction of a
/// script's history with its own request, so after every history batch the
/// unknown transactions are fetched in one batch and handed out from there.
pub struct Conn {
    servers: Arc<Servers>,
    live: RwLock<Option<Live>>,
    /// One connect at a time.
    connecting: Mutex<()>,
    prefetch: Mutex<Prefetch>,
    last_used: Mutex<Instant>,
}

impl Conn {
    pub fn new(servers: Arc<Servers>) -> Self {
        Conn {
            servers,
            live: RwLock::new(None),
            connecting: Mutex::new(()),
            prefetch: Mutex::new(Prefetch::default()),
            last_used: Mutex::new(Instant::now()),
        }
    }

    pub fn servers(&self) -> &Arc<Servers> {
        &self.servers
    }

    /// The live connection, opening one if needed (servers in order,
    /// starting with the current one).
    fn client(&self) -> std::result::Result<(usize, Arc<RawClient<Stream>>), EError> {
        if let Some(l) = &*self.live.read().unwrap_or_else(|e| e.into_inner()) {
            return Ok((l.index, l.client.clone()));
        }
        let _one = self.connecting.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(l) = &*self.live.read().unwrap_or_else(|e| e.into_inner()) {
            return Ok((l.index, l.client.clone()));
        }
        let mut last = None;
        for index in self.servers.order() {
            let server = &self.servers.list[index];
            let started = Instant::now();
            match open(server) {
                Ok(o) => {
                    tracing::info!(
                        "connected to Electrum server {server} in {} ms",
                        started.elapsed().as_millis()
                    );
                    self.servers.worked(index);
                    let client = Arc::new(o.client);
                    *self.live.write().unwrap_or_else(|e| e.into_inner()) = Some(Live {
                        index,
                        client: client.clone(),
                        socket: o.socket,
                    });
                    return Ok((index, client));
                }
                Err(e) => {
                    tracing::warn!("Electrum server {server}: {e}");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| EError::Message("no Electrum servers".into())))
    }

    /// Drop `client`'s connection (if it's still the live one).
    fn drop_client(&self, client: &Arc<RawClient<Stream>>) {
        let mut live = self.live.write().unwrap_or_else(|e| e.into_inner());
        if live
            .as_ref()
            .is_some_and(|l| Arc::ptr_eq(&l.client, client))
        {
            if let Some(l) = live.take() {
                // Wake up any thread still blocked reading from it.
                let _ = l.socket.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    /// Close the connection (it reopens on the next call).
    pub fn disconnect(&self) {
        if let Some(l) = self.live.write().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = l.socket.shutdown(std::net::Shutdown::Both);
        }
    }

    /// The server in use, if connected.
    /// The chain tip through this connection (reconnecting if needed):
    /// whether the wallet can reach its server right now.
    pub fn tip_height(&self) -> std::result::Result<usize, EError> {
        Ok(self.block_headers_subscribe()?.height)
    }

    pub fn connected_to(&self) -> Option<ServerUrl> {
        self.live
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|l| self.servers.list[l.index].clone())
    }

    /// Run `f` on the connection, retrying on a fresh one after a
    /// connection problem. Errors the server answered with aren't retried.
    fn call<T>(
        &self,
        what: &str,
        f: impl Fn(&RawClient<Stream>) -> std::result::Result<T, EError>,
    ) -> std::result::Result<T, EError> {
        let mut last = None;
        for attempt in 1..=ATTEMPTS {
            let (index, client) = match self.client() {
                Ok(c) => c,
                Err(e) => {
                    // Every server failed to connect: no point hammering.
                    last = Some(e);
                    break;
                }
            };
            *self.last_used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
            match f(&client) {
                Ok(v) => {
                    self.servers.fails.store(0, Ordering::SeqCst);
                    return Ok(v);
                }
                Err(e) if !is_transport(&e) => return Err(e),
                Err(e) => {
                    tracing::warn!(
                        "Electrum {what} on {} failed (attempt {attempt}/{ATTEMPTS}): {e}",
                        self.servers.list[index]
                    );
                    self.drop_client(&client);
                    self.servers.failed(index);
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| EError::Message("no Electrum servers".into())))
    }

    /// The connection when it's been idle for [`KEEPALIVE_EVERY`]: ping it,
    /// and drop it if that fails (the next call reconnects).
    pub fn keepalive(&self) {
        let idle = self
            .last_used
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed();
        if idle < KEEPALIVE_EVERY {
            return;
        }
        let Some(client) = self
            .live
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|l| l.client.clone())
        else {
            return;
        };
        *self.last_used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        // Block headers BDK subscribed to pile up otherwise.
        while let Ok(Some(_)) = client.block_headers_pop_raw() {}
        if let Err(e) = client.ping() {
            tracing::info!("Electrum keepalive failed ({e}); reconnecting on next use");
            self.drop_client(&client);
        }
    }

    /// Transactions BDK already has: never prefetched.
    pub fn mark_known(&self, txids: impl IntoIterator<Item = Txid>) {
        self.prefetch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .known
            .extend(txids);
    }

    /// Fetch, in batches, the transactions in `histories` BDK doesn't have.
    fn prefetch(&self, histories: &[Vec<GetHistoryRes>]) {
        let wanted: Vec<Txid> = {
            let p = self.prefetch.lock().unwrap_or_else(|e| e.into_inner());
            let mut seen = HashSet::new();
            histories
                .iter()
                .flatten()
                .map(|h| h.tx_hash)
                .filter(|t| !p.known.contains(t) && !p.stash.contains_key(t) && seen.insert(*t))
                .collect()
        };
        for chunk in wanted.chunks(PREFETCH_BATCH) {
            match self.call("transaction prefetch", |c| {
                c.batch_transaction_get_raw(chunk.iter())
            }) {
                Ok(raws) => {
                    let mut p = self.prefetch.lock().unwrap_or_else(|e| e.into_inner());
                    if p.stash.len() > 5_000 {
                        p.stash.clear(); // never asked for: don't grow forever
                    }
                    p.stash.extend(chunk.iter().copied().zip(raws));
                }
                // BDK fetches them one by one instead.
                Err(e) => {
                    tracing::debug!("Electrum prefetch: {e}");
                    return;
                }
            }
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.disconnect();
    }
}

impl ElectrumApi for Conn {
    fn raw_call(
        &self,
        method_name: &str,
        params: impl IntoIterator<Item = Param>,
    ) -> std::result::Result<serde_json::Value, EError> {
        let params: Vec<Param> = params.into_iter().collect();
        self.call(method_name, |c| c.raw_call(method_name, params.clone()))
    }

    fn batch_call(&self, batch: &Batch) -> std::result::Result<Vec<serde_json::Value>, EError> {
        self.call("batch", |c| c.batch_call(batch))
    }

    fn block_headers_subscribe_raw(&self) -> std::result::Result<RawHeaderNotification, EError> {
        self.call("headers.subscribe", |c| c.block_headers_subscribe_raw())
    }

    fn block_headers_pop_raw(&self) -> std::result::Result<Option<RawHeaderNotification>, EError> {
        match &*self.live.read().unwrap_or_else(|e| e.into_inner()) {
            Some(l) => l.client.block_headers_pop_raw(),
            None => Ok(None),
        }
    }

    fn block_header_raw(&self, height: usize) -> std::result::Result<Vec<u8>, EError> {
        self.call("block.header", |c| c.block_header_raw(height))
    }

    fn block_headers(
        &self,
        start_height: usize,
        count: usize,
    ) -> std::result::Result<GetHeadersRes, EError> {
        self.call("block.headers", |c| c.block_headers(start_height, count))
    }

    fn estimate_fee(
        &self,
        number: usize,
        mode: Option<EstimationMode>,
    ) -> std::result::Result<f64, EError> {
        self.call("estimatefee", |c| c.estimate_fee(number, mode))
    }

    fn relay_fee(&self) -> std::result::Result<f64, EError> {
        self.call("relayfee", |c| c.relay_fee())
    }

    fn script_subscribe(
        &self,
        script: &Script,
    ) -> std::result::Result<Option<ScriptStatus>, EError> {
        self.call("scripthash.subscribe", |c| c.script_subscribe(script))
    }

    fn batch_script_subscribe<'s, I>(
        &self,
        scripts: I,
    ) -> std::result::Result<Vec<Option<ScriptStatus>>, EError>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        let v: Vec<&Script> = scripts.into_iter().map(|s| *s.borrow()).collect();
        self.call("scripthash.subscribe", |c| {
            c.batch_script_subscribe(v.iter().copied())
        })
    }

    fn script_unsubscribe(&self, script: &Script) -> std::result::Result<bool, EError> {
        self.call("scripthash.unsubscribe", |c| c.script_unsubscribe(script))
    }

    fn script_pop(&self, script: &Script) -> std::result::Result<Option<ScriptStatus>, EError> {
        match &*self.live.read().unwrap_or_else(|e| e.into_inner()) {
            Some(l) => l.client.script_pop(script),
            None => Ok(None),
        }
    }

    fn script_get_balance(&self, script: &Script) -> std::result::Result<GetBalanceRes, EError> {
        self.call("get_balance", |c| c.script_get_balance(script))
    }

    fn batch_script_get_balance<'s, I>(
        &self,
        scripts: I,
    ) -> std::result::Result<Vec<GetBalanceRes>, EError>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        let v: Vec<&Script> = scripts.into_iter().map(|s| *s.borrow()).collect();
        self.call("get_balance", |c| {
            c.batch_script_get_balance(v.iter().copied())
        })
    }

    fn script_get_history(
        &self,
        script: &Script,
    ) -> std::result::Result<Vec<GetHistoryRes>, EError> {
        self.call("get_history", |c| c.script_get_history(script))
    }

    fn batch_script_get_history<'s, I>(
        &self,
        scripts: I,
    ) -> std::result::Result<Vec<Vec<GetHistoryRes>>, EError>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        let v: Vec<&Script> = scripts.into_iter().map(|s| *s.borrow()).collect();
        let histories = self.call("get_history", |c| {
            c.batch_script_get_history(v.iter().copied())
        })?;
        self.prefetch(&histories);
        Ok(histories)
    }

    fn script_list_unspent(
        &self,
        script: &Script,
    ) -> std::result::Result<Vec<ListUnspentRes>, EError> {
        self.call("listunspent", |c| c.script_list_unspent(script))
    }

    fn batch_script_list_unspent<'s, I>(
        &self,
        scripts: I,
    ) -> std::result::Result<Vec<Vec<ListUnspentRes>>, EError>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        let v: Vec<&Script> = scripts.into_iter().map(|s| *s.borrow()).collect();
        self.call("listunspent", |c| {
            c.batch_script_list_unspent(v.iter().copied())
        })
    }

    fn transaction_get_raw(&self, txid: &Txid) -> std::result::Result<Vec<u8>, EError> {
        let stashed = {
            let mut p = self.prefetch.lock().unwrap_or_else(|e| e.into_inner());
            p.known.insert(*txid);
            p.stash.remove(txid)
        };
        match stashed {
            Some(raw) => Ok(raw),
            None => self.call("transaction.get", |c| c.transaction_get_raw(txid)),
        }
    }

    fn batch_transaction_get_raw<'t, I>(
        &self,
        txids: I,
    ) -> std::result::Result<Vec<Vec<u8>>, EError>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'t Txid>,
    {
        let v: Vec<&Txid> = txids.into_iter().map(|t| *t.borrow()).collect();
        self.call("transaction.get", |c| {
            c.batch_transaction_get_raw(v.iter().copied())
        })
    }

    fn batch_block_header_raw<I>(&self, heights: I) -> std::result::Result<Vec<Vec<u8>>, EError>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<u32>,
    {
        let v: Vec<u32> = heights.into_iter().map(|h| *h.borrow()).collect();
        self.call("block.header", |c| {
            c.batch_block_header_raw(v.iter().copied())
        })
    }

    fn batch_estimate_fee<I>(&self, numbers: I) -> std::result::Result<Vec<f64>, EError>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<usize>,
    {
        let v: Vec<usize> = numbers.into_iter().map(|n| *n.borrow()).collect();
        self.call("estimatefee", |c| c.batch_estimate_fee(v.iter().copied()))
    }

    fn transaction_broadcast_raw(&self, raw_tx: &[u8]) -> std::result::Result<Txid, EError> {
        self.call("broadcast", |c| c.transaction_broadcast_raw(raw_tx))
    }

    fn transaction_broadcast_package_raw<T: AsRef<[u8]>>(
        &self,
        raw_txs: &[T],
    ) -> std::result::Result<BroadcastPackageRes, EError> {
        self.call("broadcast_package", |c| {
            c.transaction_broadcast_package_raw(raw_txs)
        })
    }

    fn transaction_get_merkle(
        &self,
        txid: &Txid,
        height: usize,
    ) -> std::result::Result<GetMerkleRes, EError> {
        self.call("get_merkle", |c| c.transaction_get_merkle(txid, height))
    }

    fn batch_transaction_get_merkle<I>(
        &self,
        txids_and_heights: I,
    ) -> std::result::Result<Vec<GetMerkleRes>, EError>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<(Txid, usize)>,
    {
        let v: Vec<(Txid, usize)> = txids_and_heights.into_iter().map(|t| *t.borrow()).collect();
        self.call("get_merkle", |c| c.batch_transaction_get_merkle(v.iter()))
    }

    fn txid_from_pos(&self, height: usize, tx_pos: usize) -> std::result::Result<Txid, EError> {
        self.call("id_from_pos", |c| c.txid_from_pos(height, tx_pos))
    }

    fn txid_from_pos_with_merkle(
        &self,
        height: usize,
        tx_pos: usize,
    ) -> std::result::Result<TxidFromPosRes, EError> {
        self.call("id_from_pos", |c| {
            c.txid_from_pos_with_merkle(height, tx_pos)
        })
    }

    fn server_features(&self) -> std::result::Result<ServerFeaturesRes, EError> {
        self.call("server.features", |c| c.server_features())
    }

    fn mempool_get_info(&self) -> std::result::Result<MempoolInfoRes, EError> {
        self.call("mempool.get_info", |c| c.mempool_get_info())
    }

    fn ping(&self) -> std::result::Result<(), EError> {
        self.call("ping", |c| c.ping())
    }

    fn calls_made(&self) -> std::result::Result<usize, EError> {
        match &*self.live.read().unwrap_or_else(|e| e.into_inner()) {
            Some(l) => l.client.calls_made(),
            None => Ok(0),
        }
    }
}

/// The app's connection to one list of servers, with BDK's caches on top
/// (transactions, headers, confirmations) and the request lanes.
pub struct Pool {
    pub client: BdkElectrumClient<Conn>,
    pub(crate) lanes: Arc<crate::net::Lanes>,
    key: String,
}

impl Pool {
    pub fn conn(&self) -> &Conn {
        &self.client.inner
    }

    pub fn servers(&self) -> &Arc<Servers> {
        self.client.inner.servers()
    }

    pub fn key(&self) -> &str {
        &self.key
    }
}

/// The shared pool for `servers` (one per distinct list), created on first
/// use. Lives while an account uses it; a keepalive thread pings it when
/// idle and stops once it's gone.
pub fn pool_for(servers: &[ServerUrl]) -> Result<Arc<Pool>> {
    static ALL: OnceLock<Mutex<HashMap<String, Weak<Pool>>>> = OnceLock::new();
    let key = format!(
        "electrum:{}",
        servers
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut all = ALL
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    all.retain(|_, p| p.strong_count() > 0);
    if let Some(p) = all.get(&key).and_then(Weak::upgrade) {
        return Ok(p);
    }
    let pool = Arc::new(Pool {
        client: BdkElectrumClient::new(Conn::new(Servers::new(servers.to_vec())?)),
        lanes: crate::net::lanes_for(&key),
        key: key.clone(),
    });
    let weak = Arc::downgrade(&pool);
    all.insert(key, weak.clone());
    let spawned = std::thread::Builder::new()
        .name("electrum-keepalive".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            match weak.upgrade() {
                Some(p) => p.conn().keepalive(),
                None => return,
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("no Electrum keepalive: {e}");
    }
    Ok(pool)
}

/// Default servers for `network`, or `None` where there are none (use Esplora).
pub fn default_servers(network: sessions_core::Network) -> Option<Vec<ServerUrl>> {
    match network {
        sessions_core::Network::Bitcoin => Some(
            MAINNET_SERVERS
                .iter()
                .map(|s| ServerUrl::parse(s).expect("valid built-in server"))
                .collect(),
        ),
        _ => None,
    }
}

/// Electrum's script hash: SHA256 of the script, byte-reversed, as hex.
pub fn script_hash(script: &Script) -> String {
    use bdk_wallet::bitcoin::hashes::{sha256, Hash};
    let mut h = sha256::Hash::hash(script.as_bytes()).to_byte_array();
    h.reverse();
    h.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> ServerUrl {
        ServerUrl::parse(s).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probe_reads_the_tip_and_explains_failures() {
        let server = crate::push::tests::fake_server().await;
        let url = server.url.clone();
        let height = tokio::task::spawn_blocking(move || probe(&url)).await.unwrap().unwrap();
        assert_eq!(height, 0);
        // Wrong transport: SSL to a plain port fails.
        let ssl = server.url.other_transport();
        let e = tokio::task::spawn_blocking(move || probe(&ssl).map(|_| ()))
            .await
            .unwrap()
            .unwrap_err();
        assert!(!explain(&server.url.other_transport(), &e).is_empty());
        // Nothing listening.
        let closed = ServerUrl::parse("tcp://127.0.0.1:1").unwrap();
        let c = closed.clone();
        let e = tokio::task::spawn_blocking(move || probe(&c).map(|_| ())).await.unwrap().unwrap_err();
        assert!(explain(&closed, &e).contains("nothing answered"), "{}", explain(&closed, &e));
    }

    #[test]
    fn parses_server_addresses() {
        let ssl = |h: &str, p| ServerUrl {
            host: h.into(),
            port: p,
            tls: true,
        };
        let tcp = |h: &str, p| ServerUrl {
            host: h.into(),
            port: p,
            tls: false,
        };
        assert_eq!(
            url("ssl://electrum.blockstream.info:50002"),
            ssl("electrum.blockstream.info", 50002)
        );
        assert_eq!(
            url(" tcp://umbrel.local:50001/ "),
            tcp("umbrel.local", 50001)
        );
        assert_eq!(
            url("Electrum.Example.com:443"),
            ssl("electrum.example.com", 443)
        );
        assert_eq!(url("example.com:50001:t"), tcp("example.com", 50001));
        // What Umbrel shows: host:50001 means plain TCP unless told otherwise.
        assert_eq!(url("umbrelhome.local:50001"), tcp("umbrelhome.local", 50001));
        assert_eq!(url("ssl://umbrel.local:50001"), ssl("umbrel.local", 50001));
        assert_eq!(url("umbrel.local:50001:s"), ssl("umbrel.local", 50001));
        assert_eq!(url("umbrel.local:50001").other_transport(), ssl("umbrel.local", 50001));
        assert_eq!(url("example.com:50002:s"), ssl("example.com", 50002));
        assert_eq!(url("example.com"), ssl("example.com", 50002));
        assert_eq!(url("tcp://192.168.1.10"), tcp("192.168.1.10", 50001));
        assert_eq!(url("tls://[2001:db8::1]:700"), ssl("2001:db8::1", 700));
        assert_eq!(url("[2001:db8::1]"), ssl("2001:db8::1", 50002));
        // Round trip.
        for s in [
            "ssl://electrum.blockstream.info:50002",
            "tcp://umbrel.local:50001",
            "ssl://[2001:db8::1]:700",
        ] {
            assert_eq!(url(s).to_string(), s);
        }
        for bad in [
            "",
            "   ",
            "https://example.com:50002",
            "example.com:port",
            "example.com:0",
            "example.com:70000",
            "ssl://user@example.com:50002",
            "ssl://example.com/path:50002",
            "[2001:db8::1",
        ] {
            assert!(ServerUrl::parse(bad).is_err(), "{bad:?} should be refused");
        }
        let onion = ServerUrl::parse("ssl://abcdefghijklmnop.onion:50002").unwrap_err();
        assert!(onion.to_string().contains("Onion"));
    }

    #[test]
    fn built_in_servers_parse() {
        let list = default_servers(sessions_core::Network::Bitcoin).unwrap();
        assert_eq!(list.len(), MAINNET_SERVERS.len());
        assert!(list.iter().all(|s| s.tls));
        assert!(default_servers(sessions_core::Network::Testnet).is_none());
    }

    #[test]
    fn servers_rotate_after_repeated_failures_and_remember_what_works() {
        let s = Servers::new(vec![
            url("a.example:1"),
            url("b.example:1"),
            url("c.example:1"),
        ])
        .unwrap();
        assert_eq!(s.order().collect::<Vec<_>>(), vec![0, 1, 2]);
        // One failure isn't enough to give up on a server.
        assert!(!s.failed(0));
        assert_eq!(s.current(), 0);
        assert!(s.failed(0));
        assert_eq!(s.current(), 1);
        assert_eq!(s.order().collect::<Vec<_>>(), vec![1, 2, 0]);
        // A stale report about a server no longer in use changes nothing.
        assert!(!s.failed(0));
        assert!(!s.failed(0));
        assert_eq!(s.current(), 1);
        // A success resets the count.
        s.failed(1);
        s.worked(1);
        assert!(!s.failed(1));
        assert_eq!(s.current(), 1);
        // Connecting found that the third one works.
        s.worked(2);
        assert_eq!(s.order().collect::<Vec<_>>(), vec![2, 0, 1]);
        // Wraps around.
        s.failed(2);
        s.failed(2);
        assert_eq!(s.current(), 0);
    }

    #[test]
    fn transport_errors_are_told_apart_from_server_answers() {
        let io = EError::IOError(std::io::Error::new(std::io::ErrorKind::TimedOut, "t"));
        assert!(is_transport(&io));
        assert!(is_transport(&EError::Mpsc));
        assert!(!is_transport(&EError::Protocol(serde_json::json!({
            "code": 1, "message": "missing inputs"
        }))));
        assert!(!is_transport(&EError::Message("bad".into())));
    }

    #[test]
    fn script_hash_matches_electrum() {
        // From the Electrum protocol docs: P2PKH 1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa.
        let script = bdk_wallet::bitcoin::ScriptBuf::from_hex(
            "76a91462e907b15cbf27d5425399ebf6f0fb50ebb88f1888ac",
        )
        .unwrap();
        assert_eq!(
            script_hash(&script),
            "8b01df4e368ea28f8dc0423bcf7a4923e3a12d307c875e47a0cfbf90b5c39161"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn falls_back_to_the_next_server_and_retries_dropped_calls() {
        let server = crate::push::tests::fake_server_dropping(Some(3)).await;
        let dead = ServerUrl {
            host: "127.0.0.1".into(),
            port: 9,
            tls: false,
        };
        let servers = Servers::new(vec![dead, server.url.clone()]).unwrap();
        let conn = Arc::new(Conn::new(servers.clone()));
        let c = conn.clone();
        let heights = tokio::task::spawn_blocking(move || {
            // version (1), ping (2), then the 3rd request is cut off: the
            // call is retried on a new connection and still answers.
            c.ping().unwrap();
            let h = c.block_headers_subscribe().unwrap();
            let fee = c.estimate_fee(6, None).unwrap();
            (h.height, fee)
        })
        .await
        .unwrap();
        assert_eq!(heights, (0, 0.0001));
        assert_eq!(servers.current(), 1);
        assert_eq!(conn.connected_to(), Some(server.url.clone()));
        assert_eq!(server.connections.load(Ordering::SeqCst), 2);
        // A server's own error isn't retried.
        let c = conn.clone();
        let e = tokio::task::spawn_blocking(move || c.relay_fee().unwrap_err())
            .await
            .unwrap();
        assert!(!is_transport(&e));
        assert_eq!(server.connections.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn pools_are_shared_per_server_list() {
        let a = pool_for(&[url("a.example:1")]).unwrap();
        let b = pool_for(&[url("a.example:1")]).unwrap();
        let c = pool_for(&[url("c.example:1")]).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &c));
        assert!(Arc::ptr_eq(&a.lanes, &b.lanes));
    }
}

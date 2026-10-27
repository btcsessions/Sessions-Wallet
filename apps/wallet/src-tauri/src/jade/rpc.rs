//! Jade's CBOR RPC over any byte link, and the PIN-server unlock.
//!
//! Requests and replies are CBOR maps (`{id, method, params}` →
//! `{id, result | error}`) streamed over the link with no framing, so
//! incoming bytes collect until one whole CBOR item decodes. Jade may also
//! send `{log: …}` messages, which are skipped, as are replies to requests we
//! already gave up on.
//!
//! The protocol reference is Jade's `docs/index.rst`.

use std::future::Future;
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use ciborium::Value as Cbor;
use sessions_core::{Error, Network};
use tokio::sync::Mutex;
use tokio::time::Instant;

/// Jade's RPC error codes (`main/utils/cbor_rpc.h`).
pub const UNKNOWN_METHOD: i64 = -32601;
pub const USER_CANCELLED: i64 = -32000;
pub const HW_LOCKED: i64 = -32002;
pub const NETWORK_MISMATCH: i64 = -32003;

/// A sanity cap on one message (a large signed PSBT arrives in pieces anyway).
const MAX_MESSAGE: usize = 1 << 20;

/// Rounds of PIN-server requests before giving up (one today, two on old firmware).
const MAX_UNLOCK_ROUNDS: usize = 5;

/// Why a call failed.
#[derive(Debug)]
pub enum Fail {
    /// The link dropped or I/O failed: reconnecting may help.
    Link(String),
    /// Jade didn't answer in time.
    Timeout,
    /// Jade answered with an error.
    Rpc { code: i64, message: String },
    /// Plain English for people, passed on as-is.
    App(Error),
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::App(e)
    }
}

/// Bytes to and from Jade (Bluetooth LE here). Implementations chunk writes
/// as the link needs.
#[async_trait]
pub trait Link: Send + Sync {
    async fn send(&self, bytes: &[u8]) -> Result<(), Fail>;
    /// The next bytes Jade sent. Waits; fails once the link is gone.
    async fn recv(&self) -> Result<Vec<u8>, Fail>;
    /// False once the link is known to have dropped.
    fn is_up(&self) -> bool {
        true
    }
    /// How Jade names this connection in `get_version_info` ("BLE", "SERIAL").
    fn interface(&self) -> &'static str {
        "BLE"
    }
    async fn close(&self);
}

/// The name Jade uses for a network.
pub fn network_name(network: Network) -> &'static str {
    match network {
        Network::Bitcoin => "mainnet",
        Network::Testnet | Network::Signet => "testnet",
        Network::Regtest => "localtest",
    }
}

// --- CBOR helpers -----------------------------------------------------------

fn text(s: &str) -> Cbor {
    Cbor::Text(s.to_owned())
}

fn map(entries: Vec<(&str, Cbor)>) -> Cbor {
    Cbor::Map(entries.into_iter().map(|(k, v)| (text(k), v)).collect())
}

fn get<'a>(v: &'a Cbor, key: &str) -> Option<&'a Cbor> {
    v.as_map()?
        .iter()
        .find(|(k, _)| k.as_text() == Some(key))
        .map(|(_, v)| v)
}

fn as_u64(v: &Cbor) -> Option<u64> {
    v.as_integer().and_then(|i| u64::try_from(i).ok())
}

fn path(numbers: &[u32]) -> Cbor {
    Cbor::Array(numbers.iter().map(|n| Cbor::Integer((*n).into())).collect())
}

pub fn to_bytes(v: &Cbor) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).expect("serialising a CBOR value can't fail");
    out
}

/// One whole CBOR item from the front of `buf` and how many bytes it took,
/// or `None` if more bytes are needed.
fn decode_one(buf: &[u8]) -> Result<Option<(Cbor, usize)>, Fail> {
    if buf.is_empty() {
        return Ok(None);
    }
    let mut cursor = Cursor::new(buf);
    match ciborium::de::from_reader::<Cbor, _>(&mut cursor) {
        Ok(v) => Ok(Some((v, cursor.position() as usize))),
        Err(ciborium::de::Error::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            Ok(None)
        }
        Err(e) => Err(Fail::Link(format!("garbled data from Jade: {e}"))),
    }
}

fn unexpected() -> Fail {
    Fail::App(Error::Invalid("Jade sent an unexpected reply".into()))
}

/// A reply to one of our requests.
#[derive(Debug)]
pub struct Reply {
    pub result: Option<Cbor>,
    pub error: Option<(i64, String)>,
    /// Set when the result comes in several messages (`sign_psbt`).
    pub seqnum: Option<u64>,
    pub seqlen: Option<u64>,
}

impl Reply {
    fn parse(msg: &Cbor) -> Reply {
        let error = get(msg, "error").map(|e| {
            let code = get(e, "code")
                .and_then(|c| c.as_integer())
                .and_then(|c| i64::try_from(c).ok())
                .unwrap_or(0);
            let message = get(e, "message")
                .and_then(Cbor::as_text)
                .unwrap_or_default()
                .to_owned();
            (code, message)
        });
        Reply {
            result: get(msg, "result").cloned(),
            error,
            seqnum: get(msg, "seqnum").and_then(as_u64),
            seqlen: get(msg, "seqlen").and_then(as_u64),
        }
    }

    pub fn into_result(self) -> Result<Cbor, Fail> {
        if let Some((code, message)) = self.error {
            return Err(Fail::Rpc { code, message });
        }
        self.result.ok_or_else(unexpected)
    }
}

/// Version and state from `get_version_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    pub version: (u32, u32, u32),
    /// UNINIT, UNSAVED, LOCKED, READY or TEMP.
    pub state: String,
    /// Which connection holds the unlocked wallet: NONE, QR, SERIAL or BLE.
    pub interface: String,
}

impl VersionInfo {
    /// Already unlocked for this connection (`"BLE"`, `"SERIAL"`): no PIN needed.
    pub fn ready_on(&self, interface: &str) -> bool {
        matches!(self.state.as_str(), "READY" | "TEMP") && self.interface == interface
    }
}

/// "1.0.41" or "1.0.41-beta2" → (1, 0, 41).
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut parts = s.trim().splitn(3, '.');
    let mut next = || -> Option<u32> {
        let p = parts.next()?;
        let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    };
    Some((next()?, next()?, next()?))
}

// --- multisig ---------------------------------------------------------------

/// One key of a multisig wallet registered on Jade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signer {
    /// Root fingerprint of the key's wallet.
    pub fingerprint: [u8; 4],
    /// Path from that root to `xpub` (Jade checks it for its own key).
    pub derivation: Vec<u32>,
    pub xpub: String,
    /// Fixed path applied after the xpub; empty for BIP48 wallets.
    pub path: Vec<u32>,
}

/// A multisig wallet as Jade registers it (`register_multisig`,
/// `get_registered_multisig`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Multisig {
    /// "wsh(multi(k))", "sh(wsh(multi(k)))" or "sh(multi(k))".
    pub variant: String,
    /// BIP67 key sorting (`sortedmulti`).
    pub sorted: bool,
    pub threshold: u32,
    pub signers: Vec<Signer>,
}

/// A registration as `get_registered_multisigs` sums it up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultisigSummary {
    pub variant: String,
    pub sorted: bool,
    pub threshold: u32,
    pub num_signers: u32,
}

fn as_u32(v: &Cbor) -> Option<u32> {
    as_u64(v).and_then(|n| u32::try_from(n).ok())
}

fn path_of(v: Option<&Cbor>) -> Option<Vec<u32>> {
    match v {
        None | Some(Cbor::Null) => Some(Vec::new()),
        Some(v) => v.as_array()?.iter().map(as_u32).collect(),
    }
}

impl Multisig {
    fn to_cbor(&self) -> Cbor {
        map(vec![
            ("variant", text(&self.variant)),
            ("sorted", Cbor::Bool(self.sorted)),
            ("threshold", Cbor::Integer(self.threshold.into())),
            (
                "signers",
                Cbor::Array(
                    self.signers
                        .iter()
                        .map(|s| {
                            map(vec![
                                ("fingerprint", Cbor::Bytes(s.fingerprint.to_vec())),
                                ("derivation", path(&s.derivation)),
                                ("xpub", text(&s.xpub)),
                                ("path", path(&s.path)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])
    }

    fn from_cbor(v: &Cbor) -> Option<Multisig> {
        let signers = get(v, "signers")?
            .as_array()?
            .iter()
            .map(|s| {
                Some(Signer {
                    fingerprint: get(s, "fingerprint")?
                        .as_bytes()?
                        .as_slice()
                        .try_into()
                        .ok()?,
                    derivation: path_of(get(s, "derivation"))?,
                    xpub: get(s, "xpub")?.as_text()?.to_owned(),
                    path: path_of(get(s, "path"))?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Multisig {
            variant: get(v, "variant")?.as_text()?.to_owned(),
            sorted: get(v, "sorted")?.as_bool()?,
            threshold: as_u32(get(v, "threshold")?)?,
            signers,
        })
    }
}

// --- client -----------------------------------------------------------------

/// One conversation with Jade at a time over a [`Link`].
pub struct Client {
    link: Box<dyn Link>,
    /// Bytes received but not yet a whole message. The lock also keeps one
    /// request/reply exchange at a time.
    inbox: Mutex<Vec<u8>>,
    next_id: AtomicU64,
}

impl Client {
    pub fn new(link: Box<dyn Link>) -> Self {
        Client {
            link,
            inbox: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
        }
    }

    pub async fn close(&self) {
        self.link.close().await;
    }

    /// False once the link is known to have dropped.
    pub fn is_up(&self) -> bool {
        self.link.is_up()
    }

    pub fn interface(&self) -> &'static str {
        self.link.interface()
    }

    /// Jade answers `ping` at once, even mid-request, without touching the
    /// screen or the wallet. It does count as activity for Jade's idle timer.
    pub async fn ping(&self, wait: Duration) -> Result<(), Fail> {
        match self.call("ping", None, wait).await {
            Ok(_) | Err(Fail::Rpc { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn new_id(&self) -> String {
        // Jade allows ids up to 15 characters.
        format!(
            "w{}",
            self.next_id.fetch_add(1, Ordering::Relaxed) % 1_000_000_000
        )
    }

    fn request(&self, method: &str, params: Option<Cbor>) -> Cbor {
        let mut entries = vec![("id", text(&self.new_id())), ("method", text(method))];
        if let Some(p) = params {
            entries.push(("params", p));
        }
        map(entries)
    }

    /// Send a whole request message (it must have an `id`) and wait up to
    /// `wait` for the reply with the same id.
    pub async fn exchange(&self, request: &Cbor, wait: Duration) -> Result<Reply, Fail> {
        let id = get(request, "id")
            .and_then(Cbor::as_text)
            .ok_or_else(unexpected)?
            .to_owned();
        let mut inbox = self.inbox.lock().await;
        self.link.send(&to_bytes(request)).await?;
        let deadline = Instant::now() + wait;
        loop {
            let msg = self.read_message(&mut inbox, deadline).await?;
            let Some(reply_id) = get(&msg, "id").and_then(Cbor::as_text) else {
                // A log line, or something else unsolicited.
                if let Some(line) = get(&msg, "log") {
                    tracing::debug!("jade: {line:?}");
                }
                continue;
            };
            let is_error = get(&msg, "error").is_some();
            // "00" is Jade's id for errors it couldn't tie to a request.
            if reply_id == id || (reply_id == "00" && is_error) {
                return Ok(Reply::parse(&msg));
            }
            tracing::debug!("jade: skipping a reply to an earlier request ({reply_id})");
        }
    }

    async fn read_message(&self, buf: &mut Vec<u8>, deadline: Instant) -> Result<Cbor, Fail> {
        loop {
            match decode_one(buf) {
                Ok(Some((msg, used))) => {
                    buf.drain(..used);
                    return Ok(msg);
                }
                Ok(None) => {}
                Err(e) => {
                    buf.clear();
                    return Err(e);
                }
            }
            if buf.len() > MAX_MESSAGE {
                buf.clear();
                return Err(Fail::Link("message from Jade too large".into()));
            }
            let chunk = tokio::time::timeout_at(deadline, self.link.recv())
                .await
                .map_err(|_| Fail::Timeout)??;
            buf.extend_from_slice(&chunk);
        }
    }

    /// Call `method` and return its `result`.
    pub async fn call(
        &self,
        method: &str,
        params: Option<Cbor>,
        wait: Duration,
    ) -> Result<Cbor, Fail> {
        self.exchange(&self.request(method, params), wait)
            .await?
            .into_result()
    }

    pub async fn version_info(&self, wait: Duration) -> Result<VersionInfo, Fail> {
        let r = self.call("get_version_info", None, wait).await?;
        let version = get(&r, "JADE_VERSION")
            .and_then(Cbor::as_text)
            .and_then(parse_version)
            .ok_or_else(unexpected)?;
        let field = |key| {
            get(&r, key)
                .and_then(Cbor::as_text)
                .unwrap_or("UNKNOWN")
                .to_owned()
        };
        Ok(VersionInfo {
            version,
            state: field("JADE_STATE"),
            interface: field("JADE_INTERFACE"),
        })
    }

    /// Unlock: `auth_user`, relaying every PIN-server request Jade makes
    /// (through `relay`, which returns `(on_reply, body)`) until Jade says
    /// whether the PIN was right. The PIN itself is entered on Jade.
    pub async fn auth_user<R, F>(
        &self,
        network: &str,
        epoch: u64,
        relay: R,
        wait: Duration,
    ) -> Result<(), Fail>
    where
        R: Fn(serde_json::Value) -> F,
        F: Future<Output = sessions_core::Result<(String, serde_json::Value)>>,
    {
        let mut request = self.request(
            "auth_user",
            Some(map(vec![
                ("network", text(network)),
                ("epoch", Cbor::Integer(epoch.into())),
            ])),
        );
        for _ in 0..MAX_UNLOCK_ROUNDS {
            let result = self.exchange(&request, wait).await?.into_result()?;
            match &result {
                Cbor::Bool(true) => return Ok(()),
                Cbor::Bool(false) => {
                    return Err(Fail::App(Error::Invalid(
                        "Wrong PIN. Jade shows how many tries are left. Try again".into(),
                    )))
                }
                _ => {}
            }
            let http = get(&result, "http_request").ok_or_else(unexpected)?;
            let http = serde_json::to_value(http).map_err(|_| unexpected())?;
            let (on_reply, body) = relay(http).await?;
            let reply = super::reply_message(&on_reply, body);
            request = Cbor::serialized(&reply).map_err(|_| unexpected())?;
        }
        Err(Fail::App(Error::Invalid(
            "Jade kept asking its PIN server. Restart Jade and try again".into(),
        )))
    }

    /// The xpub at `path` (empty for the root key).
    pub async fn get_xpub(
        &self,
        network: &str,
        path_: &[u32],
        wait: Duration,
    ) -> Result<String, Fail> {
        let r = self
            .call(
                "get_xpub",
                Some(map(vec![("network", text(network)), ("path", path(path_))])),
                wait,
            )
            .await?;
        r.into_text().map_err(|_| unexpected())
    }

    /// Ask Jade to sign `psbt` (raw bytes). Waits for the user to confirm on
    /// the device; collects a reply split over several messages.
    pub async fn sign_psbt(
        &self,
        network: &str,
        psbt: Vec<u8>,
        wait: Duration,
    ) -> Result<Vec<u8>, Fail> {
        let request = self.request(
            "sign_psbt",
            Some(map(vec![
                ("network", text(network)),
                ("psbt", Cbor::Bytes(psbt)),
            ])),
        );
        let origid = get(&request, "id")
            .and_then(Cbor::as_text)
            .unwrap_or_default()
            .to_owned();
        let mut reply = self.exchange(&request, wait).await?;
        let mut out = Vec::new();
        loop {
            let (seqnum, seqlen) = (reply.seqnum, reply.seqlen);
            out.extend(
                reply
                    .into_result()?
                    .into_bytes()
                    .map_err(|_| unexpected())?,
            );
            let (Some(seqnum), Some(seqlen)) = (seqnum, seqlen) else {
                return Ok(out);
            };
            if seqnum >= seqlen {
                return Ok(out);
            }
            if seqlen > 10_000 {
                return Err(unexpected());
            }
            let more = self.request(
                "get_extended_data",
                Some(map(vec![
                    ("origid", text(&origid)),
                    ("orig", text("sign_psbt")),
                    ("seqnum", Cbor::Integer((seqnum + 1).into())),
                    ("seqlen", Cbor::Integer(seqlen.into())),
                ])),
            );
            reply = self.exchange(&more, wait).await?;
        }
    }

    /// Show a single-sig address on Jade (`variant` "wpkh(k)" or "tr(k)",
    /// full path). Returns it once the user confirms.
    pub async fn receive_address(
        &self,
        network: &str,
        variant: &str,
        path_: &[u32],
        wait: Duration,
    ) -> Result<String, Fail> {
        let r = self
            .call(
                "get_receive_address",
                Some(map(vec![
                    ("network", text(network)),
                    ("variant", text(variant)),
                    ("path", path(path_)),
                ])),
                wait,
            )
            .await?;
        r.into_text().map_err(|_| unexpected())
    }

    /// Multisig wallets registered on Jade for its current wallet, by name.
    pub async fn registered_multisigs(
        &self,
        wait: Duration,
    ) -> Result<Vec<(String, MultisigSummary)>, Fail> {
        let r = self.call("get_registered_multisigs", None, wait).await?;
        let entries = r.as_map().ok_or_else(unexpected)?;
        entries
            .iter()
            .map(|(name, v)| {
                let summary = MultisigSummary {
                    variant: get(v, "variant")?.as_text()?.to_owned(),
                    sorted: get(v, "sorted")?.as_bool()?,
                    threshold: as_u32(get(v, "threshold")?)?,
                    num_signers: as_u32(get(v, "num_signers")?)?,
                };
                Some((name.as_text()?.to_owned(), summary))
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(unexpected)
    }

    /// The keys of registered multisig wallet `name` (firmware 1.0.23+, and
    /// only for wallets registered by such firmware).
    pub async fn registered_multisig(&self, name: &str, wait: Duration) -> Result<Multisig, Fail> {
        let r = self
            .call(
                "get_registered_multisig",
                Some(map(vec![
                    ("multisig_name", text(name)),
                    ("as_file", Cbor::Bool(false)),
                ])),
                wait,
            )
            .await?;
        get(&r, "descriptor")
            .and_then(Multisig::from_cbor)
            .ok_or_else(unexpected)
    }

    /// Register multisig wallet `name` (at most 15 characters). Jade asks the
    /// person to confirm, unless the very same registration already exists;
    /// a different registration with the same name is overwritten.
    pub async fn register_multisig(
        &self,
        network: &str,
        name: &str,
        multisig: &Multisig,
        wait: Duration,
    ) -> Result<(), Fail> {
        let r = self
            .call(
                "register_multisig",
                Some(map(vec![
                    ("network", text(network)),
                    ("multisig_name", text(name)),
                    ("descriptor", multisig.to_cbor()),
                ])),
                wait,
            )
            .await?;
        match r {
            Cbor::Bool(true) => Ok(()),
            _ => Err(unexpected()),
        }
    }

    /// Show an address of registered multisig wallet `name` on Jade. `paths`
    /// are the suffixes after each signer's xpub, in registration order.
    /// Returns it once the person confirms.
    pub async fn multisig_receive_address(
        &self,
        network: &str,
        name: &str,
        paths: &[Vec<u32>],
        wait: Duration,
    ) -> Result<String, Fail> {
        let r = self
            .call(
                "get_receive_address",
                Some(map(vec![
                    ("network", text(network)),
                    ("multisig_name", text(name)),
                    (
                        "paths",
                        Cbor::Array(paths.iter().map(|p| path(p)).collect()),
                    ),
                ])),
                wait,
            )
            .await?;
        r.into_text().map_err(|_| unexpected())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    type Handler = dyn Fn(&Cbor) -> Vec<Cbor> + Send + Sync;

    /// Stands in for Jade: decodes each request as its bytes arrive and
    /// answers with `handler`'s messages, delivered in awkward 7-byte pieces.
    pub(crate) struct FakeJade {
        sent: std::sync::Mutex<Vec<u8>>,
        incoming: std::sync::Mutex<VecDeque<Vec<u8>>>,
        handler: Box<Handler>,
    }

    impl FakeJade {
        pub(crate) fn new(handler: impl Fn(&Cbor) -> Vec<Cbor> + Send + Sync + 'static) -> Self {
            FakeJade {
                sent: Default::default(),
                incoming: Default::default(),
                handler: Box::new(handler),
            }
        }
    }

    #[async_trait]
    impl Link for FakeJade {
        async fn send(&self, bytes: &[u8]) -> Result<(), Fail> {
            let mut sent = self.sent.lock().unwrap();
            sent.extend_from_slice(bytes);
            while let Some((req, used)) = decode_one(&sent)? {
                sent.drain(..used);
                let out: Vec<u8> = (self.handler)(&req).iter().flat_map(to_bytes).collect();
                let mut incoming = self.incoming.lock().unwrap();
                for piece in out.chunks(7) {
                    incoming.push_back(piece.to_vec());
                }
            }
            Ok(())
        }

        async fn recv(&self) -> Result<Vec<u8>, Fail> {
            self.incoming
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Fail::Link("nothing more from the fake".into()))
        }

        async fn close(&self) {}
    }

    pub(crate) fn method(req: &Cbor) -> &str {
        get(req, "method").and_then(Cbor::as_text).unwrap_or("")
    }

    pub(crate) fn reply(req: &Cbor, result: Cbor) -> Cbor {
        map(vec![
            ("id", get(req, "id").unwrap().clone()),
            ("result", result),
        ])
    }

    fn error(req: &Cbor, code: i64, message: &str) -> Cbor {
        map(vec![
            ("id", get(req, "id").unwrap().clone()),
            (
                "error",
                map(vec![
                    ("code", Cbor::Integer(code.into())),
                    ("message", text(message)),
                ]),
            ),
        ])
    }

    fn http_request(on_reply: &str) -> Cbor {
        map(vec![(
            "http_request",
            map(vec![
                (
                    "params",
                    map(vec![
                        (
                            "urls",
                            Cbor::Array(vec![
                                text("https://jadepin.blockstream.com/get_pin"),
                                text("http://x.onion/get_pin"),
                            ]),
                        ),
                        ("method", text("POST")),
                        ("accept", text("json")),
                        ("data", map(vec![("data", text("from-jade"))])),
                    ]),
                ),
                ("on-reply", text(on_reply)),
            ]),
        )])
    }

    const W: Duration = Duration::from_secs(5);

    #[test]
    fn decodes_streamed_cbor_piece_by_piece() {
        let a = to_bytes(&map(vec![
            ("id", text("1")),
            ("result", Cbor::Bytes(vec![7; 300])),
        ]));
        let b = to_bytes(&map(vec![("log", Cbor::Bytes(b"I hello".to_vec()))]));
        let mut all = a.clone();
        all.extend(&b);
        // Every strict prefix of the first message needs more bytes.
        for n in 0..a.len() {
            assert!(decode_one(&all[..n]).unwrap().is_none(), "prefix {n}");
        }
        let (first, used) = decode_one(&all).unwrap().unwrap();
        assert_eq!(used, a.len());
        assert_eq!(get(&first, "id").unwrap().as_text(), Some("1"));
        let (second, used) = decode_one(&all[a.len()..]).unwrap().unwrap();
        assert_eq!(used, b.len());
        assert!(get(&second, "log").is_some());
        // Not CBOR at all.
        assert!(decode_one(&[0xff, 0xff]).is_err());
    }

    #[test]
    fn versions_parse() {
        assert_eq!(parse_version("1.0.41"), Some((1, 0, 41)));
        assert_eq!(parse_version("0.1.48-beta2"), Some((0, 1, 48)));
        assert!(parse_version("1.0").is_none());
        assert!(parse_version("x.y.z").is_none());
        assert_eq!(network_name(Network::Bitcoin), "mainnet");
        assert_eq!(network_name(Network::Testnet), "testnet");
    }

    #[tokio::test]
    async fn calls_skip_logs_and_stale_replies() {
        let jade = FakeJade::new(|req| {
            assert_eq!(method(req), "get_version_info");
            vec![
                map(vec![("log", Cbor::Bytes(b"I busy".to_vec()))]),
                map(vec![("id", text("stale")), ("result", Cbor::Bool(true))]),
                reply(
                    req,
                    map(vec![
                        ("JADE_VERSION", text("1.0.41")),
                        ("JADE_STATE", text("LOCKED")),
                    ]),
                ),
            ]
        });
        let client = Client::new(Box::new(jade));
        let info = client.version_info(W).await.unwrap();
        assert_eq!(info.version, (1, 0, 41));
        assert_eq!(info.state, "LOCKED");
        assert!(!info.ready_on("BLE"));
    }

    #[tokio::test]
    async fn errors_come_back_as_rpc_failures() {
        let client = Client::new(Box::new(FakeJade::new(|req| {
            vec![error(req, USER_CANCELLED, "User declined")]
        })));
        match client.get_xpub("mainnet", &[], W).await {
            Err(Fail::Rpc { code, message }) => {
                assert_eq!(code, USER_CANCELLED);
                assert_eq!(message, "User declined");
            }
            other => panic!("{other:?}"),
        }
    }

    type RelayFuture = std::pin::Pin<
        Box<dyn Future<Output = sessions_core::Result<(String, serde_json::Value)>> + Send>,
    >;

    /// Relays nothing over the network: records the request, answers for the server.
    fn fake_relay(calls: Arc<AtomicUsize>) -> impl Fn(serde_json::Value) -> RelayFuture {
        move |req| {
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    req["params"]["urls"][0],
                    "https://jadepin.blockstream.com/get_pin"
                );
                assert_eq!(req["params"]["data"]["data"], "from-jade");
                let on_reply = req["on-reply"].as_str().unwrap().to_owned();
                Ok((on_reply, serde_json::json!({ "data": "from-server" })))
            })
        }
    }

    #[tokio::test]
    async fn unlock_relays_to_the_pin_server() {
        let jade = FakeJade::new(|req| match method(req) {
            "auth_user" => {
                let p = get(req, "params").unwrap();
                assert_eq!(get(p, "network").unwrap().as_text(), Some("mainnet"));
                assert_eq!(as_u64(get(p, "epoch").unwrap()), Some(1_700_000_000));
                vec![reply(req, http_request("pin"))]
            }
            "pin" => {
                let p = get(req, "params").unwrap();
                assert_eq!(get(p, "data").unwrap().as_text(), Some("from-server"));
                vec![reply(req, Cbor::Bool(true))]
            }
            m => panic!("unexpected {m}"),
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let client = Client::new(Box::new(jade));
        client
            .auth_user("mainnet", 1_700_000_000, fake_relay(calls.clone()), W)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unlock_handles_the_old_two_step_handshake_and_already_unlocked() {
        let jade = FakeJade::new(|req| match method(req) {
            "auth_user" => vec![reply(req, http_request("handshake_init"))],
            "handshake_init" => vec![reply(req, http_request("handshake_complete"))],
            "handshake_complete" => vec![reply(req, Cbor::Bool(true))],
            m => panic!("unexpected {m}"),
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let client = Client::new(Box::new(jade));
        client
            .auth_user("mainnet", 1, fake_relay(calls.clone()), W)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let unlocked = Client::new(Box::new(FakeJade::new(|req| {
            vec![reply(req, Cbor::Bool(true))]
        })));
        let calls = Arc::new(AtomicUsize::new(0));
        unlocked
            .auth_user("mainnet", 1, fake_relay(calls.clone()), W)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn wrong_pin_and_cancel_fail_plainly() {
        let jade = FakeJade::new(|req| match method(req) {
            "auth_user" => vec![reply(req, http_request("pin"))],
            _ => vec![reply(req, Cbor::Bool(false))],
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let client = Client::new(Box::new(jade));
        match client.auth_user("mainnet", 1, fake_relay(calls), W).await {
            Err(Fail::App(e)) => assert!(e.to_string().starts_with("Wrong PIN")),
            other => panic!("{other:?}"),
        }

        let cancelled = Client::new(Box::new(FakeJade::new(|req| {
            vec![error(req, USER_CANCELLED, "User abandonded pin entry")]
        })));
        let calls = Arc::new(AtomicUsize::new(0));
        assert!(matches!(
            cancelled
                .auth_user("mainnet", 1, fake_relay(calls), W)
                .await,
            Err(Fail::Rpc {
                code: USER_CANCELLED,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn signed_psbts_are_collected_from_several_messages() {
        let parts: Vec<Vec<u8>> = vec![vec![1; 600], vec![2; 600], vec![3; 10]];
        let first_id = Arc::new(std::sync::Mutex::new(String::new()));
        let seen = first_id.clone();
        let jade = FakeJade::new(move |req| {
            let p = get(req, "params").unwrap();
            let with_seq = |n: u64, data: &Vec<u8>| {
                let mut m = reply(req, Cbor::Bytes(data.clone()));
                if let Cbor::Map(entries) = &mut m {
                    entries.push((text("seqnum"), Cbor::Integer(n.into())));
                    entries.push((text("seqlen"), Cbor::Integer(3u64.into())));
                }
                m
            };
            match method(req) {
                "sign_psbt" => {
                    assert_eq!(get(p, "network").unwrap().as_text(), Some("mainnet"));
                    assert_eq!(get(p, "psbt").unwrap().as_bytes(), Some(&b"psbt".to_vec()));
                    *seen.lock().unwrap() = get(req, "id").unwrap().as_text().unwrap().to_owned();
                    vec![with_seq(1, &parts[0])]
                }
                "get_extended_data" => {
                    assert_eq!(
                        get(p, "origid").unwrap().as_text().unwrap(),
                        *seen.lock().unwrap()
                    );
                    assert_eq!(get(p, "orig").unwrap().as_text(), Some("sign_psbt"));
                    let n = as_u64(get(p, "seqnum").unwrap()).unwrap();
                    vec![with_seq(n, &parts[n as usize - 1])]
                }
                m => panic!("unexpected {m}"),
            }
        });
        let client = Client::new(Box::new(jade));
        let out = client
            .sign_psbt("mainnet", b"psbt".to_vec(), W)
            .await
            .unwrap();
        assert_eq!(out.len(), 1210);
        assert!(out[..600].iter().all(|b| *b == 1));
        assert!(out[600..1200].iter().all(|b| *b == 2));
        assert!(out[1200..].iter().all(|b| *b == 3));
        assert!(!first_id.lock().unwrap().is_empty());

        // A reply in one piece has no sequence fields.
        let single = Client::new(Box::new(FakeJade::new(|req| {
            vec![reply(req, Cbor::Bytes(vec![9; 5]))]
        })));
        assert_eq!(
            single.sign_psbt("mainnet", vec![0], W).await.unwrap(),
            vec![9; 5]
        );
    }

    #[tokio::test]
    async fn requests_carry_paths_and_variants() {
        let client = Client::new(Box::new(FakeJade::new(|req| {
            let p = get(req, "params").unwrap();
            let path: Vec<u64> = get(p, "path")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|n| as_u64(n).unwrap())
                .collect();
            match method(req) {
                "get_xpub" => {
                    assert_eq!(path, vec![0x8000_0054, 0x8000_0000, 0x8000_0002]);
                    vec![reply(req, text("xpub-here"))]
                }
                "get_receive_address" => {
                    assert_eq!(get(p, "variant").unwrap().as_text(), Some("tr(k)"));
                    assert_eq!(path.len(), 5);
                    vec![reply(req, text("bc1p…"))]
                }
                m => panic!("unexpected {m}"),
            }
        })));
        assert_eq!(
            client
                .get_xpub("mainnet", &[0x8000_0054, 0x8000_0000, 0x8000_0002], W)
                .await
                .unwrap(),
            "xpub-here"
        );
        assert_eq!(
            client
                .receive_address("mainnet", "tr(k)", &[1, 2, 3, 4, 5], W)
                .await
                .unwrap(),
            "bc1p…"
        );
    }

    #[tokio::test]
    async fn multisig_requests_have_jades_shape() {
        let wanted = Multisig {
            variant: "wsh(multi(k))".into(),
            sorted: true,
            threshold: 2,
            signers: vec![
                Signer {
                    fingerprint: [0x73, 0xc5, 0xda, 0x0a],
                    derivation: vec![0x8000_0030, 0x8000_0000, 0x8000_0000, 0x8000_0002],
                    xpub: "xpub-a".into(),
                    path: vec![],
                },
                Signer {
                    fingerprint: [1, 2, 3, 4],
                    derivation: vec![],
                    xpub: "xpub-b".into(),
                    path: vec![],
                },
            ],
        };
        let registered = std::sync::Mutex::new(None::<Cbor>);
        let registered = Arc::new(registered);
        let seen = registered.clone();
        let client = Client::new(Box::new(FakeJade::new(move |req| {
            let p = get(req, "params");
            match method(req) {
                "register_multisig" => {
                    let p = p.unwrap();
                    assert_eq!(get(p, "network").unwrap().as_text(), Some("mainnet"));
                    assert_eq!(get(p, "multisig_name").unwrap().as_text(), Some("Family"));
                    let d = get(p, "descriptor").unwrap();
                    assert_eq!(
                        get(
                            &get(d, "signers").unwrap().as_array().unwrap()[0],
                            "fingerprint"
                        )
                        .unwrap()
                        .as_bytes(),
                        Some(&vec![0x73, 0xc5, 0xda, 0x0a])
                    );
                    *seen.lock().unwrap() = Some(d.clone());
                    vec![reply(req, Cbor::Bool(true))]
                }
                "get_registered_multisigs" => {
                    assert!(p.is_none());
                    vec![reply(
                        req,
                        map(vec![(
                            "Family",
                            map(vec![
                                ("variant", text("wsh(multi(k))")),
                                ("sorted", Cbor::Bool(true)),
                                ("threshold", Cbor::Integer(2.into())),
                                ("num_signers", Cbor::Integer(2.into())),
                                ("master_blinding_key", Cbor::Bytes(vec![])),
                            ]),
                        )]),
                    )]
                }
                "get_registered_multisig" => {
                    let p = p.unwrap();
                    assert_eq!(get(p, "multisig_name").unwrap().as_text(), Some("Family"));
                    assert_eq!(get(p, "as_file"), Some(&Cbor::Bool(false)));
                    let d = seen.lock().unwrap().clone().unwrap();
                    vec![reply(
                        req,
                        map(vec![("multisig_name", text("Family")), ("descriptor", d)]),
                    )]
                }
                "get_receive_address" => {
                    let p = p.unwrap();
                    assert_eq!(get(p, "multisig_name").unwrap().as_text(), Some("Family"));
                    assert!(get(p, "variant").is_none() && get(p, "path").is_none());
                    let paths = get(p, "paths").unwrap().as_array().unwrap();
                    assert_eq!(paths.len(), 2);
                    assert_eq!(paths[1], path(&[0, 7]));
                    vec![reply(req, text("bc1q-vault"))]
                }
                m => panic!("unexpected {m}"),
            }
        })));
        client
            .register_multisig("mainnet", "Family", &wanted, W)
            .await
            .unwrap();
        let list = client.registered_multisigs(W).await.unwrap();
        assert_eq!(
            list,
            vec![(
                "Family".to_string(),
                MultisigSummary {
                    variant: "wsh(multi(k))".into(),
                    sorted: true,
                    threshold: 2,
                    num_signers: 2,
                }
            )]
        );
        assert_eq!(
            client.registered_multisig("Family", W).await.unwrap(),
            wanted
        );
        assert_eq!(
            client
                .multisig_receive_address("mainnet", "Family", &[vec![0, 7], vec![0, 7]], W)
                .await
                .unwrap(),
            "bc1q-vault"
        );
    }

    #[tokio::test]
    async fn silence_times_out() {
        struct Silent;
        #[async_trait]
        impl Link for Silent {
            async fn send(&self, _: &[u8]) -> Result<(), Fail> {
                Ok(())
            }
            async fn recv(&self) -> Result<Vec<u8>, Fail> {
                std::future::pending().await
            }
            async fn close(&self) {}
        }
        let client = Client::new(Box::new(Silent));
        assert!(matches!(
            client.call("ping", None, Duration::from_millis(50)).await,
            Err(Fail::Timeout)
        ));
    }
}

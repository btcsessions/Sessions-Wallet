//! Blockstream Jade: PIN-server relay, QR unlock, and Jade over Bluetooth
//! or (desktop) a USB cable.
//!
//! When Jade unlocks with a PIN it asks the host to forward an encrypted
//! blob to its PIN server (blind oracle) and hand the reply back. The server
//! never sees the PIN, seed or keys. The same relay serves QR unlock (the
//! request arrives as a `jade-pin` QR) and Bluetooth unlock.
//!
//! Over Bluetooth LE ([`ble`]) the wallet speaks Jade's CBOR RPC ([`rpc`]):
//! unlock, account xpubs, PSBT signing and address checks. The connected,
//! unlocked Jade stays connected between calls, so the PIN comes up once:
//! Jade locks itself whenever its Bluetooth link drops.
//!
//! Keeping the link: while the app is in the foreground and Jade was used in
//! the last [`KEEPALIVE_FOR`], a `ping` goes out every [`KEEPALIVE_EVERY`].
//! That notices a dropped link early (so the next call reconnects instead of
//! failing) and keeps Jade from powering off mid-task. Pings count as
//! activity for Jade's idle timer, so they stop after that window (and in
//! the background): Jade's own idle timeout, its owner's setting, then
//! decides when it locks and powers off, as it would without this app. The
//! link itself stays open.
//!
//! When the link has dropped (Jade off, out of range), the next call
//! reconnects by itself ("reconnecting"); a Jade the phone is paired with
//! isn't paired again. If the pairing went stale (Jade reset or forgot it),
//! Android re-pairs: the person confirms the number again (see [`ble`]).
//!
//! Events for the UI:
//! * `jade://status` `{ state }`, one of
//!   - "scanning": looking for Jade;
//!   - "reconnecting": the link dropped since the last call; finding Jade again;
//!   - "connecting": found it, connecting;
//!   - "pairing": the phone asks to pair: confirm the same number on Jade and
//!     the phone (Android; also when re-pairing after a stale pairing);
//!   - "unlock": enter the PIN on Jade (the PIN-server relay is automatic);
//!   - "confirm": check and confirm on Jade's screen.
//! * `jade://connected` `{}`: connected and unlocked.

mod ble;
mod rpc;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod usb;

/// A plugged-in Jade (desktop) wins over Bluetooth.
async fn open_link(app: &AppHandle) -> Result<Box<dyn rpc::Link>> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if let Some(port) = usb::find() {
        tracing::info!("jade: using USB ({port})");
        status(app, "connecting");
        return usb::open(&port).await;
    }
    ble::open(app).await
}

use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bitcoin::bip32::{DerivationPath, Fingerprint, Xpub};
use bitcoin::Psbt;
use rail_onchain::bitbox::{self as helpers, ScriptKind, JADE};
use rail_onchain::descriptors;
use rail_onchain::multisig::{MultisigConfig, Policy, ScriptType};
use serde::Serialize;
use serde_json::{json, Value};
use sessions_core::{AccountId, Error, Network, Result};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;

use crate::bitbox::{find_vault, vault};
use crate::config::AccountSpec;
use crate::state::{AppState, NETWORK};
use rpc::Fail;

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The `http_request` Jade sent, from a full RPC reply message.
fn http_request(message: &Value) -> Result<&Value> {
    message
        .get("result")
        .and_then(|r| r.get("http_request"))
        .ok_or_else(|| Error::Invalid("That isn't Jade's unlock QR. On Jade, choose QR PIN Unlock and scan the first code it shows.".into()))
}

/// First clearnet URL; onion URLs need Tor, which this app doesn't run.
fn pick_url(params: &Value) -> Result<String> {
    params
        .get("urls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find(|u| !u.is_empty() && !u.contains(".onion"))
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::Invalid("Jade didn't give a PIN server address this app can reach".into())
        })
}

/// Forward Jade's request to its PIN server. Returns `(on_reply, body)`.
pub async fn relay(request: &Value) -> Result<(String, Value)> {
    let params = request
        .get("params")
        .ok_or_else(|| Error::Invalid("Jade's unlock request is incomplete".into()))?;
    let on_reply = request
        .get("on-reply")
        .and_then(Value::as_str)
        .unwrap_or("pin")
        .to_owned();
    let url = pick_url(params)?;
    if params
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("POST")
        != "POST"
    {
        return Err(Error::Invalid(
            "Jade asked for an unsupported request".into(),
        ));
    }
    let data = params.get("data").cloned().unwrap_or(Value::Null);
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let res = client
        .post(&url)
        .header("content-type", "application/json")
        .body(data.to_string())
        .send()
        .await
        .map_err(|_| {
            Error::Invalid(
                "Couldn't reach Jade's PIN server. Check your internet connection and try again."
                    .into(),
            )
        })?;
    if !res.status().is_success() {
        return Err(Error::Invalid(format!(
            "Jade's PIN server refused the unlock ({}). Start again on Jade.",
            res.status().as_u16()
        )));
    }
    let body: Value = res
        .json()
        .await
        .map_err(|_| Error::Invalid("Jade's PIN server sent an unexpected reply".into()))?;
    Ok((on_reply, body))
}

/// The RPC message to send back to Jade with the server's reply.
pub fn reply_message(on_reply: &str, body: Value) -> Value {
    let id = format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() % 1_000_000)
            .unwrap_or(1)
    );
    json!({ "id": id, "method": on_reply, "params": body })
}

fn from_cbor(bytes: &[u8]) -> Result<Value> {
    ciborium::de::from_reader(bytes)
        .map_err(|_| Error::Invalid("Couldn't read Jade's QR code".into()))
}

fn to_cbor(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).expect("serialising JSON values to CBOR can't fail");
    out
}

/// QR unlock, step 2: take the CBOR from Jade's first `jade-pin` QR, ask the
/// PIN server, and return the CBOR for the `jade-pin` QR Jade scans next.
pub async fn qr_pin_step(request_cbor: &[u8]) -> Result<Vec<u8>> {
    let message = from_cbor(request_cbor)?;
    let (on_reply, body) = relay(http_request(&message)?).await?;
    Ok(to_cbor(&reply_message(&on_reply, body)))
}

/// Tauri command for QR unlock: bytes of Jade's QR in, bytes of the reply QR out.
#[tauri::command]
pub async fn jade_qr_pin(request: Vec<u8>) -> Result<Vec<u8>> {
    qr_pin_step(&request).await
}

// --- Bluetooth ------------------------------------------------------------------

const HARDENED: u32 = 0x8000_0000;
/// Oldest firmware whose `sign_psbt` takes a network.
const MIN_FIRMWARE: (u32, u32, u32) = (0, 1, 47);
/// First firmware that signs and shows single-key Taproot (BIP86) addresses.
const TAPROOT_FIRMWARE: (u32, u32, u32) = (1, 0, 34);
/// Calls that need nobody to do anything.
const CALL_WAIT: Duration = Duration::from_secs(30);
/// The first message: the OS may be showing its Bluetooth pairing dialog.
const FIRST_WAIT: Duration = Duration::from_secs(90);
/// Waiting on a person: PIN entry, checking a transaction or an address.
const USER_WAIT: Duration = Duration::from_secs(180);
/// Ping a connected Jade this often (Android and Jade never drop a link
/// that carries traffic, and a dead one shows up within this)...
const KEEPALIVE_EVERY: Duration = Duration::from_secs(15);
/// ...for this long after the wallet last used it. See the module docs.
const KEEPALIVE_FOR: Duration = Duration::from_secs(5 * 60);
/// Jade answers a ping at once.
const PING_WAIT: Duration = Duration::from_secs(5);

/// A connected, unlocked Jade.
struct Session {
    client: Arc<rpc::Client>,
    network: Network,
    fingerprint: Fingerprint,
    version: (u32, u32, u32),
}

/// The Bluetooth link to Jade, kept between calls.
struct Conn {
    /// Tells this link's keepalive task it's still the current one.
    id: u64,
    client: Arc<rpc::Client>,
    /// Once unlocked.
    session: Option<Arc<Session>>,
}

/// The Jade connected over Bluetooth, kept between calls.
pub struct JadeState {
    /// Also serialises device access: one conversation at a time.
    conn: Mutex<Option<Conn>>,
    next_id: AtomicU64,
    /// When the wallet last asked Jade for something.
    last_used: std::sync::Mutex<Instant>,
    /// The app is on screen (Android tells us when it isn't).
    foreground: AtomicBool,
    /// The last link dropped on its own: the next connect is a reconnect.
    dropped: AtomicBool,
    /// Bumped when a conversation is cancelled: one that then fails doesn't
    /// reconnect and try again.
    cancels: AtomicU64,
    /// The open link, reachable without the `conn` lock, so a stuck call can
    /// be cut off from outside.
    link: std::sync::Mutex<Option<Arc<rpc::Client>>>,
}

impl JadeState {
    pub fn new() -> Self {
        JadeState {
            conn: Mutex::new(None),
            next_id: AtomicU64::new(1),
            last_used: std::sync::Mutex::new(Instant::now()),
            foreground: AtomicBool::new(true),
            dropped: AtomicBool::new(false),
            cancels: AtomicU64::new(0),
            link: std::sync::Mutex::new(None),
        }
    }

    /// Stop whatever is talking to Jade: close its link so a waiting call
    /// fails at once, and don't let it reconnect.
    async fn cancel(&self) {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        let link = self.link.lock().ok().and_then(|mut l| l.take());
        if let Some(c) = link {
            c.close().await;
        }
    }

    fn touch(&self) {
        if let Ok(mut t) = self.last_used.lock() {
            *t = Instant::now();
        }
    }

    fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or(Duration::MAX)
    }

    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    fn set_foreground(&self, on_screen: bool) {
        self.foreground.store(on_screen, Ordering::SeqCst);
    }

    /// Close the link (if any). `lost`: it dropped on its own.
    async fn close_link(&self, slot: &mut Option<Conn>, lost: bool) {
        if let Ok(mut l) = self.link.lock() {
            *l = None;
        }
        if let Some(c) = slot.take() {
            c.client.close().await;
        }
        if lost {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
}

/// Whether the keepalive should ping now: on screen, and Jade used recently.
fn keepalive_due(foreground: bool, idle: Duration) -> bool {
    foreground && idle < KEEPALIVE_FOR
}

/// Pings Jade every [`KEEPALIVE_EVERY`] while [`keepalive_due`], for as long
/// as link `id` is the current one. A ping Jade doesn't answer drops the
/// link, so the next call reconnects.
async fn keepalive(app: AppHandle, id: u64) {
    loop {
        tokio::time::sleep(KEEPALIVE_EVERY).await;
        let jade = app.state::<JadeState>();
        // Busy: a call is talking to Jade, which is traffic enough.
        let Ok(mut slot) = jade.conn.try_lock() else {
            continue;
        };
        let (client, up) = match slot.as_ref() {
            Some(c) if c.id == id => (c.client.clone(), c.client.is_up()),
            _ => return,
        };
        if !up {
            tracing::info!("jade: link {id} dropped");
            jade.close_link(&mut slot, true).await;
            return;
        }
        if !keepalive_due(jade.foreground.load(Ordering::SeqCst), jade.idle_for()) {
            continue;
        }
        if let Err(e) = client.ping(PING_WAIT).await {
            tracing::info!("jade: keepalive got no answer ({e:?}); dropping link {id}");
            jade.close_link(&mut slot, true).await;
            return;
        }
    }
}

impl Default for JadeState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Serialize)]
struct Status {
    state: &'static str,
}

#[derive(Clone, Serialize)]
struct Connected {}

fn status(app: &AppHandle, state: &'static str) {
    let _ = app.emit("jade://status", Status { state });
}

// --- errors -----------------------------------------------------------------

fn not_found() -> Error {
    Error::Invalid(
        if cfg!(any(target_os = "android", target_os = "ios")) {
            "Turn on Jade and keep it close (choose Bluetooth on Jade if it asks how to connect), then try again"
        } else {
            "Plug Jade in with a USB cable, or turn it on and keep it close for Bluetooth, then try again"
        }
        .into(),
    )
}

fn turn_on_bluetooth() -> Error {
    Error::Invalid("Turn on Bluetooth, then try again".into())
}

fn allow_bluetooth() -> Error {
    Error::Invalid(
        "Allow Bluetooth access for Sessions Wallet in your system settings, then try again".into(),
    )
}

/// Linux leaves pairing to the desktop's Bluetooth settings.
#[cfg(any(not(any(target_os = "android", target_os = "ios")), test))]
const LINUX_PAIR_HINT: &str = "Jade and this computer aren't paired, or the pairing is out of \
date (Jade was reset or forgot it). Open your system's Bluetooth settings, remove Jade if it's listed, \
add it again and confirm the same number on Jade's screen. Then try again";

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn pair_first() -> Error {
    Error::Invalid(if cfg!(target_os = "linux") {
        LINUX_PAIR_HINT.into()
    } else {
        "Jade doesn't recognise this computer's pairing, or pairing didn't finish. Remove Jade \
         from your Bluetooth settings if it's listed, then try again and confirm the same number on Jade"
            .into()
    })
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn forget_and_repair() -> Error {
    Error::Invalid(
        "Jade doesn't recognise this phone's pairing. Forget Jade in your Bluetooth settings, then try again and confirm the number on Jade"
            .into(),
    )
}

fn lost_connection() -> Error {
    Error::Invalid("Lost the connection to Jade. Keep it on and close by, then try again".into())
}

fn timed_out() -> Error {
    Error::Invalid("Jade didn't respond in time. Keep it on and close by, then try again".into())
}

fn update_firmware(min: (u32, u32, u32)) -> Error {
    Error::Invalid(format!(
        "Update your Jade firmware (needs {}.{}.{} or newer) in the Blockstream app, then try again",
        min.0, min.1, min.2
    ))
}

/// Plain English for people holding the device.
fn friendly(f: Fail) -> Error {
    match f {
        Fail::App(e) => e,
        Fail::Link(m) => {
            tracing::warn!("jade: link: {m}");
            lost_connection()
        }
        Fail::Timeout => timed_out(),
        Fail::Rpc { code, message } => Error::Invalid(match code {
            rpc::USER_CANCELLED => "Cancelled on Jade".into(),
            rpc::NETWORK_MISMATCH => {
                "This Jade is set up for a different network than this wallet".into()
            }
            rpc::HW_LOCKED => {
                "Jade is locked or in use over another connection. Restart Jade and try again"
                    .into()
            }
            rpc::UNKNOWN_METHOD => {
                "Update your Jade firmware in the Blockstream app, then try again".into()
            }
            _ => format!("Jade error: {message}"),
        }),
    }
}

// --- connection -------------------------------------------------------------

/// Check the firmware, unlock (PIN on Jade, relayed to its PIN server by
/// `relay`) and read the root fingerprint. `asking_pin` runs when Jade is
/// about to ask for its PIN.
async fn unlock<R, F>(
    client: &rpc::Client,
    network: Network,
    asking_pin: impl Fn(),
    relay: R,
) -> std::result::Result<((u32, u32, u32), Fingerprint), Fail>
where
    R: Fn(Value) -> F,
    F: Future<Output = Result<(String, Value)>>,
{
    // The first write is what makes the OS pair with Jade; on some systems
    // that write gives up while the person is still confirming. Ask again.
    let info = match client.version_info(FIRST_WAIT).await {
        Err(Fail::Link(m)) => {
            tracing::info!("jade: first request failed ({m}), asking again");
            client.version_info(FIRST_WAIT).await?
        }
        other => other?,
    };
    if info.version < MIN_FIRMWARE {
        return Err(update_firmware(MIN_FIRMWARE).into());
    }
    if info.state == "UNINIT" {
        return Err(Error::Invalid(
            "Set up this Jade first (create or restore its wallet on Jade), then try again".into(),
        )
        .into());
    }
    if !info.ready_on(client.interface()) {
        asking_pin();
    }
    let net = rpc::network_name(network);
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    client.auth_user(net, epoch, relay, USER_WAIT).await?;
    let root = client.get_xpub(net, &[], CALL_WAIT).await?;
    let root = Xpub::from_str(&root)
        .map_err(|_| Error::Invalid("Jade returned an unreadable public key".into()))?;
    Ok((info.version, root.fingerprint()))
}

/// Run `op` on the connected Jade, connecting (and unlocking) first if
/// needed. A link that dropped since the last call (Jade turned off, out of
/// range) is reconnected, once, without asking the person to pair again; a
/// Jade that locked itself is unlocked again on the same link.
async fn with_jade<T, F, Fut>(
    app: &AppHandle,
    jade: &JadeState,
    network: Network,
    op: F,
) -> Result<T>
where
    F: Fn(Arc<Session>) -> Fut,
    Fut: Future<Output = std::result::Result<T, Fail>>,
{
    // One conversation at a time. An earlier one that's stuck (a screen
    // closed mid-scan, a link Jade stopped answering) is cut off first.
    let mut slot = match tokio::time::timeout(Duration::from_secs(2), jade.conn.lock()).await {
        Ok(slot) => slot,
        Err(_) => {
            tracing::warn!("jade: an earlier request is still running; stopping it");
            jade.cancel().await;
            tokio::time::timeout(Duration::from_secs(20), jade.conn.lock())
                .await
                .map_err(|_| {
                    Error::Invalid(
                        "Jade is still busy with an earlier request. Turn it off and on again, then try again".into(),
                    )
                })?
        }
    };
    let generation = jade.cancels.load(Ordering::SeqCst);
    let cancelled = || jade.cancels.load(Ordering::SeqCst) != generation;
    jade.touch();
    if slot
        .as_ref()
        .and_then(|c| c.session.as_ref())
        .is_some_and(|s| s.network != network)
    {
        // Jade unlocks for one network at a time: start over.
        jade.close_link(&mut slot, false).await;
    }
    if slot.as_ref().is_some_and(|c| !c.client.is_up()) {
        tracing::info!("jade: link dropped since the last call");
        jade.close_link(&mut slot, true).await;
    }
    let mut retried = false;
    loop {
        let fresh_link = slot.is_none();
        if fresh_link {
            let reconnecting = jade.dropped.swap(false, Ordering::SeqCst);
            status(
                app,
                if reconnecting {
                    "reconnecting"
                } else {
                    "scanning"
                },
            );
            let link = open_link(app).await?;
            if cancelled() {
                return Err(Error::Invalid("Stopped".into()));
            }
            let id = jade.next_id.fetch_add(1, Ordering::Relaxed);
            let client = Arc::new(rpc::Client::new(link));
            if let Ok(mut l) = jade.link.lock() {
                *l = Some(client.clone());
            }
            *slot = Some(Conn {
                id,
                client,
                session: None,
            });
            tauri::async_runtime::spawn(keepalive(app.clone(), id));
        }
        let conn = slot.as_mut().expect("connected above");
        let session = match conn.session.clone() {
            Some(s) => s,
            None => {
                let client = conn.client.clone();
                let unlocked = unlock(
                    &client,
                    network,
                    || status(app, "unlock"),
                    |req| async move { relay(&req).await },
                )
                .await;
                match unlocked {
                    Ok((version, fingerprint)) => {
                        let _ = app.emit("jade://connected", Connected {});
                        let s = Arc::new(Session {
                            client,
                            network,
                            fingerprint,
                            version,
                        });
                        conn.session = Some(s.clone());
                        s
                    }
                    Err(Fail::Link(m)) => {
                        tracing::info!("jade: link failed while unlocking ({m})");
                        jade.close_link(&mut slot, true).await;
                        if cancelled() {
                            return Err(Error::Invalid("Stopped".into()));
                        }
                        if !fresh_link && !retried {
                            retried = true;
                            continue;
                        }
                        return Err(lost_connection());
                    }
                    Err(e) => {
                        if matches!(e, Fail::Timeout) {
                            // Jade is still on its PIN screen: hang up so it gives up too.
                            jade.close_link(&mut slot, false).await;
                        }
                        return Err(friendly(e));
                    }
                }
            }
        };
        match op(session.clone()).await {
            Ok(v) => {
                jade.touch();
                return Ok(v);
            }
            Err(Fail::Link(m)) => {
                tracing::info!("jade: link failed during a call ({m})");
                jade.close_link(&mut slot, true).await;
                if cancelled() {
                    return Err(Error::Invalid("Stopped".into()));
                }
                if !fresh_link && !retried {
                    retried = true;
                    continue;
                }
                return Err(lost_connection());
            }
            Err(Fail::Rpc {
                code: rpc::HW_LOCKED,
                ..
            }) if !retried => {
                // Jade locked itself (from its menu, say): unlock again on this link.
                tracing::info!("jade: locked; unlocking again");
                if let Some(c) = slot.as_mut() {
                    c.session = None;
                }
                retried = true;
            }
            Err(Fail::Timeout) => {
                // Nobody confirmed in time. Jade answers pings even mid-request:
                // if it does, keep the link (and the unlock); its screen still
                // shows the request, to confirm or cancel there.
                if session.client.ping(PING_WAIT).await.is_err() {
                    jade.close_link(&mut slot, true).await;
                }
                return Err(timed_out());
            }
            Err(e) => return Err(friendly(e)),
        }
    }
}

/// Stop whatever the app is asking Jade (the screen was closed, or it's stuck).
#[tauri::command]
pub async fn jade_cancel(jade: State<'_, JadeState>) -> Result<()> {
    jade.cancel().await;
    Ok(())
}

/// The wallet's network, or the build's default before a wallet exists.
async fn network(state: &AppState) -> Network {
    state.network().await.unwrap_or(NETWORK)
}

fn taproot_firmware() -> Error {
    Error::Invalid(format!(
        "Update your Jade firmware (needs {}.{}.{} or newer for Taproot) in the Blockstream app, then try again",
        TAPROOT_FIRMWARE.0, TAPROOT_FIRMWARE.1, TAPROOT_FIRMWARE.2
    ))
}

/// `m/purpose'/coin'/account'`.
fn account_path(script: ScriptKind, network: Network, account: u32) -> Vec<u32> {
    let coin = if network == Network::Bitcoin { 0 } else { 1 };
    vec![
        script.purpose() | HARDENED,
        coin | HARDENED,
        account | HARDENED,
    ]
}

/// Copy Jade's signatures into our PSBT, after checking it signed the same
/// transaction.
fn merge_signatures(ours: &mut Psbt, theirs: &Psbt) -> Result<()> {
    if theirs.unsigned_tx != ours.unsigned_tx || theirs.inputs.len() != ours.inputs.len() {
        return Err(Error::Invalid(
            "Jade returned a different transaction than the one sent. Don't use it".into(),
        ));
    }
    for (mine, signed) in ours.inputs.iter_mut().zip(&theirs.inputs) {
        mine.partial_sigs
            .extend(signed.partial_sigs.iter().map(|(k, v)| (*k, *v)));
        if signed.tap_key_sig.is_some() {
            mine.tap_key_sig = signed.tap_key_sig;
        }
    }
    Ok(())
}

// --- commands ---------------------------------------------------------------

/// One descriptor per account number, read from Jade over Bluetooth:
/// `wpkh([fp/84h/0h/{n}h]xpub…/<0;1>/*)` (`tr`, `86h` for Taproot).
#[tauri::command]
pub async fn jade_descriptors(
    app: AppHandle,
    state: State<'_, AppState>,
    jade: State<'_, JadeState>,
    script: String,
    accounts: Vec<u32>,
) -> Result<Vec<String>> {
    let script = ScriptKind::parse(&script)?;
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    if accounts.len() > 20 {
        return Err(Error::Invalid(
            "Ask for at most 20 accounts at a time".into(),
        ));
    }
    if accounts.iter().any(|n| *n >= HARDENED) {
        return Err(Error::Invalid("That account number is too large".into()));
    }
    let network = network(&state).await;
    let accounts = Arc::new(accounts);
    with_jade(&app, &jade, network, |s| {
        let accounts = accounts.clone();
        async move {
            if script == ScriptKind::Tr && s.version < TAPROOT_FIRMWARE {
                return Err(taproot_firmware().into());
            }
            let net = rpc::network_name(network);
            let fp = s.fingerprint.to_string();
            let mut out = Vec::with_capacity(accounts.len());
            for n in accounts.iter() {
                let path = account_path(script, network, *n);
                let xpub = s.client.get_xpub(net, &path, CALL_WAIT).await?;
                out.push(helpers::descriptor_on(
                    &JADE, script, network, &fp, *n, &xpub,
                )?);
            }
            Ok(out)
        }
    })
    .await
}

/// Sign an unsigned PSBT (base64) on Jade over Bluetooth. Returns the PSBT
/// with Jade's signatures, ready for `resume`.
///
/// With `account` set to a multisig vault, the vault is registered on Jade
/// first if needed, and the PSBT comes back with Jade's signatures only:
/// other cosigners may still need to sign.
#[tauri::command]
pub async fn jade_sign(
    app: AppHandle,
    state: State<'_, AppState>,
    jade: State<'_, JadeState>,
    psbt: String,
    account: Option<AccountId>,
) -> Result<String> {
    let unsigned = Psbt::from_str(psbt.trim())
        .map_err(|_| Error::Invalid("That isn't a transaction Jade can sign".into()))?;
    if let Some(id) = &account {
        if let Some(vault) = find_vault(&state, id).await? {
            let network = vault.network;
            let vault = Arc::new(vault);
            let ui = app.clone();
            return with_jade(&app, &jade, network, |s| {
                let (vault, ui, psbt) = (vault.clone(), ui.clone(), unsigned.clone());
                async move {
                    let signed = sign_vault_psbt(
                        &s.client,
                        network,
                        s.fingerprint,
                        &vault.name,
                        &vault.config,
                        psbt,
                        || status(&ui, "confirm"),
                    )
                    .await?;
                    Ok(signed.to_string())
                }
            })
            .await;
        }
    }
    let network = network(&state).await;
    let ui = app.clone();
    with_jade(&app, &jade, network, |s| {
        let mut psbt = unsigned.clone();
        let ui = ui.clone();
        async move {
            helpers::check_psbt_on(&JADE, &psbt, s.fingerprint)?;
            let taproot = psbt.inputs.iter().any(|i| i.tap_internal_key.is_some());
            if taproot && s.version < TAPROOT_FIRMWARE {
                return Err(taproot_firmware().into());
            }
            status(&ui, "confirm");
            let signed = s
                .client
                .sign_psbt(rpc::network_name(network), psbt.serialize(), USER_WAIT)
                .await?;
            let signed = Psbt::deserialize(&signed)
                .map_err(|_| Error::Invalid("Jade sent back an unreadable transaction".into()))?;
            merge_signatures(&mut psbt, &signed)?;
            if !helpers::fully_signed(&psbt) {
                return Err(Error::Invalid("Jade didn't sign every input".into()).into());
            }
            Ok(psbt.to_string())
        }
    })
    .await
}

/// Show receive address `index` of a hardware account on Jade, after
/// checking it matches the address this wallet derives. Returns the address.
#[tauri::command]
pub async fn jade_show_address(
    app: AppHandle,
    state: State<'_, AppState>,
    jade: State<'_, JadeState>,
    account: AccountId,
    index: u32,
) -> Result<String> {
    let unsupported = || Error::Unsupported("show its addresses on a Jade".into());
    let (pair, network) = {
        let guard = state.inner.lock().await;
        let u = guard.as_ref().ok_or(Error::Locked)?;
        let spec = u
            .config
            .accounts
            .iter()
            .find(|a| a.id() == &account)
            .ok_or_else(|| Error::AccountNotFound(account.0.clone()))?;
        match spec {
            AccountSpec::Hardware { descriptors, .. } => (descriptors.clone(), u.config.network),
            _ => return Err(unsupported()),
        }
    };
    let path = helpers::receive_path(&pair.external, index).map_err(|e| match e {
        Error::Unsupported(_) => unsupported(),
        e => e,
    })?;
    let expected = descriptors::address_at(&pair, network, index)?;
    let variant = match path.script {
        ScriptKind::Wpkh => "wpkh(k)",
        ScriptKind::Tr => "tr(k)",
    };
    let path = Arc::new(path);
    let ui = app.clone();
    with_jade(&app, &jade, network, |s| {
        let ui = ui.clone();
        let expected = expected.clone();
        let path = path.clone();
        async move {
            if s.fingerprint != path.fingerprint {
                return Err(Error::Invalid(
                    "This Jade doesn't hold this account's keys. Connect the Jade you added it with"
                        .into(),
                )
                .into());
            }
            if path.script == ScriptKind::Tr && s.version < TAPROOT_FIRMWARE {
                return Err(taproot_firmware().into());
            }
            status(&ui, "confirm");
            let shown = s
                .client
                .receive_address(
                    rpc::network_name(network),
                    variant,
                    &path.keypath,
                    USER_WAIT,
                )
                .await?;
            if shown != expected {
                return Err(Error::Invalid(
                    "Jade showed a different address than this wallet. Don't use it".into(),
                )
                .into());
            }
            Ok(shown)
        }
    })
    .await
}

// --- multisig vaults --------------------------------------------------------
//
// Jade signs any input it holds a key for, but it only recognises a vault's
// change, and shows its addresses, once the vault is registered (the person
// confirms the quorum and keys on Jade). Registrations are named (at most 15
// characters, no spaces) and Jade keeps up to 16. Jade overwrites a
// registration with the same name, so a name another wallet already uses
// gets a suffix. Seeing whether the vault is already registered needs
// firmware 1.0.23+ (`get_registered_multisig`); on older firmware a summary
// match under the vault's own name counts.

/// Jade's name for a vault's script type.
fn jade_variant(script: ScriptType) -> &'static str {
    match script {
        ScriptType::P2wsh => "wsh(multi(k))",
        ScriptType::P2shP2wsh => "sh(wsh(multi(k)))",
        ScriptType::P2sh => "sh(multi(k))",
    }
}

/// The vault as Jade registers it.
fn jade_multisig(config: &MultisigConfig) -> Result<rpc::Multisig> {
    let Policy::Multi { threshold, sorted } = config.policy;
    let signers = config
        .cosigners
        .iter()
        .map(|c| {
            let fp =
                Fingerprint::from_str(&c.fingerprint).map_err(|e| Error::Crypto(e.to_string()))?;
            let derivation = DerivationPath::from_str(&c.path)
                .map_err(|e| Error::Crypto(e.to_string()))?
                .into_iter()
                .map(|n| u32::from(*n))
                .collect();
            Ok(rpc::Signer {
                fingerprint: fp.to_bytes(),
                derivation,
                xpub: c.xpub.clone(),
                path: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(rpc::Multisig {
        variant: jade_variant(config.script).into(),
        sorted,
        threshold: threshold.into(),
        signers,
    })
}

/// Whether two registrations are the same wallet: same script, quorum and
/// keys (in any order when the keys are sorted).
fn same_multisig(a: &rpc::Multisig, b: &rpc::Multisig) -> bool {
    let keys = |m: &rpc::Multisig| {
        let mut keys: Vec<(String, [u8; 4], Vec<u32>)> = m
            .signers
            .iter()
            .map(|s| {
                let key = Xpub::from_str(&s.xpub)
                    .map(|x| format!("{}{}", x.public_key, x.chain_code))
                    .unwrap_or_else(|_| s.xpub.clone());
                (key, s.fingerprint, s.path.clone())
            })
            .collect();
        if m.sorted {
            keys.sort();
        }
        keys
    };
    a.variant == b.variant
        && a.sorted == b.sorted
        && a.threshold == b.threshold
        && keys(a) == keys(b)
}

/// Make sure Jade holds one of the vault's keys and knows the vault,
/// registering it if needed (`confirming` runs first: the person confirms on
/// Jade). Returns the registration's name and whether it was registered now.
async fn ensure_multisig(
    client: &rpc::Client,
    network: Network,
    fingerprint: Fingerprint,
    vault_name: &str,
    config: &MultisigConfig,
    confirming: impl Fn(),
) -> std::result::Result<(String, bool), Fail> {
    let net = rpc::network_name(network);
    // Jade's key, not just its fingerprint.
    let mut ours = false;
    for key in helpers::vault_keys(&JADE, config, fingerprint)? {
        let xpub = client.get_xpub(net, &key.keypath, CALL_WAIT).await?;
        if Xpub::from_str(&xpub).is_ok_and(|x| helpers::same_key(&x, &key.xpub)) {
            ours = true;
            break;
        }
    }
    if !ours {
        return Err(helpers::not_a_vault_key(&JADE).into());
    }
    let wanted = jade_multisig(config)?;
    let own_name = helpers::jade_vault_name(vault_name, &[]);
    let existing = client.registered_multisigs(CALL_WAIT).await?;
    for (name, summary) in &existing {
        if summary.variant != wanted.variant
            || summary.sorted != wanted.sorted
            || summary.threshold != wanted.threshold
            || summary.num_signers as usize != wanted.signers.len()
        {
            continue;
        }
        match client.registered_multisig(name, CALL_WAIT).await {
            Ok(m) if same_multisig(&m, &wanted) => return Ok((name.clone(), false)),
            Ok(_) => {}
            // Older firmware (or a registration it made): can't see the keys.
            Err(Fail::Rpc { .. } | Fail::App(_)) if *name == own_name => {
                return Ok((name.clone(), false))
            }
            Err(Fail::Rpc { .. } | Fail::App(_)) => {}
            Err(e) => return Err(e),
        }
    }
    let taken: Vec<String> = existing.into_iter().map(|(n, _)| n).collect();
    let name = helpers::jade_vault_name(vault_name, &taken);
    confirming();
    client
        .register_multisig(net, &name, &wanted, USER_WAIT)
        .await?;
    Ok((name, true))
}

/// Sign a vault PSBT on Jade, registering the vault first if needed.
/// Returns the PSBT with Jade's signatures added (not finalized).
async fn sign_vault_psbt(
    client: &rpc::Client,
    network: Network,
    fingerprint: Fingerprint,
    vault_name: &str,
    config: &MultisigConfig,
    mut psbt: Psbt,
    confirming: impl Fn(),
) -> std::result::Result<Psbt, Fail> {
    helpers::check_multisig_psbt(&JADE, &psbt, fingerprint)?;
    ensure_multisig(
        client,
        network,
        fingerprint,
        vault_name,
        config,
        &confirming,
    )
    .await?;
    confirming();
    let signed = client
        .sign_psbt(rpc::network_name(network), psbt.serialize(), USER_WAIT)
        .await?;
    let signed = Psbt::deserialize(&signed)
        .map_err(|_| Error::Invalid("Jade sent back an unreadable transaction".into()))?;
    merge_signatures(&mut psbt, &signed)?;
    if !helpers::signed_own_inputs(&psbt, fingerprint) {
        return Err(Error::Invalid("Jade didn't sign this vault's inputs".into()).into());
    }
    Ok(psbt)
}

/// Jade's key for a multisig vault (BIP48 native SegWit):
/// `[fingerprint/48h/0h/{account}h/2h]xpub…`.
#[tauri::command]
pub async fn jade_multisig_key(
    app: AppHandle,
    state: State<'_, AppState>,
    jade: State<'_, JadeState>,
    account: u32,
) -> Result<String> {
    let network = network(&state).await;
    let path = Arc::new(helpers::multisig_account_keypath(network, account)?);
    with_jade(&app, &jade, network, |s| {
        let path = path.clone();
        async move {
            let xpub = s
                .client
                .get_xpub(rpc::network_name(network), &path, CALL_WAIT)
                .await?;
            Ok(helpers::multisig_key_on(
                &JADE,
                network,
                &s.fingerprint.to_string(),
                account,
                &xpub,
            )?)
        }
    })
    .await
}

/// Register multisig vault `account` on Jade, unless it already is. Returns
/// whether it was registered now (the person confirmed on Jade).
#[tauri::command]
pub async fn jade_register_multisig(
    app: AppHandle,
    state: State<'_, AppState>,
    jade: State<'_, JadeState>,
    account: AccountId,
) -> Result<bool> {
    let vault = Arc::new(vault(&state, &account).await?);
    let network = vault.network;
    let ui = app.clone();
    with_jade(&app, &jade, network, |s| {
        let (vault, ui) = (vault.clone(), ui.clone());
        async move {
            let (_, registered) = ensure_multisig(
                &s.client,
                network,
                s.fingerprint,
                &vault.name,
                &vault.config,
                || status(&ui, "confirm"),
            )
            .await?;
            Ok(registered)
        }
    })
    .await
}

/// Show receive address `index` of multisig vault `account` on Jade
/// (registering the vault first if needed), after checking it matches the
/// address this wallet derives. Returns the address.
#[tauri::command]
pub async fn jade_show_multisig_address(
    app: AppHandle,
    state: State<'_, AppState>,
    jade: State<'_, JadeState>,
    account: AccountId,
    index: u32,
) -> Result<String> {
    if index >= HARDENED {
        return Err(Error::Invalid("That address number is too large".into()));
    }
    let vault = Arc::new(vault(&state, &account).await?);
    let network = vault.network;
    let expected = descriptors::address_at(&vault.descriptors, network, index)?;
    let ui = app.clone();
    with_jade(&app, &jade, network, |s| {
        let (vault, ui, expected) = (vault.clone(), ui.clone(), expected.clone());
        async move {
            let (name, _) = ensure_multisig(
                &s.client,
                network,
                s.fingerprint,
                &vault.name,
                &vault.config,
                || status(&ui, "confirm"),
            )
            .await?;
            status(&ui, "confirm");
            let paths = vec![vec![0, index]; vault.config.cosigners.len()];
            let shown = s
                .client
                .multisig_receive_address(rpc::network_name(network), &name, &paths, USER_WAIT)
                .await?;
            if shown != expected {
                return Err(Error::Invalid(
                    "Jade showed a different address than this wallet. Don't use it".into(),
                )
                .into());
            }
            Ok(shown)
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_jade_request_and_builds_reply() {
        let msg = json!({
            "id": "0",
            "result": { "http_request": {
                "params": { "urls": ["https://j8d.io/get_pin", "http://abc.onion/get_pin"], "method": "POST", "accept": "json", "data": { "data": "xyz" } },
                "on-reply": "pin"
            }}
        });
        let back = from_cbor(&to_cbor(&msg)).unwrap();
        let req = http_request(&back).unwrap();
        assert_eq!(pick_url(&req["params"]).unwrap(), "https://j8d.io/get_pin");
        assert_eq!(req["on-reply"], "pin");

        let reply = reply_message("pin", json!({ "data": "server" }));
        assert_eq!(reply["method"], "pin");
        assert_eq!(reply["params"]["data"], "server");
        assert!(!reply["id"].as_str().unwrap().is_empty());
    }

    // --- Bluetooth ---------------------------------------------------------

    use super::rpc::tests::{method, reply, FakeJade};
    use ciborium::Value as Cbor;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn cbor(v: Value) -> Cbor {
        Cbor::serialized(&v).unwrap()
    }

    /// A Jade holding the "abandon … about" seed, locked, on `version`.
    fn fake_jade(version: &'static str, state: &'static str) -> FakeJade {
        use bitcoin::bip32::{ChildNumber, DerivationPath};
        let master = sessions_core::keystore::Seed::from_phrase(ABANDON)
            .unwrap()
            .master_xpriv(Network::Bitcoin)
            .unwrap();
        FakeJade::new(move |req| {
            let json = serde_json::to_value(req).unwrap();
            match method(req) {
                "get_version_info" => vec![reply(
                    req,
                    cbor(
                        json!({ "JADE_VERSION": version, "JADE_STATE": state, "JADE_INTERFACE": "NONE" }),
                    ),
                )],
                "auth_user" => {
                    assert_eq!(json["params"]["network"], "mainnet");
                    vec![reply(
                        req,
                        cbor(json!({ "http_request": {
                            "params": { "urls": ["https://j8d.io/get_pin"], "method": "POST", "accept": "json", "data": { "data": "x" } },
                            "on-reply": "pin"
                        }})),
                    )]
                }
                "pin" => {
                    assert_eq!(json["params"]["data"], "from-server");
                    vec![reply(req, Cbor::Bool(true))]
                }
                "get_xpub" => {
                    let path: DerivationPath = json["params"]["path"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|n| ChildNumber::from(n.as_u64().unwrap() as u32))
                        .collect();
                    let secp = bitcoin::secp256k1::Secp256k1::new();
                    let key = master.derive_priv(&secp, &path).unwrap();
                    let xpub = Xpub::from_priv(&secp, &key);
                    vec![reply(req, Cbor::Text(xpub.to_string()))]
                }
                m => panic!("unexpected {m}"),
            }
        })
    }

    #[tokio::test]
    async fn unlocks_over_bluetooth_and_reads_descriptors() {
        let client = rpc::Client::new(Box::new(fake_jade("1.0.41", "LOCKED")));
        let asked = AtomicBool::new(false);
        let relayed = AtomicUsize::new(0);
        let (version, fp) = unlock(
            &client,
            Network::Bitcoin,
            || asked.store(true, Ordering::SeqCst),
            |req| {
                let relayed = &relayed;
                async move {
                    relayed.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(req["on-reply"], "pin");
                    Ok(("pin".to_string(), json!({ "data": "from-server" })))
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(version, (1, 0, 41));
        assert_eq!(fp.to_string(), "73c5da0a");
        assert!(asked.load(Ordering::SeqCst));
        assert_eq!(relayed.load(Ordering::SeqCst), 1);

        let path = account_path(ScriptKind::Wpkh, Network::Bitcoin, 0);
        let xpub = client.get_xpub("mainnet", &path, CALL_WAIT).await.unwrap();
        let d = helpers::descriptor_on(
            &JADE,
            ScriptKind::Wpkh,
            Network::Bitcoin,
            &fp.to_string(),
            0,
            &xpub,
        )
        .unwrap();
        assert_eq!(d, "wpkh([73c5da0a/84h/0h/0h]xpub6CatWdiZiodmUeTDp8LT5or8nmbKNcuyvz7WyksVFkKB4RHwCD3XyuvPEbvqAQY3rAPshWcMLoP2fMFMKHPJ4ZeZXYVUhLv1VMrjPC7PW6V/<0;1>/*)");
        let imported = descriptors::import(&d, Network::Bitcoin).unwrap();
        assert_eq!(
            descriptors::address_at(&imported, Network::Bitcoin, 0).unwrap(),
            "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu"
        );
    }

    #[tokio::test]
    async fn old_or_empty_jades_are_turned_away() {
        let no_relay = |_: Value| async { Err::<(String, Value), _>(Error::Invalid("no".into())) };
        let old = rpc::Client::new(Box::new(fake_jade("0.1.46", "LOCKED")));
        let e = friendly(
            unlock(&old, Network::Bitcoin, || {}, no_relay)
                .await
                .unwrap_err(),
        );
        assert!(e
            .to_string()
            .starts_with("Update your Jade firmware (needs 0.1.47"));
        let empty = rpc::Client::new(Box::new(fake_jade("1.0.41", "UNINIT")));
        let e = friendly(
            unlock(&empty, Network::Bitcoin, || {}, no_relay)
                .await
                .unwrap_err(),
        );
        assert!(e.to_string().starts_with("Set up this Jade first"));
    }

    #[test]
    fn bluetooth_errors_are_plain() {
        let rpc_err = |code| {
            friendly(Fail::Rpc {
                code,
                message: "x".into(),
            })
            .to_string()
        };
        assert_eq!(rpc_err(rpc::USER_CANCELLED), "Cancelled on Jade");
        assert!(rpc_err(rpc::UNKNOWN_METHOD).starts_with("Update your Jade firmware"));
        assert!(rpc_err(rpc::NETWORK_MISMATCH).contains("different network"));
        assert_eq!(rpc_err(-1), "Jade error: x");
        assert!(friendly(Fail::Link("gone".into()))
            .to_string()
            .starts_with("Lost the connection to Jade"));
        assert!(friendly(Fail::Timeout)
            .to_string()
            .starts_with("Jade didn't respond in time"));
        assert!(taproot_firmware().to_string().contains("1.0.34"));
        assert!(LINUX_PAIR_HINT.contains("Bluetooth settings"));
        assert_eq!(
            account_path(ScriptKind::Tr, Network::Testnet, 2),
            vec![86 | HARDENED, 1 | HARDENED, 2 | HARDENED]
        );
    }

    #[test]
    fn keepalive_pings_only_while_in_use_and_on_screen() {
        assert!(keepalive_due(true, Duration::ZERO));
        assert!(keepalive_due(true, KEEPALIVE_FOR - Duration::from_secs(1)));
        // Idle a while: let Jade's own idle timeout decide again.
        assert!(!keepalive_due(true, KEEPALIVE_FOR));
        // In the background: no pings.
        assert!(!keepalive_due(false, Duration::ZERO));
        // Several pings fit in the window, and a ping answers well within a period.
        assert!(KEEPALIVE_FOR >= KEEPALIVE_EVERY * 4);
        assert!(PING_WAIT < KEEPALIVE_EVERY);
    }

    #[test]
    fn idle_time_counts_from_last_use() {
        let jade = JadeState::new();
        *jade.last_used.lock().unwrap() = Instant::now() - Duration::from_secs(600);
        assert!(jade.idle_for() >= Duration::from_secs(600));
        jade.touch();
        assert!(jade.idle_for() < Duration::from_secs(5));
        jade.set_foreground(false);
        assert!(!jade.foreground.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn ping_counts_any_answer() {
        let client = rpc::Client::new(Box::new(rpc::tests::FakeJade::new(|req| {
            assert_eq!(method(req), "ping");
            vec![reply(req, Cbor::Integer(0.into()))]
        })));
        client.ping(PING_WAIT).await.unwrap();
        assert!(client.is_up());
    }

    #[test]
    fn merges_only_signatures_for_the_same_transaction() {
        use bitcoin::secp256k1::{Message, Secp256k1, SecretKey};
        use bitcoin::{absolute, transaction, Amount, ScriptBuf, Transaction, TxIn, TxOut};
        let tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let ours = Psbt::from_unsigned_tx(tx).unwrap();
        let secp = Secp256k1::new();
        let sk = SecretKey::from_slice(&[1; 32]).unwrap();
        let pk = bitcoin::PublicKey::new(sk.public_key(&secp));
        let sig = bitcoin::ecdsa::Signature::sighash_all(
            secp.sign_ecdsa(&Message::from_digest([2; 32]), &sk),
        );
        let mut theirs = ours.clone();
        theirs.inputs[0].partial_sigs.insert(pk, sig);

        let mut merged = ours.clone();
        merge_signatures(&mut merged, &theirs).unwrap();
        assert!(helpers::fully_signed(&merged));
        assert_eq!(merged.inputs[0].partial_sigs.get(&pk), Some(&sig));

        let mut other = theirs.clone();
        other.unsigned_tx.output[0].value = Amount::from_sat(999);
        let mut untouched = ours.clone();
        assert!(merge_signatures(&mut untouched, &other).is_err());
        assert!(!helpers::fully_signed(&untouched));
    }

    // --- multisig ---------------------------------------------------------

    const LEGAL: &str =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";
    const LETTER: &str =
        "letter advice cage absurd amount doctor acoustic avoid letter advice cage above";

    fn master(phrase: &str) -> bitcoin::bip32::Xpriv {
        sessions_core::keystore::Seed::from_phrase(phrase)
            .unwrap()
            .master_xpriv(Network::Bitcoin)
            .unwrap()
    }

    /// A 2-of-3 `wsh(sortedmulti)` vault of three test seeds.
    fn test_vault() -> MultisigConfig {
        let keys: Vec<String> = [ABANDON, LEGAL, LETTER]
            .iter()
            .map(|p| descriptors::multisig_cosigner_key(&master(p), Network::Bitcoin, 0).unwrap())
            .collect();
        let pair = descriptors::multisig(2, &keys).unwrap();
        MultisigConfig::from_descriptor(&pair.external, Network::Bitcoin).unwrap()
    }

    fn field<'a>(v: &'a Cbor, key: &str) -> &'a Cbor {
        v.as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_text() == Some(key))
            .map(|(_, v)| v)
            .unwrap_or_else(|| panic!("no {key}"))
    }

    /// Jade with the "abandon … about" seed, its multisig registrations in
    /// `registry`; `sign_psbt` adds a signature for its key in input 0
    /// (unless `signs` is false).
    fn vault_jade(registry: Arc<std::sync::Mutex<Vec<(String, Cbor)>>>, signs: bool) -> FakeJade {
        use bitcoin::bip32::{ChildNumber, DerivationPath};
        let master = master(ABANDON);
        FakeJade::new(move |req| {
            let json = serde_json::to_value(req).unwrap();
            match method(req) {
                "get_xpub" => {
                    let path: DerivationPath = json["params"]["path"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|n| ChildNumber::from(n.as_u64().unwrap() as u32))
                        .collect();
                    let secp = bitcoin::secp256k1::Secp256k1::new();
                    let xpub = Xpub::from_priv(&secp, &master.derive_priv(&secp, &path).unwrap());
                    vec![reply(req, Cbor::Text(xpub.to_string()))]
                }
                "get_registered_multisigs" => {
                    let entries = registry
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|(name, d)| {
                            let summary = vec![
                                (Cbor::Text("variant".into()), field(d, "variant").clone()),
                                (Cbor::Text("sorted".into()), field(d, "sorted").clone()),
                                (
                                    Cbor::Text("threshold".into()),
                                    field(d, "threshold").clone(),
                                ),
                                (
                                    Cbor::Text("num_signers".into()),
                                    Cbor::Integer(
                                        (field(d, "signers").as_array().unwrap().len() as u64)
                                            .into(),
                                    ),
                                ),
                            ];
                            (Cbor::Text(name.clone()), Cbor::Map(summary))
                        })
                        .collect();
                    vec![reply(req, Cbor::Map(entries))]
                }
                "get_registered_multisig" => {
                    let name = json["params"]["multisig_name"].as_str().unwrap().to_owned();
                    let d = registry
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(n, _)| *n == name)
                        .map(|(_, d)| d.clone())
                        .unwrap();
                    vec![reply(
                        req,
                        Cbor::Map(vec![
                            (Cbor::Text("multisig_name".into()), Cbor::Text(name)),
                            (Cbor::Text("descriptor".into()), d),
                        ]),
                    )]
                }
                "register_multisig" => {
                    assert_eq!(json["params"]["network"], "mainnet");
                    let name = json["params"]["multisig_name"].as_str().unwrap().to_owned();
                    assert!(name.len() <= 15 && !name.contains(' '));
                    let d = field(field(req, "params"), "descriptor").clone();
                    let mut registry = registry.lock().unwrap();
                    registry.retain(|(n, _)| *n != name);
                    registry.push((name, d));
                    vec![reply(req, Cbor::Bool(true))]
                }
                "sign_psbt" => {
                    let bytes = field(field(req, "params"), "psbt").as_bytes().unwrap();
                    let mut psbt = Psbt::deserialize(bytes).unwrap();
                    if signs {
                        let fp = master.fingerprint(&bitcoin::secp256k1::Secp256k1::new());
                        let pk = psbt.inputs[0]
                            .bip32_derivation
                            .iter()
                            .find(|(_, (f, _))| *f == fp)
                            .map(|(pk, _)| *pk)
                            .unwrap();
                        let secp = bitcoin::secp256k1::Secp256k1::new();
                        let sk = bitcoin::secp256k1::SecretKey::from_slice(&[5; 32]).unwrap();
                        let sig =
                            bitcoin::ecdsa::Signature::sighash_all(secp.sign_ecdsa(
                                &bitcoin::secp256k1::Message::from_digest([6; 32]),
                                &sk,
                            ));
                        psbt.inputs[0]
                            .partial_sigs
                            .insert(bitcoin::PublicKey::new(pk), sig);
                    }
                    vec![reply(req, Cbor::Bytes(psbt.serialize()))]
                }
                m => panic!("unexpected {m}"),
            }
        })
    }

    /// A PSBT spending the vault's receive address 0, keys and script filled in.
    fn vault_psbt(config: &MultisigConfig) -> Psbt {
        use bitcoin::bip32::{ChildNumber, DerivationPath};
        use bitcoin::blockdata::opcodes::all::OP_CHECKMULTISIG;
        use bitcoin::blockdata::script::Builder;
        use bitcoin::{absolute, transaction, Amount, ScriptBuf, Transaction, TxIn, TxOut};
        let secp = bitcoin::secp256k1::Secp256k1::new();
        let mut keys: Vec<_> = config
            .cosigners
            .iter()
            .map(|c| {
                let xpub = Xpub::from_str(&c.xpub).unwrap();
                let tail = [ChildNumber::from(0), ChildNumber::from(0)];
                let pk = xpub.derive_pub(&secp, &tail).unwrap().public_key;
                let path = DerivationPath::from_str(&c.path).unwrap().extend(tail);
                (pk, (Fingerprint::from_str(&c.fingerprint).unwrap(), path))
            })
            .collect();
        keys.sort_by_key(|(pk, _)| pk.serialize());
        let mut script = Builder::new().push_int(2);
        for (pk, _) in &keys {
            script = script.push_key(&bitcoin::PublicKey::new(*pk));
        }
        let script = script
            .push_int(keys.len() as i64)
            .push_opcode(OP_CHECKMULTISIG)
            .into_script();
        let tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![TxOut {
                value: Amount::from_sat(90_000),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let mut psbt = Psbt::from_unsigned_tx(tx).unwrap();
        psbt.inputs[0].witness_utxo = Some(TxOut {
            value: Amount::from_sat(100_000),
            script_pubkey: ScriptBuf::new_p2wsh(&script.wscript_hash()),
        });
        psbt.inputs[0].witness_script = Some(script);
        psbt.inputs[0].bip32_derivation = keys.into_iter().collect();
        psbt
    }

    #[tokio::test]
    async fn vaults_are_registered_on_jade_once() {
        let config = test_vault();
        let fp = Fingerprint::from_str("73c5da0a").unwrap();
        let registry = Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = rpc::Client::new(Box::new(vault_jade(registry.clone(), true)));
        let confirms = AtomicUsize::new(0);
        let confirm = || {
            confirms.fetch_add(1, Ordering::SeqCst);
        };

        let (name, registered) = ensure_multisig(
            &client,
            Network::Bitcoin,
            fp,
            "Family vault",
            &config,
            confirm,
        )
        .await
        .unwrap();
        assert_eq!((name.as_str(), registered), ("Family_vault", true));
        assert_eq!(confirms.load(Ordering::SeqCst), 1);
        {
            let registry = registry.lock().unwrap();
            assert_eq!(registry.len(), 1);
            let d = &registry[0].1;
            assert_eq!(field(d, "variant").as_text(), Some("wsh(multi(k))"));
            assert_eq!(field(d, "sorted"), &Cbor::Bool(true));
            assert_eq!(field(d, "threshold"), &Cbor::Integer(2.into()));
            let signers = field(d, "signers").as_array().unwrap();
            assert_eq!(signers.len(), 3);
            for (s, c) in signers.iter().zip(&config.cosigners) {
                assert_eq!(field(s, "xpub").as_text(), Some(c.xpub.as_str()));
                assert_eq!(
                    field(s, "fingerprint").as_bytes().unwrap().as_slice(),
                    Fingerprint::from_str(&c.fingerprint).unwrap().as_bytes()
                );
                assert_eq!(field(s, "derivation").as_array().unwrap().len(), 4);
                assert!(field(s, "path").as_array().unwrap().is_empty());
            }
        }

        // Already there: nothing to confirm.
        let (name, registered) = ensure_multisig(
            &client,
            Network::Bitcoin,
            fp,
            "Family vault",
            &config,
            confirm,
        )
        .await
        .unwrap();
        assert_eq!((name.as_str(), registered), ("Family_vault", false));
        assert_eq!(confirms.load(Ordering::SeqCst), 1);

        // Another vault with the same name doesn't overwrite this one.
        let mut other = config.clone();
        other.policy = Policy::Multi {
            threshold: 1,
            sorted: true,
        };
        let (name, registered) = ensure_multisig(
            &client,
            Network::Bitcoin,
            fp,
            "Family vault",
            &other,
            confirm,
        )
        .await
        .unwrap();
        assert_eq!((name.as_str(), registered), ("Family_vault-2", true));
        assert_eq!(registry.lock().unwrap().len(), 2);

        // A Jade that isn't one of the keys is turned away before anything else.
        let e = ensure_multisig(
            &client,
            Network::Bitcoin,
            Fingerprint::from([1, 2, 3, 4]),
            "Family vault",
            &config,
            confirm,
        )
        .await
        .unwrap_err();
        assert_eq!(
            friendly(e).to_string(),
            "This Jade isn't one of this vault's keys"
        );
    }

    #[tokio::test]
    async fn jade_signs_its_part_of_a_vault_psbt() {
        let config = test_vault();
        let fp = Fingerprint::from_str("73c5da0a").unwrap();
        let registry = Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = rpc::Client::new(Box::new(vault_jade(registry.clone(), true)));
        let psbt = vault_psbt(&config);
        let signed = sign_vault_psbt(&client, Network::Bitcoin, fp, "Vault", &config, psbt, || {})
            .await
            .unwrap();
        // Registered on the way, signed by Jade's key only: not final yet.
        assert_eq!(registry.lock().unwrap().len(), 1);
        assert_eq!(signed.inputs[0].partial_sigs.len(), 1);
        assert!(helpers::signed_own_inputs(&signed, fp));

        // A Jade that comes back without its signature fails plainly.
        let lazy = rpc::Client::new(Box::new(vault_jade(registry, false)));
        let e = sign_vault_psbt(
            &lazy,
            Network::Bitcoin,
            fp,
            "Vault",
            &config,
            vault_psbt(&config),
            || {},
        )
        .await
        .unwrap_err();
        assert_eq!(
            friendly(e).to_string(),
            "Jade didn't sign this vault's inputs"
        );
    }

    #[test]
    fn registrations_compare_by_keys() {
        let config = test_vault();
        let a = jade_multisig(&config).unwrap();
        assert_eq!(a.variant, "wsh(multi(k))");
        let mut b = a.clone();
        b.signers.reverse();
        assert!(same_multisig(&a, &b), "sorted keys: order doesn't matter");
        b.sorted = false;
        let mut c = b.clone();
        assert!(!same_multisig(&a, &b));
        c.signers.reverse();
        let mut a2 = a.clone();
        a2.sorted = false;
        assert!(same_multisig(&a2, &c));
        assert!(!same_multisig(&a2, &b), "unsorted keys: order matters");
        let mut d = a.clone();
        d.threshold = 3;
        assert!(!same_multisig(&a, &d));
    }

    #[test]
    fn rejects_other_qr_codes() {
        assert!(http_request(&json!({ "id": "1", "result": true })).is_err());
        assert!(pick_url(&json!({ "urls": ["http://x.onion/get_pin"] })).is_err());
    }
}

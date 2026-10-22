//! Blockstream Jade: PIN-server relay, QR unlock, and Jade over Bluetooth.
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

use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bitcoin::bip32::{Fingerprint, Xpub};
use bitcoin::Psbt;
use rail_onchain::bitbox::{self as helpers, ScriptKind, JADE};
use rail_onchain::descriptors;
use serde::Serialize;
use serde_json::{json, Value};
use sessions_core::{AccountId, Error, Network, Result};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;

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
}

impl JadeState {
    pub fn new() -> Self {
        JadeState {
            conn: Mutex::new(None),
            next_id: AtomicU64::new(1),
            last_used: std::sync::Mutex::new(Instant::now()),
            foreground: AtomicBool::new(true),
            dropped: AtomicBool::new(false),
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
        "Turn on Jade and keep it close (choose Bluetooth on Jade if it asks how to connect), then try again"
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
    if !info.ready_over_ble() {
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
    let mut slot = jade.conn.lock().await;
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
            let link = ble::open(app).await?;
            let id = jade.next_id.fetch_add(1, Ordering::Relaxed);
            *slot = Some(Conn {
                id,
                client: Arc::new(rpc::Client::new(link)),
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
#[tauri::command]
pub async fn jade_sign(
    app: AppHandle,
    state: State<'_, AppState>,
    jade: State<'_, JadeState>,
    psbt: String,
) -> Result<String> {
    let unsigned = Psbt::from_str(psbt.trim())
        .map_err(|_| Error::Invalid("That isn't a transaction Jade can sign".into()))?;
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

    #[test]
    fn rejects_other_qr_codes() {
        assert!(http_request(&json!({ "id": "1", "result": true })).is_err());
        assert!(pick_url(&json!({ "urls": ["http://x.onion/get_pin"] })).is_err());
    }
}

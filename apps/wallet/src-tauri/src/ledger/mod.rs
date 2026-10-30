//! Ledger hardware wallets (Flex, Nano Gen5, and the others with the
//! Bitcoin app 2.1+), over USB or Bluetooth, on computers and phones.
//!
//! The Bitcoin app speaks "wallet policies": a single-sig account is a
//! standard policy (`wpkh(@0/**)` or `tr(@0/**)` with the account's key)
//! that needs no registration. The app checks change outputs and shows
//! addresses itself; `ledger_bitcoin_client` drives the protocol and checks
//! every address the device returns against its own derivation.
//!
//! Before anything else the Bitcoin app has to be open: from the dashboard
//! it's opened for the person (they confirm on the Ledger); another app is
//! closed first. A locked Ledger is waited for while the screen says so.
//!
//! Progress goes to the UI as `ledger://status` with `{ state }`:
//! "connecting", "bluetooth", "pairing", "unlock", "open-app", "confirm".

mod frame;
mod link;

use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bitcoin::bip32::{ChildNumber, DerivationPath, Fingerprint};
use bitcoin::Psbt;
use ledger_bitcoin_client::apdu::{APDUCommand, StatusWord};
use ledger_bitcoin_client::async_client::{BitcoinClient, Transport};
use ledger_bitcoin_client::error::BitcoinClientError;
use ledger_bitcoin_client::wallet::Version;
use ledger_bitcoin_client::{PartialSignature, SignPsbtYieldedObject, WalletPolicy, WalletPubKey};
use rail_onchain::bitbox::{self as helpers, ScriptKind};
use rail_onchain::descriptors;
use serde::Serialize;
use sessions_core::{AccountId, Error, Network, Result};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Mutex;

use crate::config::AccountSpec;
use crate::state::AppState;
use link::{Fail, Link};

const HARDENED: u32 = 0x8000_0000;
/// Most an operation waits on the person (confirming on the device).
const CONFIRM_WAIT: Duration = Duration::from_secs(300);
/// A request that needs nobody.
const QUICK: Duration = Duration::from_secs(15);
/// Waiting for the person to unlock the Ledger or open the app.
const PERSON_WAIT: Duration = Duration::from_secs(180);

pub struct LedgerState {
    link: Mutex<Option<Arc<dyn Link>>>,
    cancels: AtomicU64,
}

impl LedgerState {
    pub fn new() -> Self {
        LedgerState {
            link: Mutex::new(None),
            cancels: AtomicU64::new(0),
        }
    }
}

#[derive(Clone, Serialize)]
struct Status {
    state: &'static str,
}

fn status(app: &AppHandle, state: &'static str) {
    let _ = app.emit("ledger://status", Status { state });
}

fn cancelled() -> Error {
    Error::Invalid("Cancelled".into())
}

// --- the APDU transport the client uses -----------------------------------------

struct Apdu {
    link: Arc<dyn Link>,
    wait: Duration,
}

#[async_trait]
impl Transport for Apdu {
    type Error = Fail;

    async fn exchange(&self, command: &APDUCommand) -> std::result::Result<(StatusWord, Vec<u8>), Fail> {
        let resp = self.link.exchange(&command.encode(), self.wait).await?;
        let (sw, data) = frame::split_status(&resp).ok_or_else(|| Fail::Link("empty response".into()))?;
        match StatusWord::try_from(sw) {
            Ok(s) => Ok((s, data.to_vec())),
            Err(_) => Err(Fail::App(status_error(sw))),
        }
    }
}

/// Status words outside the Bitcoin app's own, in plain words.
fn status_error(sw: u16) -> Error {
    match sw {
        0x5515 | 0x6b0c | 0x6982 => Error::Invalid("Unlock your Ledger with its PIN, then try again".into()),
        0x5501 | 0x6985 => Error::Invalid("Cancelled on the Ledger".into()),
        0x6e00 | 0x6e01 | 0x6d00 | 0x6511 | 0x6d02 => {
            Error::Invalid("Open the Bitcoin app on your Ledger, then try again".into())
        }
        0x6807 => Error::Invalid(
            "The Bitcoin app isn't installed on this Ledger. Install it with Ledger Live, then try again".into(),
        ),
        other => Error::Invalid(format!("The Ledger answered with an error ({other:04x})")),
    }
}

fn client_error(e: BitcoinClientError<Fail>) -> Fail {
    match e {
        BitcoinClientError::Transport(f) => f,
        BitcoinClientError::Device { status: StatusWord::Deny, .. } => {
            Fail::App(Error::Invalid("Cancelled on the Ledger".into()))
        }
        BitcoinClientError::Device { status, .. } => {
            Fail::App(Error::Invalid(format!("The Ledger refused the request ({status:?})")))
        }
        BitcoinClientError::UnsupportedAppVersion => Fail::App(update_app()),
        BitcoinClientError::InvalidPsbt => {
            Fail::App(Error::Invalid("The Ledger can't read this transaction".into()))
        }
        BitcoinClientError::InvalidResponse(m) => {
            tracing::warn!("ledger: invalid response: {m}");
            Fail::App(Error::Invalid(
                "The Ledger returned something this wallet didn't expect. Don't use this address or transaction"
                    .into(),
            ))
        }
        other => {
            tracing::warn!("ledger: {other:?}");
            Fail::App(Error::Invalid("The Ledger couldn't do that. Try again".into()))
        }
    }
}

fn update_app() -> Error {
    Error::Invalid("Update the Bitcoin app on your Ledger (needs 2.1 or newer) in Ledger Live, then try again".into())
}

// --- connecting and getting to the Bitcoin app --------------------------------------

/// The open link, or a new one.
async fn connect(app: &AppHandle, st: &LedgerState) -> Result<Arc<dyn Link>> {
    if let Some(l) = st.link.lock().await.clone() {
        return Ok(l);
    }
    status(app, "connecting");
    let a1 = app.clone();
    let a2 = app.clone();
    let link: Arc<dyn Link> = Arc::from(
        link::open(app, move || status(&a1, "bluetooth"), move || status(&a2, "pairing")).await?,
    );
    tracing::info!("ledger: connected over {:?}", link.via());
    *st.link.lock().await = Some(link.clone());
    Ok(link)
}

async fn drop_link(st: &LedgerState) {
    if let Some(l) = st.link.lock().await.take() {
        l.close().await;
    }
}

/// The running app's name and version (`B0 01`), or the status word.
async fn running_app(link: &Arc<dyn Link>) -> std::result::Result<std::result::Result<(String, String), u16>, Fail> {
    let resp = link.exchange(&[0xb0, 0x01, 0x00, 0x00, 0x00], QUICK).await?;
    let (sw, data) = frame::split_status(&resp).ok_or_else(|| Fail::Link("empty response".into()))?;
    if sw != 0x9000 {
        return Ok(Err(sw));
    }
    let field = |at: usize| -> Option<(String, usize)> {
        let len = *data.get(at)? as usize;
        let s = data.get(at + 1..at + 1 + len)?;
        Some((String::from_utf8_lossy(s).into_owned(), at + 1 + len))
    };
    let (name, next) = field(1).ok_or_else(|| Fail::Link("bad app info".into()))?;
    let (version, _) = field(next).unwrap_or_default();
    Ok(Ok((name, version)))
}

fn app_name(network: Network) -> &'static str {
    if network == Network::Bitcoin {
        "Bitcoin"
    } else {
        "Bitcoin Test"
    }
}

/// Version `a.b.c` at least 2.1.0 (wallet policies).
fn new_enough(version: &str) -> bool {
    let mut parts = version.split(|c: char| !c.is_ascii_digit()).filter_map(|p| p.parse::<u32>().ok());
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (major, minor) >= (2, 1)
}

/// A link to the Ledger with the Bitcoin app open and unlocked.
async fn ready(app: &AppHandle, st: &LedgerState, network: Network) -> Result<Arc<dyn Link>> {
    let cancels = st.cancels.load(Ordering::SeqCst);
    let started = std::time::Instant::now();
    let mut reconnects = 0;
    let want = app_name(network);
    loop {
        if st.cancels.load(Ordering::SeqCst) != cancels {
            return Err(cancelled());
        }
        if started.elapsed() > PERSON_WAIT {
            return Err(Error::Invalid(
                "The Ledger didn't get ready in time. Unlock it, open the Bitcoin app, then try again".into(),
            ));
        }
        let link = connect(app, st).await?;
        match running_app(&link).await {
            Err(Fail::App(e)) => return Err(e),
            Err(Fail::Link(why)) => {
                // Opening an app restarts the USB connection; Bluetooth may drop.
                tracing::info!("ledger: link dropped ({why}); reconnecting");
                drop_link(st).await;
                reconnects += 1;
                if reconnects > 8 {
                    return Err(link::lost_connection());
                }
                tokio::time::sleep(Duration::from_millis(1200)).await;
            }
            Ok(Err(0x5515 | 0x6b0c | 0x6982)) => {
                status(app, "unlock");
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
            Ok(Err(sw)) => return Err(status_error(sw)),
            Ok(Ok((name, version))) if name == want => {
                if !new_enough(&version) {
                    return Err(update_app());
                }
                return Ok(link);
            }
            Ok(Ok((name, _))) if name == "BOLOS" => {
                status(app, "open-app");
                let mut apdu = vec![0xe0, 0xd8, 0x00, 0x00, want.len() as u8];
                apdu.extend_from_slice(want.as_bytes());
                match link.exchange(&apdu, PERSON_WAIT).await {
                    Ok(resp) => match frame::split_status(&resp).map(|(sw, _)| sw) {
                        Some(0x9000) => {}
                        Some(0x5501) | Some(0x6985) => {
                            return Err(Error::Invalid("Opening the Bitcoin app was declined on the Ledger".into()))
                        }
                        Some(sw) => return Err(status_error(sw)),
                        None => {}
                    },
                    Err(Fail::App(e)) => return Err(e),
                    Err(Fail::Link(_)) => drop_link(st).await,
                }
                tokio::time::sleep(Duration::from_millis(800)).await;
            }
            Ok(Ok((name, _))) => {
                // Another app (or the old Bitcoin Legacy app): close it first.
                tracing::info!("ledger: {name} is open; closing it");
                status(app, "open-app");
                let _ = link.exchange(&[0xb0, 0xa7, 0x00, 0x00, 0x00], QUICK).await;
                tokio::time::sleep(Duration::from_millis(800)).await;
            }
        }
    }
}

/// Run `op` with the Bitcoin app ready. A dropped link is reconnected once.
async fn with_client<T, F, Fut>(app: &AppHandle, st: &LedgerState, network: Network, wait: Duration, op: F) -> Result<T>
where
    F: Fn(BitcoinClient<Apdu>) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, Fail>>,
{
    for attempt in 0..2 {
        let link = ready(app, st, network).await?;
        let client = BitcoinClient::new(Apdu { link, wait });
        match op(client).await {
            Ok(v) => return Ok(v),
            Err(Fail::App(e)) => return Err(e),
            Err(Fail::Link(why)) => {
                tracing::warn!("ledger: lost the link ({why})");
                drop_link(st).await;
                if attempt == 1 {
                    return Err(link::lost_connection());
                }
            }
        }
    }
    Err(link::lost_connection())
}

async fn network(state: &AppState) -> Network {
    state.network().await.unwrap_or(crate::state::NETWORK)
}

fn path(numbers: &[u32]) -> DerivationPath {
    numbers.iter().map(|n| ChildNumber::from(*n)).collect::<Vec<_>>().into()
}

fn coin(network: Network) -> u32 {
    if network == Network::Bitcoin {
        0
    } else {
        1
    }
}

/// The standard single-sig policy for an account key.
fn policy(script: ScriptKind, fp: Fingerprint, account_path: DerivationPath, xpub: bitcoin::bip32::Xpub) -> WalletPolicy {
    let template = match script {
        ScriptKind::Wpkh => "wpkh(@0/**)",
        ScriptKind::Tr => "tr(@0/**)",
    };
    WalletPolicy::new(
        String::new(),
        Version::V2,
        template.to_string(),
        vec![WalletPubKey::from(((fp, account_path), xpub))],
    )
}

// --- commands ---------------------------------------------------------------------

/// Stop whatever is being asked of the Ledger (the screen was closed).
#[tauri::command]
pub async fn ledger_cancel(ledger: State<'_, LedgerState>) -> Result<()> {
    ledger.cancels.fetch_add(1, Ordering::SeqCst);
    drop_link(&ledger).await;
    Ok(())
}

/// One descriptor per account number:
/// `wpkh([fp/84h/0h/{n}h]xpub…/<0;1>/*)` (`tr`, `86h` for Taproot).
#[tauri::command]
pub async fn ledger_descriptors(
    app: AppHandle,
    state: State<'_, AppState>,
    ledger: State<'_, LedgerState>,
    script: String,
    accounts: Vec<u32>,
) -> Result<Vec<String>> {
    let script = ScriptKind::parse(&script)?;
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    if accounts.len() > 20 || accounts.iter().any(|n| *n >= 100) {
        return Err(Error::Invalid("Ask for accounts 0 to 99, at most 20 at a time".into()));
    }
    let network = network(&state).await;
    let accounts = Arc::new(accounts);
    with_client(&app, &ledger, network, QUICK, |client| {
        let accounts = accounts.clone();
        async move {
            let fp = client.get_master_fingerprint().await.map_err(client_error)?;
            let mut out = Vec::with_capacity(accounts.len());
            for n in accounts.iter() {
                let p = path(&[script.purpose() | HARDENED, coin(network) | HARDENED, n | HARDENED]);
                let xpub = client.get_extended_pubkey(&p, false).await.map_err(client_error)?;
                out.push(
                    helpers::descriptor_on(&helpers::LEDGER, script, network, &fp.to_string(), *n, &xpub.to_string())
                        .map_err(Fail::App)?,
                );
            }
            Ok(out)
        }
    })
    .await
}

/// The account (script, account path) the PSBT's inputs spend from, by the
/// Ledger's fingerprint `fp`.
fn spending_account(psbt: &Psbt, fp: Fingerprint) -> Result<(ScriptKind, DerivationPath)> {
    let mut found: Option<(ScriptKind, DerivationPath)> = None;
    for input in &psbt.inputs {
        let origin = input
            .tap_key_origins
            .values()
            .find(|(leaves, (f, _))| *f == fp && leaves.is_empty())
            .map(|(_, (_, p))| (ScriptKind::Tr, p.clone()))
            .or_else(|| {
                input
                    .bip32_derivation
                    .values()
                    .find(|(f, _)| *f == fp)
                    .map(|(_, p)| (ScriptKind::Wpkh, p.clone()))
            });
        let Some((script, full)) = origin else {
            return Err(Error::Invalid(
                "This Ledger doesn't hold the keys for this account. Connect the Ledger you added it with".into(),
            ));
        };
        let numbers: Vec<u32> = full.into_iter().map(|c| u32::from(*c)).collect();
        if numbers.len() != 5 || numbers[0] != (script.purpose() | HARDENED) {
            return Err(Error::Unsupported("sign this kind of coin on a Ledger".into()));
        }
        let account = path(&numbers[..3]);
        match &found {
            None => found = Some((script, account)),
            Some((s, a)) if *s == script && *a == account => {}
            Some(_) => {
                return Err(Error::Unsupported("sign coins from two accounts at once on a Ledger".into()))
            }
        }
    }
    found.ok_or_else(|| Error::Invalid("There's nothing to sign".into()))
}

/// Put the Ledger's signatures into the PSBT.
fn apply(psbt: &mut Psbt, sigs: Vec<(usize, SignPsbtYieldedObject)>) -> Result<()> {
    for (i, obj) in sigs {
        let input = psbt
            .inputs
            .get_mut(i)
            .ok_or_else(|| Error::Invalid("The Ledger signed an input that isn't there".into()))?;
        match obj {
            SignPsbtYieldedObject::Partial(PartialSignature::Sig(pk, sig)) => {
                input.partial_sigs.insert(pk, sig);
            }
            SignPsbtYieldedObject::Partial(PartialSignature::TapScriptSig(_, None, sig)) => {
                input.tap_key_sig = Some(sig);
            }
            SignPsbtYieldedObject::Partial(PartialSignature::TapScriptSig(key, Some(leaf), sig)) => {
                input.tap_script_sigs.insert((key, leaf), sig);
            }
            _ => {}
        }
    }
    Ok(())
}

/// Sign an unsigned PSBT (base64) on the Ledger. Returns it with the
/// Ledger's signatures, ready for `resume`.
#[tauri::command]
pub async fn ledger_sign(
    app: AppHandle,
    state: State<'_, AppState>,
    ledger: State<'_, LedgerState>,
    psbt: String,
    account: Option<AccountId>,
) -> Result<String> {
    let unsigned = Psbt::from_str(psbt.trim())
        .map_err(|_| Error::Invalid("That isn't a transaction the Ledger can sign".into()))?;
    if let Some(id) = &account {
        if crate::bitbox::find_vault(&state, id).await?.is_some() {
            return Err(Error::Unsupported("sign for a multisig vault on a Ledger yet".into()));
        }
    }
    let network = network(&state).await;
    let ui = app.clone();
    let unsigned = Arc::new(unsigned);
    with_client(&app, &ledger, network, CONFIRM_WAIT, |client| {
        let unsigned = unsigned.clone();
        let ui = ui.clone();
        async move {
            let fp = client.get_master_fingerprint().await.map_err(client_error)?;
            helpers::check_psbt_on(&helpers::LEDGER, &unsigned, fp).map_err(Fail::App)?;
            let (script, account_path) = spending_account(&unsigned, fp).map_err(Fail::App)?;
            let xpub = client.get_extended_pubkey(&account_path, false).await.map_err(client_error)?;
            let wallet = policy(script, fp, account_path, xpub);
            status(&ui, "confirm");
            let sigs = client.sign_psbt(&unsigned, &wallet, None).await.map_err(client_error)?;
            let mut psbt = (*unsigned).clone();
            apply(&mut psbt, sigs).map_err(Fail::App)?;
            if !helpers::fully_signed(&psbt) {
                return Err(Fail::App(Error::Invalid("The Ledger didn't sign every input".into())));
            }
            Ok(psbt.to_string())
        }
    })
    .await
}

/// Show receive address `index` of a Ledger account on the device, after
/// checking it matches the address this wallet derives. Returns the address.
#[tauri::command]
pub async fn ledger_show_address(
    app: AppHandle,
    state: State<'_, AppState>,
    ledger: State<'_, LedgerState>,
    account: AccountId,
    index: u32,
) -> Result<String> {
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
            _ => return Err(Error::Unsupported("show its addresses on a Ledger".into())),
        }
    };
    let at = helpers::receive_path(&pair.external, index)?;
    let expected = descriptors::address_at(&pair, network, index)?;
    let ui = app.clone();
    with_client(&app, &ledger, network, CONFIRM_WAIT, |client| {
        let ui = ui.clone();
        let expected = expected.clone();
        let at = at.clone();
        async move {
            let fp = client.get_master_fingerprint().await.map_err(client_error)?;
            if fp != at.fingerprint {
                return Err(Fail::App(Error::Invalid(
                    "This Ledger doesn't hold this account's keys. Connect the Ledger you added it with".into(),
                )));
            }
            let account_path = path(&at.keypath[..3]);
            let xpub = client.get_extended_pubkey(&account_path, false).await.map_err(client_error)?;
            let wallet = policy(at.script, fp, account_path, xpub);
            status(&ui, "confirm");
            let shown = client
                .get_wallet_address(&wallet, None, false, index, true)
                .await
                .map_err(client_error)?
                .assume_checked()
                .to_string();
            if shown != expected {
                return Err(Fail::App(Error::Invalid(
                    "The Ledger showed a different address than this wallet. Don't use it".into(),
                )));
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
    fn app_versions() {
        assert!(new_enough("2.1.0"));
        assert!(new_enough("2.4.1"));
        assert!(!new_enough("2.0.6"));
        assert!(!new_enough("1.6.6"));
        assert_eq!(app_name(Network::Bitcoin), "Bitcoin");
    }

    #[test]
    fn plain_errors_for_device_states() {
        assert!(status_error(0x5515).to_string().contains("Unlock"));
        assert!(status_error(0x6807).to_string().contains("isn't installed"));
        assert!(status_error(0x6e01).to_string().contains("Open the Bitcoin app"));
    }
}

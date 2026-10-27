//! BitBox02 over USB: account descriptors, PSBT signing and address checks.
//!
//! Desktop opens the device with hidapi. Android asks for USB access through
//! `tauri-plugin-bitbox`, whose Kotlin side moves the raw 64-byte HID reports
//! (`bulkTransfer`, as the BitBoxApp does). The unlocked, paired device stays
//! connected between calls, so the password and pairing code come up only
//! once.
//!
//! Events for the UI:
//! * `bitbox://status` `{ state: "connecting" | "unlock" | "confirm" }`
//! * `bitbox://pairing` `{ code }`: show it; the user checks it matches the
//!   device and confirms there.
//! * `bitbox://paired` `{}`: connected (and paired, if that was needed).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bitbox_api::btc::{make_script_config_multisig, make_script_config_simple, PsbtError};
use bitbox_api::error::{BitBoxError, Error as DeviceError};
use bitbox_api::pb::{self, btc_script_config::multisig::ScriptType as MultisigType};
use bitbox_api::pb::{btc_script_config::SimpleType, BtcScriptConfigWithKeypath};
use bitbox_api::runtime::TokioRuntime;
use bitbox_api::{ConfigError, Keypath, NoiseConfig, NoiseConfigData, PairedBitBox};
use bitcoin::bip32::{ChildNumber, DerivationPath, Fingerprint, Xpub};
use bitcoin::Psbt;
use rail_onchain::bitbox::{self as helpers, ScriptKind, BITBOX02};
use rail_onchain::descriptors;
use rail_onchain::multisig::{MultisigConfig, Policy, ScriptType};
use serde::Serialize;
use sessions_core::{AccountId, Error, Network, Result};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Mutex;

use crate::config::AccountSpec;
use crate::state::{AppState, NETWORK};

type Device = PairedBitBox<TokioRuntime>;

/// Generous: the user may be typing a password or reading the screen. Long
/// enough for that, short enough that a forgotten device doesn't hang the UI.
const TIMEOUT: Duration = Duration::from_secs(180);

/// The first exchange (device info) needs nobody: a BitBox02 that hasn't
/// answered by then won't, so say so instead of spinning.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
const ANSWER_WAIT: Duration = Duration::from_secs(15);

/// The connected BitBox02, kept between calls.
pub struct BitBoxState {
    pairing_file: PathBuf,
    /// Also serialises device access: one conversation at a time.
    device: Mutex<Option<Arc<Device>>>,
}

impl BitBoxState {
    pub fn new(data_dir: &Path) -> Self {
        BitBoxState {
            pairing_file: data_dir.join("bitbox-pairing.json"),
            device: Mutex::new(None),
        }
    }
}

// --- pairing keys -----------------------------------------------------------

/// The app's Noise key and the devices it has paired with, readable by the
/// owner only. Losing it just means confirming a pairing code again.
struct PairingStore(PathBuf);

impl bitbox_api::Threading for PairingStore {}

impl NoiseConfig for PairingStore {
    fn read_config(&self) -> std::result::Result<NoiseConfigData, ConfigError> {
        match std::fs::read(&self.0) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| ConfigError(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(NoiseConfigData::default()),
            Err(e) => Err(ConfigError(e.to_string())),
        }
    }

    fn store_config(&self, conf: &NoiseConfigData) -> std::result::Result<(), ConfigError> {
        let bytes = serde_json::to_vec(conf).map_err(|e| ConfigError(e.to_string()))?;
        write_private(&self.0, &bytes).map_err(|e| ConfigError(e.to_string()))
    }
}

/// Atomic write of a file only its owner can read (0600 on unix).
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    {
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

// --- events -----------------------------------------------------------------

#[derive(Clone, Serialize)]
struct Status {
    state: &'static str,
}

#[derive(Clone, Serialize)]
struct Pairing {
    code: String,
}

#[derive(Clone, Serialize)]
struct Paired {}

fn status(app: &AppHandle, state: &'static str) {
    let _ = app.emit("bitbox://status", Status { state });
}

// --- errors -----------------------------------------------------------------

fn plug_in() -> Error {
    Error::Invalid("Plug in your BitBox02".into())
}

fn timed_out() -> Error {
    Error::Invalid(
        "The BitBox02 didn't respond in time. Unplug it, plug it back in and try again".into(),
    )
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn not_answering() -> Error {
    Error::Invalid("The BitBox02 isn't answering. Unplug it, plug it back in and try again".into())
}

fn lost_connection() -> Error {
    Error::Invalid(
        "Lost the connection to the BitBox02. Unplug it, plug it back in and try again".into(),
    )
}

/// Errors that mean the connection is gone, not that the device said no.
fn is_transport(e: &DeviceError) -> bool {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if matches!(e, DeviceError::Hid(_)) {
        return true;
    }
    matches!(
        e,
        DeviceError::Communication(_)
            | DeviceError::Noise
            | DeviceError::UnexpectedResponse
            | DeviceError::ProtobufDecode
    )
}

/// Plain English for people holding the device.
fn friendly(e: DeviceError) -> Error {
    if is_transport(&e) {
        return lost_connection();
    }
    let msg = match &e {
        DeviceError::BitBox(BitBoxError::UserAbort) => "Cancelled on the BitBox02".to_string(),
        DeviceError::NoisePairingRejected => {
            "The pairing code wasn't confirmed on the BitBox02. Try again, and confirm if the codes match"
                .to_string()
        }
        DeviceError::Version(req) => match req.strip_prefix(">=") {
            Some(v) => format!(
                "Update your BitBox02 firmware (needs {} or newer) in the BitBoxApp, then try again",
                v.trim_end_matches(".0")
            ),
            None => "Update your BitBox02 firmware in the BitBoxApp, then try again".to_string(),
        },
        DeviceError::Psbt(PsbtError::KeyNotFound) => {
            "This BitBox02 doesn't hold the keys for this account. Plug in the BitBox02 you added it with"
                .to_string()
        }
        DeviceError::Psbt(_) => "The BitBox02 can't sign this kind of transaction".to_string(),
        DeviceError::BitBox(BitBoxError::InvalidState) => {
            "The BitBox02 is busy. Finish or cancel what's on its screen, then try again".to_string()
        }
        DeviceError::BitBox(BitBoxError::InvalidInput) => {
            "The BitBox02 refused this request".to_string()
        }
        _ => format!("BitBox02 error: {e}"),
    };
    Error::Invalid(msg)
}

/// Why an operation on a connected device failed.
enum Fail {
    Device(DeviceError),
    App(Error),
}

impl From<DeviceError> for Fail {
    fn from(e: DeviceError) -> Self {
        Fail::Device(e)
    }
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::App(e)
    }
}

// --- connection -------------------------------------------------------------

async fn connect(app: &AppHandle, bb: &BitBoxState) -> Result<Device> {
    status(app, "connecting");
    let noise = Box::new(PairingStore(bb.pairing_file.clone()));
    let bitbox = transport::open(app, noise).await?;
    // Returns at once if already unlocked; otherwise waits for the password.
    status(app, "unlock");
    let pairing = bitbox.unlock_and_pair().await.map_err(|e| match e {
        // unlock_and_pair reports any failure while unlocking as Unknown.
        DeviceError::Unknown => Error::Invalid(
            "Unlock your BitBox02 with your password (a new one needs setting up in the BitBoxApp first)"
                .into(),
        ),
        e => friendly(e),
    })?;
    if let Some(code) = pairing.get_pairing_code() {
        let _ = app.emit("bitbox://pairing", Pairing { code });
    }
    let paired = pairing.wait_confirm().await.map_err(friendly)?;
    let _ = app.emit("bitbox://paired", Paired {});
    Ok(paired)
}

/// Run `op` on the connected device, connecting (and unlocking, pairing)
/// first if needed. A stale connection (unplugged, replugged) is dropped and
/// retried once.
async fn with_device<T, F, Fut>(app: &AppHandle, bb: &BitBoxState, op: F) -> Result<T>
where
    F: Fn(Arc<Device>) -> Fut,
    Fut: Future<Output = std::result::Result<T, Fail>>,
{
    let mut slot = bb.device.lock().await;
    let mut retried = false;
    loop {
        let (device, fresh) = match slot.as_ref() {
            Some(d) => (d.clone(), false),
            None => {
                let d = match tokio::time::timeout(TIMEOUT, connect(app, bb)).await {
                    Ok(Ok(d)) => Arc::new(d),
                    Ok(Err(e)) => {
                        transport::release(app).await;
                        return Err(e);
                    }
                    Err(_) => {
                        transport::release(app).await;
                        return Err(timed_out());
                    }
                };
                *slot = Some(d.clone());
                (d, true)
            }
        };
        let result = tokio::time::timeout(TIMEOUT, op(device)).await;
        match result {
            Ok(Ok(v)) => return Ok(v),
            Ok(Err(Fail::App(e))) => return Err(e),
            Ok(Err(Fail::Device(e))) if is_transport(&e) => {
                *slot = None;
                transport::release(app).await;
                if fresh || retried {
                    return Err(friendly(e));
                }
                retried = true;
            }
            Ok(Err(Fail::Device(e))) => return Err(friendly(e)),
            Err(_) => {
                // The device may still be mid-request: start over next time.
                *slot = None;
                transport::release(app).await;
                return Err(timed_out());
            }
        }
    }
}

fn coin(network: Network) -> pb::BtcCoin {
    match network {
        Network::Bitcoin => pb::BtcCoin::Btc,
        Network::Regtest => pb::BtcCoin::Rbtc,
        Network::Testnet | Network::Signet => pb::BtcCoin::Tbtc,
    }
}

fn keypath(numbers: &[u32]) -> Keypath {
    let path: DerivationPath = numbers.iter().map(|n| ChildNumber::from(*n)).collect();
    Keypath::from(&path)
}

/// The wallet's network, or the build's default before a wallet exists.
async fn network(state: &AppState) -> Network {
    state.network().await.unwrap_or(NETWORK)
}

// --- commands ---------------------------------------------------------------

/// One descriptor per account number, read from the BitBox02:
/// `wpkh([fp/84h/0h/{n}h]xpub…/<0;1>/*)` (`tr`, `86h` for Taproot).
#[tauri::command]
pub async fn bitbox_descriptors(
    app: AppHandle,
    state: State<'_, AppState>,
    bitbox: State<'_, BitBoxState>,
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
    let network = network(&state).await;
    let keypaths = accounts
        .iter()
        .map(|n| helpers::account_keypath(script, network, *n).map(|k| keypath(&k)))
        .collect::<Result<Vec<_>>>()?;
    let xpub_type = if network == Network::Bitcoin {
        pb::btc_xpubs_request::XPubType::Xpub
    } else {
        pb::btc_xpubs_request::XPubType::Tpub
    };
    let accounts = Arc::new(accounts);
    let keypaths = Arc::new(keypaths);
    with_device(&app, &bitbox, |d| {
        let accounts = accounts.clone();
        let keypaths = keypaths.clone();
        async move {
            let fp = d.root_fingerprint().await?;
            let xpubs = d.btc_xpubs(coin(network), &keypaths, xpub_type).await?;
            if xpubs.len() != accounts.len() {
                return Err(DeviceError::UnexpectedResponse.into());
            }
            let mut out = Vec::with_capacity(xpubs.len());
            for (n, xpub) in accounts.iter().zip(&xpubs) {
                out.push(helpers::descriptor(script, network, &fp, *n, xpub)?);
            }
            Ok(out)
        }
    })
    .await
}

/// Sign an unsigned PSBT (base64) on the BitBox02. Returns the PSBT with the
/// device's signatures, ready for `resume`.
///
/// With `account` set to a multisig vault, the vault is registered on the
/// device first if needed, and the PSBT comes back with this device's
/// signatures only: other cosigners may still need to sign.
#[tauri::command]
pub async fn bitbox_sign(
    app: AppHandle,
    state: State<'_, AppState>,
    bitbox: State<'_, BitBoxState>,
    psbt: String,
    account: Option<AccountId>,
) -> Result<String> {
    let unsigned = Psbt::from_str(psbt.trim())
        .map_err(|_| Error::Invalid("That isn't a transaction the BitBox02 can sign".into()))?;
    if let Some(id) = &account {
        if let Some(vault) = find_vault(&state, id).await? {
            return sign_vault(&app, &bitbox, unsigned, vault).await;
        }
    }
    let network = network(&state).await;
    let ui = app.clone();
    with_device(&app, &bitbox, |d| {
        let mut psbt = unsigned.clone();
        let ui = ui.clone();
        async move {
            let fp = Fingerprint::from_str(&d.root_fingerprint().await?)
                .map_err(|_| DeviceError::UnexpectedResponse)?;
            helpers::check_psbt(&psbt, fp)?;
            status(&ui, "confirm");
            d.btc_sign_psbt(
                coin(network),
                &mut psbt,
                None,
                pb::btc_sign_init_request::FormatUnit::Default,
            )
            .await?;
            if !helpers::fully_signed(&psbt) {
                return Err(Error::Invalid("The BitBox02 didn't sign every input".into()).into());
            }
            Ok(psbt.to_string())
        }
    })
    .await
}

/// Show receive address `index` of a BitBox02 account on the device, after
/// checking it matches the address this wallet derives. Returns the address.
#[tauri::command]
pub async fn bitbox_show_address(
    app: AppHandle,
    state: State<'_, AppState>,
    bitbox: State<'_, BitBoxState>,
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
            _ => {
                return Err(Error::Unsupported(
                    "show its addresses on a BitBox02".into(),
                ))
            }
        }
    };
    let path = helpers::receive_path(&pair.external, index)?;
    let expected = descriptors::address_at(&pair, network, index)?;
    let script_config = make_script_config_simple(match path.script {
        ScriptKind::Wpkh => SimpleType::P2wpkh,
        ScriptKind::Tr => SimpleType::P2tr,
    });
    let device_keypath = keypath(&path.keypath);
    let account_fp = path.fingerprint.to_string();
    let ui = app.clone();
    with_device(&app, &bitbox, |d| {
        let ui = ui.clone();
        let expected = expected.clone();
        let script_config = script_config.clone();
        let device_keypath = device_keypath.clone();
        let account_fp = account_fp.clone();
        async move {
            if d.root_fingerprint().await? != account_fp {
                return Err(Error::Invalid(
                    "This BitBox02 doesn't hold this account's keys. Plug in the BitBox02 you added it with"
                        .into(),
                )
                .into());
            }
            status(&ui, "confirm");
            let shown = d
                .btc_address(coin(network), &device_keypath, &script_config, true)
                .await?;
            if shown != expected {
                return Err(Error::Invalid(
                    "The BitBox02 showed a different address than this wallet. Don't use it"
                        .into(),
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
// A vault is registered on the BitBox02 once (the person checks the quorum
// and keys, and the name, on the device); after that it shows the vault's
// receive addresses and recognises its change. BIP48 keys only
// (`m/48'/coin'/0..99'/2'` native SegWit, `…/1'` nested), sorted keys
// (`sortedmulti`); signing needs firmware 9.15 or newer.

/// A multisig vault from the wallet's accounts (shared with Jade and Trezor).
#[derive(Debug, Clone)]
pub(crate) struct Vault {
    /// The account's name.
    pub name: String,
    pub config: MultisigConfig,
    pub descriptors: rail_onchain::DescriptorPair,
    pub network: Network,
}

/// The vault `account`, or `None` for an account of another kind.
pub(crate) async fn find_vault(state: &AppState, account: &AccountId) -> Result<Option<Vault>> {
    let (name, pair, network) = {
        let guard = state.inner.lock().await;
        let u = guard.as_ref().ok_or(Error::Locked)?;
        let spec = u
            .config
            .accounts
            .iter()
            .find(|a| a.id() == account)
            .ok_or_else(|| Error::AccountNotFound(account.0.clone()))?;
        match spec {
            AccountSpec::Multisig {
                name, descriptors, ..
            } => (name.clone(), descriptors.clone(), u.config.network),
            _ => return Ok(None),
        }
    };
    let config = MultisigConfig::from_descriptor(&pair.external, network)?;
    Ok(Some(Vault {
        name,
        config,
        descriptors: pair,
        network,
    }))
}

/// The vault `account`; an error for any other kind of account.
pub(crate) async fn vault(state: &AppState, account: &AccountId) -> Result<Vault> {
    find_vault(state, account)
        .await?
        .ok_or_else(|| Error::Invalid("That account isn't a multisig vault".into()))
}

fn xpub_type(network: Network) -> pb::btc_xpubs_request::XPubType {
    if network == Network::Bitcoin {
        pb::btc_xpubs_request::XPubType::Xpub
    } else {
        pb::btc_xpubs_request::XPubType::Tpub
    }
}

/// The BitBox02's script type for a vault, if it can sign for it at all.
fn multisig_type(config: &MultisigConfig) -> Result<MultisigType> {
    let Policy::Multi { sorted, .. } = config.policy;
    if !sorted {
        return Err(Error::Invalid(
            "The BitBox02 only supports vaults with sorted keys (sortedmulti)".into(),
        ));
    }
    match config.script {
        ScriptType::P2wsh => Ok(MultisigType::P2wsh),
        ScriptType::P2shP2wsh => Ok(MultisigType::P2wshP2sh),
        ScriptType::P2sh => Err(Error::Invalid(
            "The BitBox02 doesn't support legacy (P2SH) multisig vaults".into(),
        )),
    }
}

/// The vault as the BitBox02 takes it, with key `ours` (an index into the
/// vault's keys) as the device's: the script config and the account keypath.
fn multisig_script_config(
    config: &MultisigConfig,
    network: Network,
    ours: usize,
) -> Result<(pb::BtcScriptConfig, Vec<u32>)> {
    let script_type = multisig_type(config)?;
    let key = config
        .cosigners
        .get(ours)
        .ok_or_else(|| helpers::not_a_vault_key(&BITBOX02))?;
    let path: Vec<u32> = DerivationPath::from_str(&key.path)
        .map_err(|e| Error::Crypto(e.to_string()))?
        .into_iter()
        .map(|c| u32::from(*c))
        .collect();
    // The firmware takes m/48'/coin'/account'/script', account 0' to 99',
    // script 2' native or 1' nested.
    const H: u32 = 0x8000_0000;
    let coin = if network == Network::Bitcoin { H } else { 1 | H };
    let level = match script_type {
        MultisigType::P2wsh => 2 | H,
        MultisigType::P2wshP2sh => 1 | H,
    };
    if !matches!(path.as_slice(), [p, c, a, s] if *p == 48 | H && *c == coin && (H..=99 | H).contains(a) && *s == level)
    {
        return Err(Error::Invalid(format!(
            "The BitBox02 only signs with vault keys on the standard path ({}), account 0 to 99",
            match script_type {
                MultisigType::P2wsh => "m/48'/coin'/account'/2'",
                MultisigType::P2wshP2sh => "m/48'/coin'/account'/1'",
            }
        )));
    }
    let xpubs = config
        .cosigners
        .iter()
        .map(|c| Xpub::from_str(&c.xpub).map_err(|e| Error::Crypto(e.to_string())))
        .collect::<Result<Vec<_>>>()?;
    Ok((
        make_script_config_multisig(
            config.threshold() as u32,
            &xpubs,
            ours as u32,
            script_type,
        ),
        path,
    ))
}

/// A vault as this BitBox02 signs for it.
#[derive(Clone)]
struct OnDevice {
    script_config: pb::BtcScriptConfig,
    /// The device's account keypath in the vault.
    account: Vec<u32>,
    fingerprint: Fingerprint,
}

/// Finds which of the vault's keys this BitBox02 holds (checking its xpub,
/// not only the fingerprint).
async fn vault_on_device(
    d: &Device,
    network: Network,
    config: &MultisigConfig,
) -> std::result::Result<OnDevice, Fail> {
    let fingerprint = Fingerprint::from_str(&d.root_fingerprint().await?)
        .map_err(|_| DeviceError::UnexpectedResponse)?;
    let mut candidates = Vec::new();
    let mut refused = None;
    for key in helpers::vault_keys(&BITBOX02, config, fingerprint)? {
        match multisig_script_config(config, network, key.index) {
            Ok(c) => candidates.push((key, c)),
            Err(e) => refused = Some(e),
        }
    }
    if candidates.is_empty() {
        return Err(refused.unwrap_or_else(|| helpers::not_a_vault_key(&BITBOX02)).into());
    }
    let paths: Vec<Keypath> = candidates.iter().map(|(_, (_, p))| keypath(p)).collect();
    let xpubs = d
        .btc_xpubs(coin(network), &paths, xpub_type(network))
        .await?;
    for ((key, (script_config, account)), xpub) in candidates.into_iter().zip(&xpubs) {
        if Xpub::from_str(xpub).is_ok_and(|x| helpers::same_key(&x, &key.xpub)) {
            return Ok(OnDevice {
                script_config,
                account,
                fingerprint,
            });
        }
    }
    Err(helpers::not_a_vault_key(&BITBOX02).into())
}

/// Register the vault on the BitBox02 unless it already is (the person
/// confirms on the device). Returns whether it registered it now.
async fn ensure_registered(
    d: &Device,
    ui: &AppHandle,
    network: Network,
    vault: &OnDevice,
    name: &str,
) -> std::result::Result<bool, Fail> {
    let account = keypath(&vault.account);
    if d
        .btc_is_script_config_registered(coin(network), &vault.script_config, Some(&account))
        .await?
    {
        return Ok(false);
    }
    status(ui, "confirm");
    d.btc_register_script_config(
        coin(network),
        &vault.script_config,
        Some(&account),
        pb::btc_register_script_config_request::XPubType::AutoXpubTpub,
        Some(name),
    )
    .await
    .map_err(|e| registration_failed(e, name))?;
    Ok(true)
}

fn registration_failed(e: DeviceError, name: &str) -> Fail {
    match e {
        DeviceError::BitBox(BitBoxError::Duplicate) => Fail::App(Error::Invalid(format!(
            "This BitBox02 already has a different multisig account called \"{name}\". Rename this vault, then try again"
        ))),
        DeviceError::BitBox(BitBoxError::Memory) => Fail::App(Error::Invalid(
            "This BitBox02 has no room for another multisig account".into(),
        )),
        e => Fail::Device(e),
    }
}

/// This BitBox02's key for a multisig vault (BIP48 native SegWit):
/// `[fingerprint/48h/0h/{account}h/2h]xpub…`.
#[tauri::command]
pub async fn bitbox_multisig_key(
    app: AppHandle,
    state: State<'_, AppState>,
    bitbox: State<'_, BitBoxState>,
    account: u32,
) -> Result<String> {
    if account > 99 {
        return Err(Error::Invalid(
            "The BitBox02 supports multisig account numbers 0 to 99".into(),
        ));
    }
    let network = network(&state).await;
    let path = keypath(&helpers::multisig_account_keypath(network, account)?);
    with_device(&app, &bitbox, |d| {
        let path = path.clone();
        async move {
            let fp = d.root_fingerprint().await?;
            let xpubs = d
                .btc_xpubs(coin(network), std::slice::from_ref(&path), xpub_type(network))
                .await?;
            let xpub = xpubs.first().ok_or(DeviceError::UnexpectedResponse)?;
            Ok(helpers::multisig_key_on(
                &BITBOX02, network, &fp, account, xpub,
            )?)
        }
    })
    .await
}

/// Register multisig vault `account` on the BitBox02, unless it already is.
/// Returns whether it was registered now (the person confirmed on the device).
#[tauri::command]
pub async fn bitbox_register_multisig(
    app: AppHandle,
    state: State<'_, AppState>,
    bitbox: State<'_, BitBoxState>,
    account: AccountId,
) -> Result<bool> {
    let vault = vault(&state, &account).await?;
    multisig_type(&vault.config)?;
    let name = helpers::bitbox02_vault_name(&vault.name);
    let network = vault.network;
    let config = Arc::new(vault.config);
    let ui = app.clone();
    with_device(&app, &bitbox, |d| {
        let (config, ui, name) = (config.clone(), ui.clone(), name.clone());
        async move {
            let on = vault_on_device(&d, network, &config).await?;
            ensure_registered(&d, &ui, network, &on, &name).await
        }
    })
    .await
}

/// Sign a vault PSBT: register the vault first if needed, then add this
/// BitBox02's signatures. Other cosigners may still have to sign.
async fn sign_vault(
    app: &AppHandle,
    bitbox: &BitBoxState,
    unsigned: Psbt,
    vault: Vault,
) -> Result<String> {
    multisig_type(&vault.config)?;
    let name = helpers::bitbox02_vault_name(&vault.name);
    let network = vault.network;
    let config = Arc::new(vault.config);
    let ui = app.clone();
    with_device(app, bitbox, |d| {
        let mut psbt = unsigned.clone();
        let (config, ui, name) = (config.clone(), ui.clone(), name.clone());
        async move {
            let on = vault_on_device(&d, network, &config).await?;
            helpers::check_multisig_psbt(&BITBOX02, &psbt, on.fingerprint)?;
            ensure_registered(&d, &ui, network, &on, &name).await?;
            status(&ui, "confirm");
            let forced = BtcScriptConfigWithKeypath {
                script_config: Some(on.script_config.clone()),
                keypath: on.account.clone(),
            };
            d.btc_sign_psbt(
                coin(network),
                &mut psbt,
                Some(forced),
                pb::btc_sign_init_request::FormatUnit::Default,
            )
            .await?;
            if !helpers::signed_own_inputs(&psbt, on.fingerprint) {
                return Err(
                    Error::Invalid("The BitBox02 didn't sign this vault's inputs".into()).into(),
                );
            }
            Ok(psbt.to_string())
        }
    })
    .await
}

/// Show receive address `index` of multisig vault `account` on the BitBox02
/// (registering the vault first if needed), after checking it matches the
/// address this wallet derives. Returns the address.
#[tauri::command]
pub async fn bitbox_show_multisig_address(
    app: AppHandle,
    state: State<'_, AppState>,
    bitbox: State<'_, BitBoxState>,
    account: AccountId,
    index: u32,
) -> Result<String> {
    let vault = vault(&state, &account).await?;
    multisig_type(&vault.config)?;
    if index > 9999 {
        return Err(Error::Invalid(
            "The BitBox02 shows vault addresses 0 to 9999".into(),
        ));
    }
    let network = vault.network;
    let expected = descriptors::address_at(&vault.descriptors, network, index)?;
    let name = helpers::bitbox02_vault_name(&vault.name);
    let config = Arc::new(vault.config);
    let ui = app.clone();
    with_device(&app, &bitbox, |d| {
        let (config, ui, name, expected) =
            (config.clone(), ui.clone(), name.clone(), expected.clone());
        async move {
            let on = vault_on_device(&d, network, &config).await?;
            ensure_registered(&d, &ui, network, &on, &name).await?;
            status(&ui, "confirm");
            let mut path = on.account.clone();
            path.extend([0, index]);
            let shown = d
                .btc_address(coin(network), &keypath(&path), &on.script_config, true)
                .await?;
            if shown != expected {
                return Err(Error::Invalid(
                    "The BitBox02 showed a different address than this wallet. Don't use it"
                        .into(),
                )
                .into());
            }
            Ok(shown)
        }
    })
    .await
}

// --- transports -------------------------------------------------------------

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod transport {
    use bitbox_api::runtime::TokioRuntime;
    use bitbox_api::{BitBox, NoiseConfig};
    use sessions_core::{Error, Result};
    use tauri::AppHandle;

    pub async fn open(
        _app: &AppHandle,
        noise: Box<dyn NoiseConfig>,
    ) -> Result<BitBox<TokioRuntime>> {
        let device = tokio::task::spawn_blocking(find)
            .await
            .map_err(|e| Error::Invalid(e.to_string()))??;
        BitBox::<TokioRuntime>::from_hid_device(device, noise)
            .await
            .map_err(super::friendly)
    }

    /// Nothing to release: dropping the device closes it.
    pub async fn release(_app: &AppHandle) {}

    fn find() -> Result<hidapi::HidDevice> {
        let api = hidapi::HidApi::new()
            .map_err(|e| Error::Invalid(format!("Couldn't use USB on this computer: {e}")))?;
        let info = api
            .device_list()
            .find(|d| bitbox_api::usb::is_bitbox02(d))
            .ok_or_else(super::plug_in)?;
        info.open_device(&api).map_err(|e| {
            tracing::warn!("bitbox: open failed: {e}");
            open_error()
        })
    }

    /// Linux only lets apps use the device once a udev rule allows it.
    #[cfg(target_os = "linux")]
    fn open_error() -> Error {
        Error::Invalid(super::LINUX_UDEV_HINT.into())
    }

    #[cfg(not(target_os = "linux"))]
    fn open_error() -> Error {
        Error::Invalid(
            "Couldn't open the BitBox02. Close other apps using it, like the BitBoxApp, and try again"
                .into(),
        )
    }
}

#[cfg(target_os = "android")]
mod transport {
    use std::sync::Mutex as SyncMutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use bitbox_api::runtime::TokioRuntime;
    use bitbox_api::{BitBox, CommunicationError, NoiseConfig, ReadWrite};
    use sessions_core::{Error, Result};
    use tauri::{AppHandle, Manager};
    use tauri_plugin_bitbox::{BitboxUsb, UsbError};

    /// Per read. The device answers every request at once (with "not ready"
    /// while it waits for the user, and bitbox-api polls), so this is only a
    /// safety net for a device that stopped talking.
    const READ_TIMEOUT: Duration = Duration::from_secs(30);
    /// Android's "allow access" prompt waits on the person.
    const PERMISSION_WAIT: Duration = Duration::from_secs(120);

    pub async fn open(
        app: &AppHandle,
        noise: Box<dyn NoiseConfig>,
    ) -> Result<BitBox<TokioRuntime>> {
        let usb = app.state::<BitboxUsb<tauri::Wry>>();
        let opened = tokio::time::timeout(PERMISSION_WAIT, usb.open())
            .await
            .map_err(|_| {
                tracing::warn!("bitbox: USB open timed out (permission prompt unanswered?)");
                allow_usb()
            })?
            .map_err(|e| {
                tracing::warn!("bitbox: USB open failed: {e}");
                match e {
                    UsbError::NotFound => super::plug_in(),
                    UsbError::PermissionDenied => allow_usb(),
                    UsbError::Busy => Error::Invalid(
                        "Another app is using the BitBox02. Close it (the BitBoxApp, for example) and try again"
                            .into(),
                    ),
                    UsbError::Timeout | UsbError::Other(_) => Error::Invalid(
                        "Couldn't open the BitBox02. Unplug it, plug it back in and try again"
                            .into(),
                    ),
                }
            })?;
        tracing::info!(
            "bitbox: opened {} over USB, asking for its info",
            opened.product
        );
        let transport = PluginTransport {
            app: app.clone(),
            pending: SyncMutex::new(Vec::new()),
        };
        // The first exchange needs nobody, so it gets a short leash.
        match tokio::time::timeout(
            super::ANSWER_WAIT,
            BitBox::<TokioRuntime>::from_transport(Box::new(transport), noise),
        )
        .await
        {
            Ok(Ok(b)) => {
                tracing::info!("bitbox: device answered");
                Ok(b)
            }
            Ok(Err(e)) => {
                tracing::warn!("bitbox: first exchange failed: {e:?}");
                Err(if super::is_transport(&e) {
                    super::not_answering()
                } else {
                    super::friendly(e)
                })
            }
            Err(_) => {
                tracing::warn!("bitbox: no answer within {:?}", super::ANSWER_WAIT);
                Err(super::not_answering())
            }
        }
    }

    fn allow_usb() -> Error {
        Error::Invalid(
            "Allow Sessions Wallet to use the BitBox02 when Android asks, then try again".into(),
        )
    }

    /// Release the interface and Android's connection to the device.
    pub async fn release(app: &AppHandle) {
        tracing::debug!("bitbox: releasing USB");
        app.state::<BitboxUsb<tauri::Wry>>().close().await;
    }

    /// Raw 64-byte HID reports through the plugin's Kotlin side. bitbox-api
    /// adds the U2F-HID framing on top.
    ///
    /// bitbox-api's `write` is synchronous but always followed by a `read`
    /// (every exchange is a query), so writes are queued here and sent, in
    /// one plugin call, by the next `read`. Nothing blocks a tokio worker.
    struct PluginTransport {
        app: AppHandle,
        pending: SyncMutex<Vec<u8>>,
    }

    impl bitbox_api::Threading for PluginTransport {}

    #[async_trait]
    impl ReadWrite for PluginTransport {
        fn write(&self, msg: &[u8]) -> std::result::Result<usize, CommunicationError> {
            let mut pending = self.pending.lock().map_err(|_| CommunicationError::Write)?;
            super::queue_reports(&mut pending, msg);
            Ok(msg.len())
        }

        async fn read(&self) -> std::result::Result<Vec<u8>, CommunicationError> {
            let usb = self.app.state::<BitboxUsb<tauri::Wry>>();
            let out =
                std::mem::take(&mut *self.pending.lock().map_err(|_| CommunicationError::Write)?);
            if !out.is_empty() {
                usb.write(&out).await.map_err(|e| {
                    tracing::warn!("bitbox: write of {} bytes failed: {e}", out.len());
                    CommunicationError::Write
                })?;
            }
            usb.read(READ_TIMEOUT).await.map_err(|e| {
                tracing::warn!("bitbox: read failed: {e}");
                CommunicationError::Read
            })
        }
    }
}

#[cfg(target_os = "ios")]
mod transport {
    use bitbox_api::runtime::TokioRuntime;
    use bitbox_api::{BitBox, NoiseConfig};
    use sessions_core::{Error, Result};
    use tauri::AppHandle;

    pub async fn open(
        _app: &AppHandle,
        _noise: Box<dyn NoiseConfig>,
    ) -> Result<BitBox<TokioRuntime>> {
        Err(Error::Invalid(
            "iPhones can't use a BitBox02 over USB. Use this wallet on a computer or Android phone"
                .into(),
        ))
    }

    pub async fn release(_app: &AppHandle) {}
}

/// One HID report.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
const REPORT: usize = 64;

/// Append `msg` as whole reports (zero-padded): raw USB, so no leading
/// report-ID byte (hidapi wants one; the wire doesn't).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn queue_reports(pending: &mut Vec<u8>, msg: &[u8]) {
    for chunk in msg.chunks(REPORT) {
        pending.extend_from_slice(chunk);
        pending.resize(pending.len() + (REPORT - chunk.len()), 0);
    }
}

/// Shown when Linux won't let us open the device (no udev rule yet).
#[cfg(any(target_os = "linux", test))]
const LINUX_UDEV_HINT: &str = "Linux is blocking access to the BitBox02. Add BitBox's udev rules \
(the Sessions Wallet .deb and the BitBoxApp both install them, or copy 53-hid-bitbox02.rules and \
54-hid-bitbox02.rules to /etc/udev/rules.d/), then unplug the BitBox02 and plug it back in";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_errors_are_plain() {
        let msg = |e| friendly(e).to_string();
        assert_eq!(
            msg(DeviceError::BitBox(BitBoxError::UserAbort)),
            "Cancelled on the BitBox02"
        );
        assert_eq!(
            msg(DeviceError::Version(">=9.15.0")),
            "Update your BitBox02 firmware (needs 9.15 or newer) in the BitBoxApp, then try again"
        );
        assert!(msg(DeviceError::Communication(
            bitbox_api::CommunicationError::Read
        ))
        .starts_with("Lost the connection"));
        assert!(is_transport(&DeviceError::Noise));
        assert!(!is_transport(&DeviceError::BitBox(BitBoxError::UserAbort)));
        assert!(LINUX_UDEV_HINT.contains("udev"));
    }

    #[test]
    fn reports_are_raw_and_padded() {
        let mut out = Vec::new();
        queue_reports(&mut out, &[0xff; 64]);
        assert_eq!(out, vec![0xff; 64]);
        queue_reports(&mut out, &[1, 2, 3]);
        assert_eq!(out.len(), 128);
        // No report-ID byte in front: the frame starts the report.
        assert_eq!(&out[64..67], &[1, 2, 3]);
        assert!(out[67..].iter().all(|b| *b == 0));
        let mut long = Vec::new();
        queue_reports(&mut long, &[7; 65]);
        assert_eq!(long.len(), 128);
        assert_eq!(long[64], 7);
        assert!(not_answering().to_string().contains("isn't answering"));
    }

    #[test]
    fn keypaths_convert() {
        let k = keypath(&[84 | 0x8000_0000, 0x8000_0000, 0x8000_0000, 0, 5]);
        assert_eq!(
            k.to_vec(),
            vec![84 | 0x8000_0000, 0x8000_0000, 0x8000_0000, 0, 5]
        );
    }

    #[test]
    fn pairing_store_round_trips_privately() {
        let dir = tempfile::tempdir().unwrap();
        let store = PairingStore(dir.path().join("bitbox-pairing.json"));
        assert!(store.read_config().unwrap().app_static_privkey.is_none());
        let conf = NoiseConfigData {
            app_static_privkey: Some([7; 32]),
            device_static_pubkeys: vec![vec![1, 2, 3]],
        };
        store.store_config(&conf).unwrap();
        let back = store.read_config().unwrap();
        assert_eq!(back.app_static_privkey, Some([7; 32]));
        assert_eq!(back.device_static_pubkeys, vec![vec![1, 2, 3]]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&store.0).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}

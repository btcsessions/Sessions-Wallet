//! Multisig wallet files: what other coordinators and signing devices write
//! and read, so a wallet made here opens in Sparrow, Nunchuk, Caravan,
//! BlueWallet, Specter, Coldcard, Keystone, Passport and Jade, and theirs
//! open here.
//!
//! Formats, from their primary sources:
//!
//! * **Descriptor** – one `<0;1>` multipath line with its checksum (BIP-389,
//!   what Sparrow shows). Import also takes the receive branch (`/0/*`),
//!   BSMS templates (`/**`), Nunchuk's `xpub/*` (receive `/0/*`, change
//!   `/1/*`), keys with no derivation at all (Sparrow's Specter export), and
//!   whole Sparrow / Coldcard descriptor files with `#` comment lines.
//! * **Coldcard text** – `Name:` / `Policy:` / `Derivation:` / `Format:` then
//!   `FINGERPRINT: xpub` lines. Coldcard, Keystone, Jade, Nunchuk and Sparrow
//!   all skip `#` lines and take a `Derivation:` line as the path for the keys
//!   after it; BlueWallet and Sparrow also read `# derivation:` comment lines.
//!   Keystone needs exactly one `Derivation:` line or one per key; Jade needs
//!   Name, Policy, Format and a Derivation before the first key; Sparrow can
//!   only read the `P2WSH-P2SH` spelling of nested segwit (the others take
//!   both), and splits on `:` so names and headers carry no colons.
//! * **BSMS 1.0** – BIP-129's unencrypted descriptor record: `BSMS 1.0`, the
//!   descriptor template with `/**`, `/0/*,/1/*`, the first receive address.
//!   Import checks that address against the keys, as the BIP requires.
//! * **Caravan** – Caravan's wallet configuration JSON. BlueWallet only reads
//!   it for mainnet. Older Caravan files may say `"Unknown"` for a key's path
//!   and leave out its fingerprint.
//! * **Specter** – `{label, blockheight, descriptor, devices}` with the
//!   receive descriptor, as Specter Desktop's `account_map` writes it (also
//!   what Fully Noded and BlueWallet read). `devices[i]` names key `i`.
//!
//! Imports also take Electrum multisig JSON (Coldcard and Sparrow write it)
//! and any JSON with a `descriptor` field (Sparrow, Nunchuk).

use std::str::FromStr;

use bdk_wallet::bitcoin::bip32::Xpub;
use bdk_wallet::bitcoin::{Address, NetworkKind};
use bdk_wallet::miniscript::descriptor::checksum::desc_checksum;
use bdk_wallet::miniscript::descriptor::{Descriptor, DescriptorPublicKey, Wildcard};
use bdk_wallet::miniscript::ForEachKey;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sessions_core::{Error, Network, Result};

use super::{Cosigner, MultisigConfig, Policy, ScriptType};
use crate::descriptors::{self, DescriptorPair};

const NOT_MULTISIG: &str = "That file isn't a multisig wallet. Load the wallet file or descriptor exported from your coordinator (Sparrow, Nunchuk, Caravan, Specter…)";
const SINGLE_SIG_WALLET: &str =
    "That's a single-signature wallet, not a multisig. Add it as a regular account instead";
const COSIGNER_NOT_WALLET: &str = "That's one cosigner's key, not a whole wallet. Add it as a key while setting up the multisig instead";
const WHOLE_WALLET: &str =
    "That's a whole multisig wallet, not one key. Import it as a wallet instead";
const SINGLE_SIG_EXPORT: &str =
    "That's a single-signature export; on the device choose the multisig export";
const SEED_IN_FILE: &str = "That file contains a recovery phrase. Export the public-keys-only (coordination) file instead, and keep the phrase offline";
const NO_KEY: &str = "Couldn't find a key in that. Load the device's multisig export file, or paste the key as [fingerprint/48h/0h/0h/2h]xpub…";
const MISSING_ORIGIN: &str = "That key is missing its fingerprint and derivation path. Use the device's multisig export, which includes them";
const TAPROOT: &str = "Taproot multisig wallets aren't supported yet";
/// Fingerprint for keys whose file didn't record one (old Caravan files,
/// Sparrow's Electrum export). Watching works; hardware signing needs the
/// real one.
const UNKNOWN_FINGERPRINT: &str = "00000000";

/// A wallet file format to export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExportFormat {
    /// One-line `<0;1>` output descriptor (Sparrow, Nunchuk, Bitcoin Core,
    /// BlueWallet, Coldcard).
    Descriptor,
    /// Coldcard-style text file (Coldcard, Keystone, Passport, Jade,
    /// BlueWallet, Sparrow, Nunchuk).
    Coldcard,
    /// BIP-129 BSMS 1.0 descriptor record (Nunchuk, Sparrow, Keystone).
    Bsms,
    /// Caravan wallet configuration JSON (Caravan, Unchained, BlueWallet).
    Caravan,
    /// Specter Desktop wallet backup JSON (Specter, Fully Noded, BlueWallet).
    Specter,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 5] = [
        ExportFormat::Descriptor,
        ExportFormat::Coldcard,
        ExportFormat::Bsms,
        ExportFormat::Caravan,
        ExportFormat::Specter,
    ];

    /// What to call it in the UI.
    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Descriptor => "Output descriptor (Sparrow, Nunchuk, Bitcoin Core)",
            ExportFormat::Coldcard => {
                "Multisig setup file (Coldcard, Keystone, Passport, Jade, BlueWallet)"
            }
            ExportFormat::Bsms => "BSMS file (Nunchuk, Sparrow)",
            ExportFormat::Caravan => "Caravan wallet configuration",
            ExportFormat::Specter => "Specter wallet backup",
        }
    }
}

/// A file ready to save or share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportFile {
    pub filename: String,
    pub mime: String,
    pub content: String,
}

impl MultisigConfig {
    /// The `<0;1>` multipath descriptor with its checksum, as Sparrow shows it.
    pub fn descriptor_with_checksum(&self) -> Result<String> {
        let pair = self.descriptors()?;
        descriptors::validate(&pair)?;
        with_checksum(&descriptors::multipath(&pair))
    }

    /// Receive address 0: the one to compare on every device (and the one a
    /// BSMS file carries).
    pub fn first_address(&self, network: Network) -> Result<String> {
        self.validate(network).map_err(net_err(network))?;
        descriptors::address_at(&self.descriptors()?, network, 0)
    }

    /// The wallet as a file for another coordinator or signing device.
    pub fn export(&self, format: ExportFormat, network: Network) -> Result<ExportFile> {
        self.validate(network).map_err(net_err(network))?;
        let stem = file_stem(self.name.as_deref());
        let (filename, mime, content) = match format {
            ExportFormat::Descriptor => (
                format!("{stem}-descriptor.txt"),
                "text/plain",
                format!("{}\n", self.descriptor_with_checksum()?),
            ),
            ExportFormat::Coldcard => (
                format!("{stem}-multisig.txt"),
                "text/plain",
                self.coldcard_text()?,
            ),
            ExportFormat::Bsms => (format!("{stem}.bsms"), "text/plain", self.bsms(network)?),
            ExportFormat::Caravan => (
                format!("{stem}-caravan.json"),
                "application/json",
                self.caravan_json(network)?,
            ),
            ExportFormat::Specter => (
                format!("{stem}-specter.json"),
                "application/json",
                self.specter_json()?,
            ),
        };
        Ok(ExportFile {
            filename,
            mime: mime.to_string(),
            content,
        })
    }

    /// Read a multisig wallet file, whichever coordinator wrote it: a
    /// descriptor (with or without checksum, `<0;1>`, `/0/*`, `/**`,
    /// Nunchuk's `/*`), a Coldcard-style text file, BSMS 1.0, Caravan,
    /// Specter or Electrum JSON, or any JSON with a `descriptor` field. The
    /// wallet and key names come along when the file has them.
    pub fn import(text: &str, network: Network) -> Result<Self> {
        let text = text.trim_start_matches('\u{feff}').trim();
        if text.is_empty() {
            return Err(invalid("That file is empty"));
        }
        let config = if text.starts_with('{') {
            import_json(text, network)?
        } else if text.starts_with("BSMS") {
            return import_bsms(text, network);
        } else if looks_like_coldcard(text) {
            parse_coldcard(text)?
        } else {
            config_from_descriptor_text(text, network)?
        };
        finish(config, network)
    }

    fn coldcard_text(&self) -> Result<String> {
        let Policy::Multi { threshold, sorted } = self.policy;
        if !sorted {
            return Err(Error::Unsupported(
                "export a multi() wallet (keys in a fixed order) as a multisig setup file – those files always mean sortedmulti. Export the descriptor instead".into(),
            ));
        }
        let uniform = self.cosigners.windows(2).all(|w| w[0].path == w[1].path);
        let mut lines = vec![
            "# Coldcard Multisig setup file (exported by Sessions Wallet)".to_string(),
            "#".to_string(),
            format!("Name: {}", coldcard_name(self.name.as_deref())),
            format!("Policy: {threshold} of {}", self.cosigners.len()),
        ];
        if uniform {
            lines.push(format!("Derivation: {}", self.cosigners[0].path));
        }
        lines.push(format!("Format: {}", coldcard_format(self.script)));
        lines.push(String::new());
        for c in &self.cosigners {
            if !uniform {
                // `Derivation:` for Coldcard, Keystone, Jade, Nunchuk and
                // Sparrow; the comment form for BlueWallet's per-key paths.
                lines.push(format!("# derivation: {}", c.path));
                lines.push(format!("Derivation: {}", c.path));
            }
            lines.push(format!("{}: {}", c.fingerprint.to_uppercase(), c.xpub));
            if !uniform {
                lines.push(String::new());
            }
        }
        let mut out = lines.join("\n");
        if !out.ends_with('\n') {
            out.push('\n');
        }
        Ok(out)
    }

    fn bsms(&self, network: Network) -> Result<String> {
        let Policy::Multi { threshold, sorted } = self.policy;
        let func = if sorted { "sortedmulti" } else { "multi" };
        let keys = self
            .cosigners
            .iter()
            .map(|c| format!("{}/**", origin_apostrophe(c)))
            .collect::<Vec<_>>()
            .join(",");
        let template = wrap_script(self.script, &format!("{func}({threshold},{keys})"));
        let address = self.first_address(network)?;
        Ok(format!("BSMS 1.0\n{template}\n/0/*,/1/*\n{address}\n"))
    }

    fn caravan_json(&self, network: Network) -> Result<String> {
        let Policy::Multi { threshold, sorted } = self.policy;
        if !sorted {
            return Err(Error::Unsupported(
                "export a multi() wallet (keys in a fixed order) to Caravan – Caravan only does sortedmulti. Export the descriptor instead".into(),
            ));
        }
        let keys: Vec<Value> = self
            .cosigners
            .iter()
            .enumerate()
            .map(|(i, c)| {
                json!({
                    "name": c.label.clone().unwrap_or_else(|| format!("Key {}", i + 1)),
                    "bip32Path": c.path,
                    "xpub": c.xpub,
                    "xfp": c.fingerprint,
                    "method": "text",
                })
            })
            .collect();
        let value = json!({
            "name": self.name.clone().unwrap_or_else(|| "Multisig".into()),
            "addressType": caravan_address_type(self.script),
            "network": match network {
                Network::Bitcoin => "mainnet",
                Network::Testnet => "testnet",
                Network::Signet => "signet",
                Network::Regtest => "regtest",
            },
            "client": { "type": "public" },
            "quorum": {
                "requiredSigners": threshold,
                "totalSigners": self.cosigners.len(),
            },
            "extendedPublicKeys": keys,
            "startingAddressIndex": 0,
        });
        to_json(&value)
    }

    fn specter_json(&self) -> Result<String> {
        let pair = self.descriptors()?;
        let devices: Vec<Value> = self
            .cosigners
            .iter()
            .enumerate()
            .map(|(i, c)| {
                json!({
                    "type": "other",
                    "label": c.label.clone().unwrap_or_else(|| format!("Key {}", i + 1)),
                })
            })
            .collect();
        let value = json!({
            "label": self.name.clone().unwrap_or_else(|| "Multisig".into()),
            "blockheight": 0,
            "descriptor": with_checksum(&pair.external)?,
            "devices": devices,
        });
        to_json(&value)
    }
}

/// One cosigner's key from whatever a device or app exports for multisig: a
/// `[fingerprint/48h/0h/0h/2h]xpub` expression (`'` or `h`, SLIP-132
/// `Zpub`/`Vpub` accepted), the Coldcard / Passport / Keystone multisig
/// JSON (`xfp` + `p2wsh_deriv`/`p2wsh`, older `p2wsh_p2sh` names too), the
/// Coldcard generic export (`bip48_2` section), Cobo/Keystone
/// `{xfp, xpub, path}`, an Electrum keystore, or a BSMS key record. When a
/// file has several keys, the BIP48 P2WSH one (`48'/coin'/account'/2'`)
/// wins.
pub fn cosigner_from_export(text: &str, network: Network) -> Result<Cosigner> {
    let text = text.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return Err(invalid(
            "Nothing to import – paste the key or load the device's export file",
        ));
    }
    if MultisigConfig::import(text, network).is_ok() {
        return Err(invalid(WHOLE_WALLET));
    }
    let mut found = Found::default();
    if text.starts_with('{') {
        let v: Value = serde_json::from_str(text).map_err(|_| {
            invalid("That file couldn't be read – it looks like JSON but isn't valid")
        })?;
        json_candidates(&v, &mut found)?;
    } else {
        text_candidates(text, &mut found);
    }
    pick_candidate(found, text, network)
}

// ---------------------------------------------------------------------------
// Import helpers.

fn invalid(msg: impl Into<String>) -> Error {
    Error::Invalid(msg.into())
}

fn wrong_network(network: Network) -> Error {
    if network == Network::Bitcoin {
        Error::Network(
            "this wallet is for a test network (testnet/signet), but the app is on Bitcoin mainnet"
                .into(),
        )
    } else {
        Error::Network(
            "this wallet is for Bitcoin mainnet, but the app is on a test network".into(),
        )
    }
}

/// Replace the generic network error with one that says which way round.
fn net_err(network: Network) -> impl Fn(Error) -> Error {
    move |e| match e {
        Error::Network(_) => wrong_network(network),
        e => e,
    }
}

fn want_kind(network: Network) -> NetworkKind {
    if network == Network::Bitcoin {
        NetworkKind::Main
    } else {
        NetworkKind::Test
    }
}

/// Tidy and check every key, then the whole wallet.
fn finish(mut config: MultisigConfig, network: Network) -> Result<MultisigConfig> {
    for c in &mut config.cosigners {
        check_cosigner(c, network)?;
    }
    config.name = config
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    config.validate(network).map_err(net_err(network))?;
    Ok(config)
}

/// Normalise one key (lowercase fingerprint, `m/48'/…` path, plain xpub) and
/// check it: valid, right network, path as deep as the xpub.
fn check_cosigner(c: &mut Cosigner, network: Network) -> Result<()> {
    c.fingerprint = c.fingerprint.trim().to_ascii_lowercase();
    if c.fingerprint.len() != 8 || !c.fingerprint.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(invalid(format!(
            "\"{}\" isn't a key fingerprint (8 hex characters)",
            c.fingerprint
        )));
    }
    let shown = c.fingerprint.to_uppercase();
    c.path = normalize_path(&c.path)?;
    c.xpub = descriptors::normalize_slip132(c.xpub.trim())
        .map_err(|_| invalid(format!("Key {shown} isn't a valid extended public key")))?;
    let xpub = Xpub::from_str(&c.xpub)
        .map_err(|_| invalid(format!("Key {shown} isn't a valid extended public key")))?;
    if xpub.network != want_kind(network) {
        return Err(wrong_network(network));
    }
    let levels = if c.path == "m" {
        0
    } else {
        c.path.split('/').count() - 1
    };
    if c.path != "m" && levels != xpub.depth as usize {
        return Err(invalid(format!(
            "Key {shown}'s derivation path ({}) doesn't match its xpub, which is {} levels deep. The file may be damaged – export it again",
            c.path, xpub.depth
        )));
    }
    c.label = c
        .label
        .take()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());
    Ok(())
}

/// `m/48'/0'/0'/2'` from `m/48h/0h/0h/2h`, `48'/0'/0'/2'`, `M/48H/…`;
/// `m` for an empty or unknown path.
fn normalize_path(p: &str) -> Result<String> {
    let p = p.trim();
    if p.is_empty() || p.eq_ignore_ascii_case("m") || p.to_ascii_lowercase().contains("unknown") {
        return Ok("m".into());
    }
    let rest = p
        .strip_prefix("m/")
        .or_else(|| p.strip_prefix("M/"))
        .unwrap_or(p);
    let bad = || invalid(format!("\"{p}\" isn't a valid derivation path"));
    let mut levels = Vec::new();
    for level in rest.split('/') {
        let (num, hardened) = match level.strip_suffix(['\'', 'h', 'H']) {
            Some(n) => (n, true),
            None => (level, false),
        };
        let n: u32 = num.parse().map_err(|_| bad())?;
        if n >= 0x8000_0000 {
            return Err(bad());
        }
        levels.push(if hardened {
            format!("{n}'")
        } else {
            n.to_string()
        });
    }
    Ok(format!("m/{}", levels.join("/")))
}

fn optional_path(p: Option<&str>) -> Option<String> {
    p.map(str::trim)
        .filter(|p| !p.is_empty() && !p.to_ascii_lowercase().contains("unknown"))
        .map(String::from)
}

fn optional_fingerprint(fp: Option<&str>) -> String {
    fp.map(str::trim)
        .filter(|f| f.len() == 8 && f.chars().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| UNKNOWN_FINGERPRINT.into())
}

fn with_checksum(desc: &str) -> Result<String> {
    let sum = desc_checksum(desc).map_err(|e| Error::Crypto(format!("bad descriptor: {e}")))?;
    Ok(format!("{desc}#{sum}"))
}

fn wrap_script(script: ScriptType, inner: &str) -> String {
    match script {
        ScriptType::P2wsh => format!("wsh({inner})"),
        ScriptType::P2shP2wsh => format!("sh(wsh({inner}))"),
        ScriptType::P2sh => format!("sh({inner})"),
    }
}

/// `[fingerprint/48'/0'/0'/2']xpub`, apostrophes as in BIP-129's examples.
fn origin_apostrophe(c: &Cosigner) -> String {
    let path = c.path.trim_start_matches('m').trim_start_matches('/');
    if path.is_empty() {
        format!("[{}]{}", c.fingerprint, c.xpub)
    } else {
        format!("[{}/{}]{}", c.fingerprint, path, c.xpub)
    }
}

fn coldcard_format(script: ScriptType) -> &'static str {
    match script {
        ScriptType::P2wsh => "P2WSH",
        // Sparrow can only read this spelling; everything else takes both.
        ScriptType::P2shP2wsh => "P2WSH-P2SH",
        ScriptType::P2sh => "P2SH",
    }
}

fn caravan_address_type(script: ScriptType) -> &'static str {
    match script {
        ScriptType::P2wsh => "P2WSH",
        ScriptType::P2shP2wsh => "P2SH-P2WSH",
        ScriptType::P2sh => "P2SH",
    }
}

/// Script type from a `Format:` / `addressType` value.
fn parse_format(value: &str) -> Result<ScriptType> {
    let v = value.trim().to_ascii_uppercase().replace('_', "-");
    match v.as_str() {
        "P2WSH" => Ok(ScriptType::P2wsh),
        "P2SH-P2WSH" | "P2WSH-P2SH" => Ok(ScriptType::P2shP2wsh),
        "P2SH" => Ok(ScriptType::P2sh),
        "P2TR" => Err(invalid(TAPROOT)),
        _ => Err(invalid(format!(
            "Unknown address format \"{}\" (expected P2WSH, P2SH-P2WSH or P2SH)",
            value.trim()
        ))),
    }
}

/// Coldcard limits names to 1–20 printable ASCII characters; `:` and `#`
/// would break the line-based readers.
fn coldcard_name(name: Option<&str>) -> String {
    let cleaned: String = name
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| (c.is_ascii_graphic() || *c == ' ') && *c != ':' && *c != '#')
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let short: String = collapsed.chars().take(20).collect();
    let short = short.trim().to_string();
    if short.is_empty() {
        "Multisig".into()
    } else {
        short
    }
}

/// A safe file name stem from the wallet name.
fn file_stem(name: Option<&str>) -> String {
    let mut out = String::new();
    for ch in name.unwrap_or_default().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else if (ch == '-' || ch == '.' || ch == '/' || ch == '\\' || ch.is_whitespace())
            && !out.is_empty()
            && !out.ends_with('-')
        {
            out.push('-');
        }
    }
    let out: String = out.chars().take(40).collect();
    let out = out.trim_matches('-');
    if out.is_empty() {
        "multisig".into()
    } else {
        out.to_string()
    }
}

fn to_json(value: &Value) -> Result<String> {
    serde_json::to_string_pretty(value)
        .map(|s| s + "\n")
        .map_err(|e| Error::Storage(e.to_string()))
}

fn str_field<'a>(v: &'a Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|n| v.get(*n).and_then(Value::as_str))
}

/// Coldcard's `ckcc_xfp`: the fingerprint bytes read as a little-endian
/// integer (number or string).
fn ckcc_xfp(v: &Value) -> Option<String> {
    let n = match v {
        Value::Number(n) => n.as_u64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    let n = u32::try_from(n).ok()?;
    Some(n.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect())
}

// --- descriptors -----------------------------------------------------------

/// The first descriptor in a text file: `#` comment lines dropped (Sparrow's
/// and Coldcard's descriptor files), lines joined until a blank line (so a
/// Coldcard "pretty" multi-line descriptor stays whole).
fn first_descriptor_block(text: &str) -> Option<String> {
    let mut block = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if line.is_empty() {
            if block.contains('(') {
                return Some(block);
            }
            block.clear();
            continue;
        }
        block.push_str(line);
    }
    block.contains('(').then_some(block)
}

fn config_from_descriptor_text(text: &str, network: Network) -> Result<MultisigConfig> {
    match first_descriptor_block(text) {
        Some(d) => config_from_descriptor(&d, network),
        None if text.contains('[') && text.contains("pub") => Err(invalid(COSIGNER_NOT_WALLET)),
        None => Err(invalid(NOT_MULTISIG)),
    }
}

fn config_from_descriptor(raw: &str, network: Network) -> Result<MultisigConfig> {
    let body = prepare_descriptor(raw)?;
    MultisigConfig::from_descriptor(&body, network).map_err(net_err(network))
}

/// A multisig descriptor in the form [`MultisigConfig::from_descriptor`]
/// reads (`<0;1>` or `/0/*` branches, plain xpubs), from any of the
/// spellings coordinators write. A checksum, when present, must match.
fn prepare_descriptor(raw: &str) -> Result<String> {
    let s: String = raw.split_whitespace().collect();
    let body = match s.rsplit_once('#') {
        Some((body, sum)) => {
            let want = desc_checksum(body)
                .map_err(|e| invalid(format!("That descriptor couldn't be read: {e}")))?;
            if sum != want {
                return Err(invalid(
                    "The descriptor's checksum doesn't match, so the file may be damaged or edited. Export it again",
                ));
            }
            body.to_string()
        }
        None => s,
    };
    if !body.contains("multi(") {
        return Err(if body.starts_with("tr(") || body.contains("multi_a(") {
            invalid(TAPROOT)
        } else if ["wpkh(", "pkh(", "sh(wpkh("]
            .iter()
            .any(|p| body.starts_with(p))
        {
            invalid(SINGLE_SIG_WALLET)
        } else {
            invalid(NOT_MULTISIG)
        });
    }
    // BSMS template: `/**` stands for the /0/*,/1/* restrictions.
    let body = body.replace("/**", "/<0;1>/*");
    let body = descriptors::normalize_slip132(&body)
        .map_err(|_| invalid("That descriptor has a key that isn't a valid extended public key"))?;
    if body.contains("<0;1>") {
        return Ok(body);
    }
    let parsed = Descriptor::<DescriptorPublicKey>::from_str(&body)
        .map_err(|e| invalid(format!("That descriptor couldn't be read: {e}")))?;
    #[derive(PartialEq)]
    enum Tail {
        Receive,
        Change,
        AnyWildcard,
        Bare,
        Other,
    }
    let mut tails = Vec::new();
    let mut xpubs = Vec::new();
    let mut single = false;
    parsed.for_each_key(|k| {
        match k {
            DescriptorPublicKey::XPub(x) => {
                let path: Vec<u32> = x
                    .derivation_path
                    .into_iter()
                    .map(|c| u32::from(*c))
                    .collect();
                tails.push(match (path.as_slice(), x.wildcard) {
                    ([0], Wildcard::Unhardened) => Tail::Receive,
                    ([1], Wildcard::Unhardened) => Tail::Change,
                    ([], Wildcard::Unhardened) => Tail::AnyWildcard,
                    ([], Wildcard::None) => Tail::Bare,
                    _ => Tail::Other,
                });
                xpubs.push(x.xkey.to_string());
            }
            DescriptorPublicKey::Single(_) => single = true,
            DescriptorPublicKey::MultiXPub(_) => tails.push(Tail::Other),
        }
        true
    });
    if single {
        return Err(invalid(
            "That wallet uses fixed public keys rather than xpubs, so it only ever has one address. Sessions Wallet needs an xpub multisig",
        ));
    }
    let all = |t: Tail| !tails.is_empty() && tails.iter().all(|x| *x == t);
    if all(Tail::Receive) {
        Ok(body)
    } else if all(Tail::Change) {
        Err(invalid(
            "That's the change descriptor (/1/*). Import the receive one (/0/*) or the <0;1> descriptor",
        ))
    } else if all(Tail::AnyWildcard) {
        // Nunchuk: `xpub/*` means receive /0/*, change /1/*.
        Ok(body.replace("/*", "/<0;1>/*"))
    } else if all(Tail::Bare) {
        // Sparrow's Specter export: account keys with no branch.
        let mut out = body;
        for x in &xpubs {
            out = out.replace(x.as_str(), &format!("{x}/<0;1>/*"));
        }
        Ok(out)
    } else {
        Err(invalid(
            "That descriptor's key paths aren't supported (expected /<0;1>/*, /0/* or /**)",
        ))
    }
}

// --- Coldcard text ---------------------------------------------------------

fn is_fingerprint(s: &str) -> bool {
    s.len() == 8 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn looks_like_coldcard(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        if line.starts_with('#') {
            return false;
        }
        let Some((label, _)) = line.split_once(':') else {
            return false;
        };
        let label = label.trim().to_ascii_lowercase();
        matches!(label.as_str(), "name" | "policy" | "format" | "derivation")
            || is_fingerprint(&label)
    })
}

/// Coldcard drops `#…` to the end of a line unless a digit follows the `#`.
fn strip_trailing_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'#' && !bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            return line[..i].trim_end();
        }
    }
    line
}

/// `M of N`, also `M/N`, `M,N`, `M N` (as Coldcard accepts).
fn parse_policy(value: &str) -> Result<(u8, usize)> {
    let nums: Vec<usize> = value
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    match nums.as_slice() {
        [m, n, ..] if *m <= 255 => Ok((*m as u8, *n)),
        _ => Err(invalid(format!(
            "\"Policy: {value}\" isn't a quorum like \"2 of 3\""
        ))),
    }
}

fn parse_coldcard(text: &str) -> Result<MultisigConfig> {
    let mut name = None;
    let mut policy = None;
    let mut script = ScriptType::P2sh; // Coldcard's default when Format is missing
    let mut sorted = true;
    let mut deriv: Option<String> = None;
    let mut cosigners = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix('#') {
            // BlueWallet / Cobo per-key path: `# derivation: m/48'/0'/0'/2'`.
            if let Some((k, v)) = comment.split_once(':') {
                if k.trim().eq_ignore_ascii_case("derivation") {
                    deriv = Some(v.trim().to_string());
                }
            }
            continue;
        }
        let line = strip_trailing_comment(line);
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        let (label, value) = (label.trim(), value.trim());
        match label.to_ascii_lowercase().as_str() {
            "name" => name = Some(value.to_string()),
            "policy" => policy = Some(parse_policy(value)?),
            "derivation" => deriv = Some(value.to_string()),
            "format" => script = parse_format(value)?,
            // Jade's extension for multi() (unsorted) wallets.
            "sorted" => sorted = !value.eq_ignore_ascii_case("false"),
            "seed" => return Err(invalid(SEED_IN_FILE)),
            fp if is_fingerprint(fp) => {
                let path = deriv.clone().ok_or_else(|| {
                    invalid(format!(
                        "The file doesn't give a derivation path for key {}. Export it again from the coordinator",
                        fp.to_uppercase()
                    ))
                })?;
                cosigners.push(Cosigner {
                    fingerprint: fp.to_string(),
                    path,
                    xpub: value.to_string(),
                    label: None,
                });
            }
            _ => {}
        }
    }
    if cosigners.is_empty() {
        return Err(invalid(NOT_MULTISIG));
    }
    let n = cosigners.len();
    let threshold = match policy {
        Some((m, total)) if total != n => {
            return Err(invalid(format!(
                "The file says {m} of {total}, but lists {n} key{}",
                if n == 1 { "" } else { "s" }
            )))
        }
        Some((m, _)) => m,
        None => u8::try_from(n).map_err(|_| invalid("Too many keys"))?,
    };
    Ok(MultisigConfig {
        name,
        policy: Policy::Multi { threshold, sorted },
        script,
        cosigners,
    })
}

// --- BSMS ------------------------------------------------------------------

fn import_bsms(text: &str, network: Network) -> Result<MultisigConfig> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let version = lines.first().copied().unwrap_or_default();
    if version != "BSMS 1.0" {
        return Err(invalid(format!(
            "\"{version}\" isn't supported – only BSMS 1.0 files can be imported"
        )));
    }
    // Round-1 key record: version, token, key, description, signature.
    if lines.len() >= 4
        && lines[1].chars().all(|c| c.is_ascii_hexdigit())
        && lines[2].starts_with('[')
    {
        return Err(invalid(COSIGNER_NOT_WALLET));
    }
    if lines.len() < 4 {
        return Err(invalid(
            "That BSMS file is incomplete: it needs the descriptor, the path restrictions and the first address",
        ));
    }
    let (descriptor, restrictions, address) = (lines[1], lines[2], lines[3]);
    let compact: String = restrictions.split_whitespace().collect();
    if restrictions.eq_ignore_ascii_case("No path restrictions") {
        if descriptor.contains("/**") {
            return Err(invalid(
                "That BSMS file has a descriptor template but no path restrictions",
            ));
        }
    } else if compact != "/0/*,/1/*" {
        return Err(invalid(format!(
            "That BSMS file limits the wallet to \"{restrictions}\"; only /0/*,/1/* (receive and change) is supported"
        )));
    }
    let config = finish(config_from_descriptor(descriptor, network)?, network)?;
    let ours = config.first_address(network)?;
    if ours != address {
        if let Ok(a) = Address::from_str(address) {
            if !a.is_valid_for_network(network.to_bitcoin()) {
                return Err(wrong_network(network));
            }
        }
        return Err(invalid(format!(
            "The first address in that file ({address}) doesn't match its keys, which give {ours}. Don't use it – ask whoever made it to export it again"
        )));
    }
    Ok(config)
}

// --- JSON ------------------------------------------------------------------

fn import_json(text: &str, network: Network) -> Result<MultisigConfig> {
    let v: Value = serde_json::from_str(text)
        .map_err(|_| invalid("That file couldn't be read – it looks like JSON but isn't valid"))?;
    if v.get("extendedPublicKeys").is_some() {
        return import_caravan(&v, network);
    }
    if let Some(wallet_type) = v.get("wallet_type").and_then(Value::as_str) {
        return import_electrum(&v, wallet_type);
    }
    if let Some(d) = str_field(&v, &["descriptor", "recv_descriptor"]) {
        let mut config = config_from_descriptor(d, network)?;
        config.name = str_field(&v, &["label", "name"]).map(String::from);
        // Specter: devices[i] is key i.
        if let Some(devices) = v.get("devices").and_then(Value::as_array) {
            if devices.len() == config.cosigners.len() {
                for (c, d) in config.cosigners.iter_mut().zip(devices) {
                    c.label = str_field(d, &["label", "name"]).map(String::from);
                }
            }
        }
        return Ok(config);
    }
    let mut found = Found::default();
    if json_candidates(&v, &mut found).is_ok() && !found.keys.is_empty() {
        return Err(invalid(COSIGNER_NOT_WALLET));
    }
    if found.single_sig {
        return Err(invalid(SINGLE_SIG_WALLET));
    }
    Err(invalid(NOT_MULTISIG))
}

fn import_caravan(v: &Value, network: Network) -> Result<MultisigConfig> {
    let script = parse_format(
        str_field(v, &["addressType"])
            .ok_or_else(|| invalid("That Caravan file doesn't say its address type"))?,
    )?;
    match str_field(v, &["network"])
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None => {}
        Some("mainnet") if network != Network::Bitcoin => return Err(wrong_network(network)),
        Some("testnet" | "testnet3" | "testnet4" | "signet" | "regtest")
            if network == Network::Bitcoin =>
        {
            return Err(wrong_network(network))
        }
        Some("mainnet" | "testnet" | "testnet3" | "testnet4" | "signet" | "regtest") => {}
        Some(other) => {
            return Err(invalid(format!(
                "That Caravan file is for an unknown network \"{other}\""
            )))
        }
    }
    let threshold = v
        .get("quorum")
        .and_then(|q| q.get("requiredSigners"))
        .and_then(Value::as_u64)
        .and_then(|m| u8::try_from(m).ok())
        .ok_or_else(|| invalid("That Caravan file doesn't say how many signatures are needed"))?;
    let keys = v
        .get("extendedPublicKeys")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(NOT_MULTISIG))?;
    let mut cosigners = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        let xpub = str_field(k, &["xpub"])
            .ok_or_else(|| invalid(format!("Key {} in that Caravan file has no xpub", i + 1)))?;
        cosigners.push(Cosigner {
            fingerprint: optional_fingerprint(str_field(k, &["xfp"])),
            path: optional_path(str_field(k, &["bip32Path"])).unwrap_or_else(|| "m".into()),
            xpub: xpub.to_string(),
            label: str_field(k, &["name"]).map(String::from),
        });
    }
    if let Some(total) = v
        .get("quorum")
        .and_then(|q| q.get("totalSigners"))
        .and_then(Value::as_u64)
    {
        if total as usize != cosigners.len() {
            return Err(invalid(format!(
                "That Caravan file says {threshold} of {total}, but lists {} keys",
                cosigners.len()
            )));
        }
    }
    Ok(MultisigConfig {
        name: str_field(v, &["name"]).map(String::from),
        // Caravan always sorts keys (BIP67).
        policy: Policy::Multi {
            threshold,
            sorted: true,
        },
        script,
        cosigners,
    })
}

fn import_electrum(v: &Value, wallet_type: &str) -> Result<MultisigConfig> {
    if wallet_type == "standard" || wallet_type == "imported" {
        return Err(invalid(SINGLE_SIG_WALLET));
    }
    let (m, n) = wallet_type
        .split_once("of")
        .and_then(|(m, n)| {
            Some((
                m.trim().parse::<u8>().ok()?,
                n.trim().parse::<usize>().ok()?,
            ))
        })
        .ok_or_else(|| invalid(NOT_MULTISIG))?;
    let mut cosigners = Vec::new();
    let mut script = None;
    for i in 1..=n {
        let ks = v
            .get(format!("x{i}/"))
            .or_else(|| v.get(format!("x{i}")))
            .ok_or_else(|| invalid(format!("That Electrum file is missing key {i} of {n}")))?;
        let xpub = str_field(ks, &["xpub"])
            .ok_or_else(|| invalid(format!("Key {i} in that Electrum file has no xpub")))?;
        if script.is_none() {
            script = Some(match xpub.get(..4).unwrap_or_default() {
                "Zpub" | "Vpub" => ScriptType::P2wsh,
                "Ypub" | "Upub" => ScriptType::P2shP2wsh,
                "xpub" | "tpub" => ScriptType::P2sh,
                _ => return Err(invalid(NOT_MULTISIG)),
            });
        }
        let fingerprint = match str_field(ks, &["root_fingerprint"]) {
            Some(fp) => optional_fingerprint(Some(fp)),
            None => ks
                .get("ckcc_xfp")
                .and_then(ckcc_xfp)
                .unwrap_or_else(|| UNKNOWN_FINGERPRINT.into()),
        };
        cosigners.push(Cosigner {
            fingerprint,
            path: optional_path(str_field(ks, &["derivation"])).unwrap_or_else(|| "m".into()),
            xpub: xpub.to_string(),
            label: str_field(ks, &["label"]).map(String::from),
        });
    }
    Ok(MultisigConfig {
        name: None,
        policy: Policy::Multi {
            threshold: m,
            sorted: true,
        },
        script: script.ok_or_else(|| invalid(NOT_MULTISIG))?,
        cosigners,
    })
}

// --- one cosigner ----------------------------------------------------------

struct Candidate {
    fingerprint: String,
    path: Option<String>,
    xpub: String,
    label: Option<String>,
}

#[derive(Default)]
struct Found {
    keys: Vec<Candidate>,
    /// Saw single-signature accounts (BIP44/49/84/86).
    single_sig: bool,
}

impl Found {
    fn push(&mut self, fingerprint: &str, path: Option<&str>, xpub: &str, label: Option<&str>) {
        let path = optional_path(path);
        if path
            .as_deref()
            .is_some_and(|p| purpose(p).is_some_and(is_single_sig))
        {
            self.single_sig = true;
            return;
        }
        self.keys.push(Candidate {
            fingerprint: fingerprint.trim().to_string(),
            path,
            xpub: xpub.trim().to_string(),
            label: label.map(String::from),
        });
    }
}

/// The first path level, without its hardened marker.
fn purpose(path: &str) -> Option<u32> {
    path.trim()
        .trim_start_matches(['m', 'M'])
        .trim_start_matches('/')
        .split('/')
        .next()?
        .trim_end_matches(['\'', 'h', 'H'])
        .parse()
        .ok()
}

fn is_single_sig(purpose: u32) -> bool {
    matches!(purpose, 44 | 49 | 84 | 86)
}

/// Lower is better: BIP48 P2WSH, BIP48 nested, BIP45, other BIP48, other.
fn rank(path: Option<&str>) -> u8 {
    let Some(path) = path else { return 5 };
    let levels: Vec<&str> = path
        .trim_start_matches(['m', 'M'])
        .trim_start_matches('/')
        .split('/')
        .map(|l| l.trim_end_matches(['\'', 'h', 'H']))
        .collect();
    match levels.as_slice() {
        ["48", _, _, "2"] => 0,
        ["48", _, _, "1"] => 1,
        ["45", ..] => 2,
        ["48", ..] => 3,
        _ => 4,
    }
}

fn json_candidates(v: &Value, found: &mut Found) -> Result<()> {
    let master = str_field(
        v,
        &[
            "xfp",
            "master_fingerprint",
            "MasterFingerprint",
            "root_fingerprint",
        ],
    );
    if let Some(fp) = master {
        // Coldcard / Passport / Keystone multisig export (`ccxp-XFP.json`);
        // older files spell nested segwit `p2wsh_p2sh`.
        for (deriv, key) in [
            ("p2wsh_deriv", "p2wsh"),
            ("p2sh_p2wsh_deriv", "p2sh_p2wsh"),
            ("p2wsh_p2sh_deriv", "p2wsh_p2sh"),
            ("p2sh_deriv", "p2sh"),
        ] {
            if let (Some(d), Some(x)) = (str_field(v, &[deriv]), str_field(v, &[key])) {
                found.push(fp, Some(d), x, None);
            }
        }
        // Coldcard generic export. A section's own `xfp` is that node's
        // fingerprint; the master's is the top-level one.
        for section in ["bip48_2", "bip48_1", "bip45"] {
            if let Some(s) = v.get(section) {
                if let (Some(d), Some(x)) = (str_field(s, &["deriv"]), str_field(s, &["xpub"])) {
                    found.push(fp, Some(d), x, None);
                }
            }
        }
        // Cobo / Keystone / generic one-key JSON.
        if let (Some(x), Some(p)) = (
            str_field(v, &["xpub", "ExtPubKey"]),
            str_field(v, &["path", "deriv", "derivation", "AccountKeyPath"]),
        ) {
            found.push(fp, Some(p), x, str_field(v, &["label", "name"]));
        }
    }
    for section in ["bip44", "bip49", "bip84", "bip86"] {
        if v.get(section).is_some() {
            found.single_sig = true;
        }
    }
    // Electrum keystore file.
    if let Some(ks) = v.get("keystore") {
        if let Some(x) = str_field(ks, &["xpub"]) {
            let fp = match str_field(ks, &["root_fingerprint"]) {
                Some(fp) => fp.to_string(),
                None => ks.get("ckcc_xfp").and_then(ckcc_xfp).unwrap_or_default(),
            };
            found.push(
                &fp,
                str_field(ks, &["derivation"]),
                x,
                str_field(ks, &["label"]),
            );
        }
    }
    if let Some(d) = str_field(v, &["descriptor"]) {
        text_candidates(d, found);
    }
    Ok(())
}

fn text_candidates(text: &str, found: &mut Found) {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    // BSMS round-1 key record: version, token, key, description, signature.
    let bsms_label =
        (lines.len() >= 4 && lines[0].starts_with("BSMS") && lines[2].starts_with('['))
            .then(|| lines[3]);
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find(']') else { break };
        let origin = &rest[..end];
        let key: String = rest[end + 1..]
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        rest = &rest[end + 1..];
        if key.len() < 100 {
            continue;
        }
        let (fp, path) = match origin.split_once('/') {
            Some((fp, path)) => (fp, Some(path)),
            None => (origin, None),
        };
        found.push(fp, path, &key, bsms_label);
    }
}

fn pick_candidate(found: Found, text: &str, network: Network) -> Result<Cosigner> {
    let Found { keys, single_sig } = found;
    if keys.is_empty() {
        return Err(invalid(if single_sig {
            SINGLE_SIG_EXPORT
        } else if !text.contains('[')
            && ["xpub", "tpub", "Zpub", "Vpub", "Ypub", "Upub"]
                .iter()
                .any(|p| text.contains(p))
        {
            MISSING_ORIGIN
        } else {
            NO_KEY
        }));
    }
    let best = keys
        .iter()
        .map(|k| rank(k.path.as_deref()))
        .min()
        .unwrap_or(5);
    let mut fingerprints: Vec<String> = keys
        .iter()
        .filter(|k| rank(k.path.as_deref()) == best)
        .map(|k| k.fingerprint.to_ascii_lowercase())
        .collect();
    fingerprints.sort();
    fingerprints.dedup();
    if fingerprints.len() > 1 || text.contains("multi(") && keys.len() > 1 {
        return Err(invalid(WHOLE_WALLET));
    }
    let k = keys
        .into_iter()
        .find(|k| rank(k.path.as_deref()) == best)
        .ok_or_else(|| invalid(NO_KEY))?;
    if k.fingerprint.is_empty() {
        return Err(invalid(MISSING_ORIGIN));
    }
    let mut c = Cosigner {
        fingerprint: k.fingerprint,
        path: k.path.unwrap_or_else(|| "m".into()),
        xpub: k.xpub,
        label: k.label,
    };
    check_cosigner(&mut c, network)?;
    // Let miniscript check the key too.
    let probe = format!("wsh(pk({}/0/*))", c.key_expression());
    let pair = DescriptorPair {
        external: probe.clone(),
        internal: probe,
    };
    descriptors::validate(&pair)?;
    descriptors::check_network(&pair, network).map_err(net_err(network))?;
    Ok(c)
}

#[cfg(test)]
#[path = "multisig_files_tests.rs"]
mod tests;

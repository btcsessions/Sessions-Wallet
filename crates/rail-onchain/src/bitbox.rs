//! BitBox02 (and Blockstream Jade) helpers that don't talk to the device.
//!
//! The BitBox02 signs PSBTs itself (firmware 9.15+). It finds its own inputs
//! and change outputs by the key origins in the PSBT, and needs the previous
//! transactions of SegWit v0 inputs to check the amounts. This module builds
//! the account descriptor from what the device returns, works out the keypath
//! of a receive address to show on the device, and checks a PSBT has what the
//! device needs before anyone is asked to confirm anything.
//!
//! Jade signs PSBTs the same way (by key origin), so it shares these helpers
//! through [`Device`]; it reads amounts from `witness_utxo` and doesn't need
//! the previous transactions.

use std::str::FromStr;

use bdk_wallet::bitcoin::bip32::{ChildNumber, DerivationPath, Fingerprint, Xpub};
use bdk_wallet::bitcoin::{NetworkKind, Psbt};
use bdk_wallet::miniscript::descriptor::{Descriptor, DescriptorPublicKey};
use bdk_wallet::miniscript::ForEachKey;
use sessions_core::{Error, Network, Result};

use crate::multisig::{bip48_path, Cosigner, MultisigConfig};

const HARDENED: u32 = 0x8000_0000;

/// A signing device, as messages name it and as far as PSBTs are concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Device {
    /// "BitBox02" in "on a BitBox02", "This BitBox02 doesn't…".
    pub name: &'static str,
    /// Starts a sentence about the device: "The BitBox02 returned…".
    pub subject: &'static str,
    /// What to do when it's the wrong device.
    pub use_right_one: &'static str,
    /// Whether SegWit v0 inputs need their full previous transactions.
    pub needs_prev_txs: bool,
    /// Whether it refuses a PSBT with an input it holds no key for (the
    /// BitBox02 does; Jade signs the inputs it can and skips the rest).
    pub needs_every_input: bool,
}

pub const BITBOX02: Device = Device {
    name: "BitBox02",
    subject: "The BitBox02",
    use_right_one: "Plug in the BitBox02 you added it with",
    needs_prev_txs: true,
    needs_every_input: true,
};

pub const JADE: Device = Device {
    name: "Jade",
    subject: "Jade",
    use_right_one: "Connect the Jade you added it with",
    needs_prev_txs: false,
    needs_every_input: false,
};

/// Single-sig address types the wallet adds from a BitBox02.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptKind {
    /// Native SegWit, BIP84.
    Wpkh,
    /// Taproot, BIP86.
    Tr,
}

impl ScriptKind {
    /// `"wpkh"` or `"tr"`, as the UI sends it.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim() {
            "wpkh" => Ok(ScriptKind::Wpkh),
            "tr" => Ok(ScriptKind::Tr),
            other => Err(Error::Invalid(format!(
                "Unknown address type \"{other}\". Use wpkh or tr"
            ))),
        }
    }

    pub fn purpose(self) -> u32 {
        match self {
            ScriptKind::Wpkh => 84,
            ScriptKind::Tr => 86,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            ScriptKind::Wpkh => "wpkh",
            ScriptKind::Tr => "tr",
        }
    }
}

fn coin_type(network: Network) -> u32 {
    if network == Network::Bitcoin {
        0
    } else {
        1
    }
}

/// `m/purpose'/coin'/account'` as the numbers the device takes.
pub fn account_keypath(script: ScriptKind, network: Network, account: u32) -> Result<Vec<u32>> {
    // The BitBox02 only accepts account numbers below 100 for single-sig.
    if account >= 100 {
        return Err(Error::Invalid(
            "The BitBox02 supports account numbers 0 to 99".into(),
        ));
    }
    Ok(vec![
        script.purpose() | HARDENED,
        coin_type(network) | HARDENED,
        account | HARDENED,
    ])
}

/// The account descriptor in the same form the Trezor flow produces:
/// `wpkh([fingerprint/84h/0h/{n}h]xpub…/<0;1>/*)` (`tr` and `86h` for Taproot).
/// `fingerprint` is the device's root fingerprint in hex; `xpub` is the plain
/// xpub/tpub the device returned for [`account_keypath`].
pub fn descriptor(
    script: ScriptKind,
    network: Network,
    fingerprint: &str,
    account: u32,
    xpub: &str,
) -> Result<String> {
    descriptor_on(&BITBOX02, script, network, fingerprint, account, xpub)
}

/// [`descriptor`], for any [`Device`].
pub fn descriptor_on(
    device: &Device,
    script: ScriptKind,
    network: Network,
    fingerprint: &str,
    account: u32,
    xpub: &str,
) -> Result<String> {
    let subject = device.subject;
    let fp = Fingerprint::from_str(fingerprint.trim())
        .map_err(|_| Error::Invalid(format!("{subject} returned an unreadable fingerprint")))?;
    let key = Xpub::from_str(xpub.trim())
        .map_err(|_| Error::Invalid(format!("{subject} returned an unreadable public key")))?;
    let want = if network == Network::Bitcoin {
        NetworkKind::Main
    } else {
        NetworkKind::Test
    };
    if key.network != want {
        return Err(Error::Network(format!(
            "{} returned a key for a different network",
            subject.replacen("The ", "the ", 1)
        )));
    }
    if key.depth != 3 || key.child_number != (ChildNumber::Hardened { index: account }) {
        return Err(Error::Invalid(format!(
            "{subject} returned a key for a different account"
        )));
    }
    Ok(format!(
        "{tag}([{fp}/{purpose}h/{coin}h/{account}h]{key}/<0;1>/*)",
        tag = script.tag(),
        purpose = script.purpose(),
        coin = coin_type(network),
    ))
}

/// Where a receive address lives on the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAddress {
    pub fingerprint: Fingerprint,
    /// Full keypath from the root, e.g. `m/84'/0'/0'/0/5`.
    pub keypath: Vec<u32>,
    pub script: ScriptKind,
}

/// The device keypath of receive address `index` of a single-sig account,
/// from its external descriptor (`…/0/*`).
pub fn receive_path(external: &str, index: u32) -> Result<DeviceAddress> {
    let unsupported = || Error::Unsupported("show its addresses on a BitBox02".into());
    let d = Descriptor::<DescriptorPublicKey>::from_str(external.trim())
        .map_err(|e| Error::Crypto(e.to_string()))?;
    let script = match &d {
        Descriptor::Wpkh(_) => ScriptKind::Wpkh,
        Descriptor::Tr(t) if t.tap_tree().is_none() => ScriptKind::Tr,
        _ => return Err(unsupported()),
    };
    let d = d
        .at_derivation_index(index)
        .map_err(|e| Error::Crypto(e.to_string()))?;
    let mut keys = Vec::new();
    d.for_each_key(|k| {
        keys.push((k.master_fingerprint(), k.full_derivation_path()));
        true
    });
    match keys.as_slice() {
        [(fingerprint, Some(path))] if path.len() == 5 => Ok(DeviceAddress {
            fingerprint: *fingerprint,
            keypath: path.into_iter().map(|c| u32::from(*c)).collect(),
            script,
        }),
        _ => Err(unsupported()),
    }
}

/// Checks that the device with root fingerprint `device` can sign every input
/// of `psbt`, with nothing missing that it needs. The messages are for people.
pub fn check_psbt(psbt: &Psbt, device: Fingerprint) -> Result<()> {
    check_psbt_on(&BITBOX02, psbt, device)
}

/// [`check_psbt`], for any [`Device`] (`fingerprint` is its root fingerprint).
pub fn check_psbt_on(kind: &Device, psbt: &Psbt, device: Fingerprint) -> Result<()> {
    let missing_prev = || {
        Error::Invalid(format!(
            "This transaction is missing details {} needs to check the amounts. \
             Refresh the account and try again",
            kind.subject.replacen("The ", "the ", 1)
        ))
    };
    if psbt.inputs.is_empty() {
        return Err(Error::Invalid("There's nothing to sign".into()));
    }
    for (i, input) in psbt.inputs.iter().enumerate() {
        let utxo = psbt.spend_utxo(i).map_err(|_| missing_prev())?;
        let spk = &utxo.script_pubkey;
        let taproot = spk.is_p2tr();
        let ours = if taproot {
            // Key-path spends only: the internal key must be the device's.
            input.tap_internal_key.is_some_and(|k| {
                input
                    .tap_key_origins
                    .get(&k)
                    .is_some_and(|(leaves, (fp, _))| *fp == device && leaves.is_empty())
            })
        } else {
            input.bip32_derivation.values().any(|(fp, _)| *fp == device)
        };
        if !ours {
            return Err(Error::Invalid(format!(
                "This {} doesn't hold the keys for this account. {}",
                kind.name, kind.use_right_one
            )));
        }
        let nested_wpkh =
            spk.is_p2sh() && input.redeem_script.as_ref().is_some_and(|s| s.is_p2wpkh());
        if !(spk.is_p2wpkh() || taproot || nested_wpkh) {
            return Err(Error::Unsupported(format!(
                "sign this kind of coin on a {}",
                kind.name
            )));
        }
        // SegWit v0 signatures don't commit to input amounts, so the device
        // checks them against the full previous transactions.
        if kind.needs_prev_txs && !taproot && input.non_witness_utxo.is_none() {
            return Err(missing_prev());
        }
    }
    Ok(())
}

/// Whether every input carries a signature (after the device signed).
pub fn fully_signed(psbt: &Psbt) -> bool {
    psbt.inputs
        .iter()
        .all(|i| !i.partial_sigs.is_empty() || i.tap_key_sig.is_some())
}

// --- multisig ---------------------------------------------------------------

/// Longest multisig account name a BitBox02 takes.
pub const BITBOX02_NAME_MAX: usize = 30;
/// Longest multisig registration name Jade takes (its storage keys are 15
/// characters).
pub const JADE_NAME_MAX: usize = 15;

/// `m/48'/coin'/account'/2'` (BIP48 P2WSH) as the numbers devices take.
pub fn multisig_account_keypath(network: Network, account: u32) -> Result<Vec<u32>> {
    if account >= HARDENED {
        return Err(Error::Invalid("That account number is too large".into()));
    }
    Ok(vec![
        48 | HARDENED,
        coin_type(network) | HARDENED,
        account | HARDENED,
        2 | HARDENED,
    ])
}

/// The device's multisig key, `[fingerprint/48h/0h/{account}h/2h]xpub`, from
/// its root fingerprint (hex) and the plain xpub/tpub it returned for
/// [`multisig_account_keypath`].
pub fn multisig_key_on(
    device: &Device,
    network: Network,
    fingerprint: &str,
    account: u32,
    xpub: &str,
) -> Result<String> {
    let subject = device.subject;
    let fp = Fingerprint::from_str(fingerprint.trim())
        .map_err(|_| Error::Invalid(format!("{subject} returned an unreadable fingerprint")))?;
    let key = Xpub::from_str(xpub.trim())
        .map_err(|_| Error::Invalid(format!("{subject} returned an unreadable public key")))?;
    let want = if network == Network::Bitcoin {
        NetworkKind::Main
    } else {
        NetworkKind::Test
    };
    if key.network != want {
        return Err(Error::Network(format!(
            "{} returned a key for a different network",
            subject.replacen("The ", "the ", 1)
        )));
    }
    if key.depth != 4 || key.child_number != (ChildNumber::Hardened { index: 2 }) {
        return Err(Error::Invalid(format!(
            "{subject} returned a key for a different account"
        )));
    }
    let cosigner = Cosigner {
        fingerprint: fp.to_string(),
        path: bip48_path(network, account),
        xpub: key.to_string(),
        label: None,
    };
    Ok(cosigner.key_expression())
}

/// One of a vault's keys that a device may hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultKey {
    /// Position in the vault's key list (descriptor order).
    pub index: usize,
    /// Keypath from the root, e.g. `m/48'/0'/0'/2'`.
    pub keypath: Vec<u32>,
    pub xpub: Xpub,
}

/// "This BitBox02 isn't one of this vault's keys".
pub fn not_a_vault_key(device: &Device) -> Error {
    Error::Invalid(format!(
        "This {} isn't one of this vault's keys",
        device.name
    ))
}

/// The vault's keys carrying the device's root fingerprint (usually one).
/// The device's xpub at the keypath still needs checking against the key.
pub fn vault_keys(
    device: &Device,
    config: &MultisigConfig,
    fingerprint: Fingerprint,
) -> Result<Vec<VaultKey>> {
    let fp = fingerprint.to_string();
    let mut out = Vec::new();
    for (index, c) in config.cosigners.iter().enumerate() {
        if !c.fingerprint.eq_ignore_ascii_case(&fp) {
            continue;
        }
        let path = DerivationPath::from_str(&c.path).map_err(|e| Error::Crypto(e.to_string()))?;
        let xpub = Xpub::from_str(&c.xpub).map_err(|e| Error::Crypto(e.to_string()))?;
        out.push(VaultKey {
            index,
            keypath: path.into_iter().map(|c| u32::from(*c)).collect(),
            xpub,
        });
    }
    if out.is_empty() {
        return Err(not_a_vault_key(device));
    }
    Ok(out)
}

/// Whether two xpubs are the same key (ignoring network and origin metadata).
pub fn same_key(a: &Xpub, b: &Xpub) -> bool {
    a.public_key == b.public_key && a.chain_code == b.chain_code
}

/// Checks a vault (multisig) PSBT before the device is asked to sign: the
/// device holds a key for its inputs (every input, for a device that needs
/// that), the inputs carry their scripts, and SegWit inputs carry their
/// previous transactions where the device needs them.
pub fn check_multisig_psbt(kind: &Device, psbt: &Psbt, device: Fingerprint) -> Result<()> {
    let missing = || {
        Error::Invalid(format!(
            "This transaction is missing details {} needs to check it. \
             Refresh the vault and try again",
            kind.subject.replacen("The ", "the ", 1)
        ))
    };
    if psbt.inputs.is_empty() {
        return Err(Error::Invalid("There's nothing to sign".into()));
    }
    let mut ours_somewhere = false;
    for (i, input) in psbt.inputs.iter().enumerate() {
        let utxo = psbt.spend_utxo(i).map_err(|_| missing())?;
        let ours = input.bip32_derivation.values().any(|(fp, _)| *fp == device);
        if !ours {
            if kind.needs_every_input {
                return Err(not_a_vault_key(kind));
            }
            continue;
        }
        ours_somewhere = true;
        let spk = &utxo.script_pubkey;
        let script_ok = if spk.is_p2wsh() {
            input.witness_script.is_some()
        } else if spk.is_p2sh() {
            input.redeem_script.is_some()
                && (!input.redeem_script.as_ref().is_some_and(|s| s.is_p2wsh())
                    || input.witness_script.is_some())
        } else {
            return Err(Error::Unsupported(format!(
                "sign this kind of coin from a vault on a {}",
                kind.name
            )));
        };
        if !script_ok {
            return Err(missing());
        }
        let segwit = spk.is_p2wsh() || input.witness_script.is_some();
        if (kind.needs_prev_txs || !segwit) && input.non_witness_utxo.is_none() {
            return Err(missing());
        }
    }
    if !ours_somewhere {
        return Err(not_a_vault_key(kind));
    }
    Ok(())
}

/// Whether the device (root fingerprint `device`) signed every input it holds
/// a key for, and there was at least one. Other cosigners' inputs may still
/// need signatures: a vault PSBT is complete only once enough keys signed.
pub fn signed_own_inputs(psbt: &Psbt, device: Fingerprint) -> bool {
    let mut any = false;
    for input in &psbt.inputs {
        let mut ours = input
            .bip32_derivation
            .iter()
            .filter(|(_, (fp, _))| *fp == device)
            .map(|(pk, _)| bdk_wallet::bitcoin::PublicKey::new(*pk))
            .peekable();
        if ours.peek().is_none() {
            continue;
        }
        any = true;
        if !ours.any(|pk| input.partial_sigs.contains_key(&pk)) {
            return false;
        }
    }
    any
}

/// The vault's name as a BitBox02 takes it: printable ASCII, single spaces,
/// at most [`BITBOX02_NAME_MAX`] characters.
pub fn bitbox02_vault_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let c = if c.is_whitespace() { ' ' } else { c };
        if !(c.is_ascii_graphic() || c == ' ')
            || (c == ' ' && (out.is_empty() || out.ends_with(' ')))
        {
            continue;
        }
        if out.len() == BITBOX02_NAME_MAX {
            break;
        }
        out.push(c);
    }
    let out = out.trim_end().to_string();
    if out.is_empty() {
        "Sessions vault".into()
    } else {
        out
    }
}

/// The vault's name as Jade takes it (printable ASCII without spaces, at
/// most [`JADE_NAME_MAX`] characters), made different from the `taken` names
/// of other registrations on the device. Jade overwrites a registration that
/// has the same name, so a clash gets a `-2`, `-3`… suffix instead.
pub fn jade_vault_name(name: &str, taken: &[String]) -> String {
    let mut base = String::new();
    for c in name.chars() {
        let c = if c.is_whitespace() { '_' } else { c };
        if !c.is_ascii_graphic() || (c == '_' && (base.is_empty() || base.ends_with('_'))) {
            continue;
        }
        base.push(c);
    }
    let base = base.trim_end_matches('_');
    let base = if base.is_empty() { "Vault" } else { base };
    let clip = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    let first = clip(base, JADE_NAME_MAX);
    if !taken.contains(&first) {
        return first;
    }
    for n in 2..1000 {
        let suffix = format!("-{n}");
        let candidate = format!("{}{suffix}", clip(base, JADE_NAME_MAX - suffix.len()));
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    first
}

/// A 2-of-3 vault of three test seeds, and a PSBT spending from it.
#[cfg(test)]
pub(crate) mod vault_fixture {
    use std::str::FromStr;

    use bdk_wallet::bitcoin::bip32::Xpriv;
    use bdk_wallet::bitcoin::{
        absolute, transaction, Address, Amount, OutPoint, Psbt, ScriptBuf, Sequence, Transaction,
        TxIn, TxOut, Witness,
    };
    use bdk_wallet::miniscript::descriptor::{Descriptor, DescriptorPublicKey};
    use bdk_wallet::miniscript::psbt::PsbtExt;
    use sessions_core::keystore::Seed;
    use sessions_core::Network;

    use crate::descriptors::{self, DescriptorPair};
    use crate::multisig::MultisigConfig;

    pub const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    pub const LEGAL: &str =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";
    pub const LETTER: &str =
        "letter advice cage absurd amount doctor acoustic avoid letter advice cage above";
    /// Somewhere else to pay (BIP84 receive 0 of "abandon … about").
    pub const PAY_TO: &str = "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu";

    pub fn master(phrase: &str) -> Xpriv {
        Seed::from_phrase(phrase)
            .unwrap()
            .master_xpriv(Network::Bitcoin)
            .unwrap()
    }

    /// `wsh(sortedmulti(2, …))` of the three seeds' BIP48 account 0 keys.
    pub fn vault() -> (MultisigConfig, DescriptorPair) {
        let keys: Vec<String> = [ABANDON, LEGAL, LETTER]
            .iter()
            .map(|p| descriptors::multisig_cosigner_key(&master(p), Network::Bitcoin, 0).unwrap())
            .collect();
        let pair = descriptors::multisig(2, &keys).unwrap();
        let config = MultisigConfig::from_descriptor(&pair.external, Network::Bitcoin).unwrap();
        (config, pair)
    }

    fn at(
        descriptor: &str,
        index: u32,
    ) -> Descriptor<bdk_wallet::miniscript::DefiniteDescriptorKey> {
        Descriptor::<DescriptorPublicKey>::from_str(descriptor)
            .unwrap()
            .at_derivation_index(index)
            .unwrap()
    }

    /// Spends receive address 0 (100 000 sats): 60 000 to [`PAY_TO`], the
    /// rest less fee to change address 3.
    pub fn psbt(pair: &DescriptorPair) -> Psbt {
        let input = at(&pair.external, 0);
        let change = at(&pair.internal, 3);
        let prev = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::from_str(
                    "3131313131313131313131313131313131313131313131313131313131313131:0",
                )
                .unwrap(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::default(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(100_000),
                script_pubkey: input.script_pubkey(),
            }],
        };
        let pay_to = Address::from_str(PAY_TO).unwrap().assume_checked();
        let tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: prev.compute_txid(),
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::default(),
            }],
            output: vec![
                TxOut {
                    value: Amount::from_sat(60_000),
                    script_pubkey: pay_to.script_pubkey(),
                },
                TxOut {
                    value: Amount::from_sat(39_000),
                    script_pubkey: change.script_pubkey(),
                },
            ],
        };
        let mut psbt = Psbt::from_unsigned_tx(tx).unwrap();
        psbt.inputs[0].witness_utxo = Some(prev.output[0].clone());
        psbt.inputs[0].non_witness_utxo = Some(prev);
        psbt.update_input_with_descriptor(0, &input).unwrap();
        psbt.update_output_with_descriptor(1, &change).unwrap();
        psbt
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptors;
    use sessions_core::keystore::Seed;

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    /// BIP84 test vector: `m/84'/0'/0'` of the "abandon … about" seed.
    const BIP84_XPUB: &str = "xpub6CatWdiZiodmUeTDp8LT5or8nmbKNcuyvz7WyksVFkKB4RHwCD3XyuvPEbvqAQY3rAPshWcMLoP2fMFMKHPJ4ZeZXYVUhLv1VMrjPC7PW6V";

    fn xpub_at(path: &str) -> String {
        let master = Seed::from_phrase(ABANDON)
            .unwrap()
            .master_xpriv(Network::Bitcoin)
            .unwrap();
        let key = descriptors::key_expression(&master, path).unwrap();
        key.split_once(']').unwrap().1.to_string()
    }

    #[test]
    fn parses_script_kinds() {
        assert_eq!(ScriptKind::parse("wpkh").unwrap(), ScriptKind::Wpkh);
        assert_eq!(ScriptKind::parse("tr").unwrap(), ScriptKind::Tr);
        assert!(ScriptKind::parse("pkh").is_err());
    }

    #[test]
    fn account_keypaths() {
        assert_eq!(
            account_keypath(ScriptKind::Wpkh, Network::Bitcoin, 3).unwrap(),
            vec![84 | HARDENED, HARDENED, 3 | HARDENED]
        );
        assert_eq!(
            account_keypath(ScriptKind::Tr, Network::Testnet, 0).unwrap(),
            vec![86 | HARDENED, 1 | HARDENED, HARDENED]
        );
        assert!(account_keypath(ScriptKind::Wpkh, Network::Bitcoin, 100).is_err());
    }

    #[test]
    fn descriptor_matches_the_trezor_form() {
        assert_eq!(xpub_at("m/84'/0'/0'"), BIP84_XPUB);
        let d = descriptor(
            ScriptKind::Wpkh,
            Network::Bitcoin,
            "73c5da0a",
            0,
            BIP84_XPUB,
        )
        .unwrap();
        assert_eq!(d, format!("wpkh([73c5da0a/84h/0h/0h]{BIP84_XPUB}/<0;1>/*)"));
        // It imports as the same account the seed itself would make.
        let master = Seed::from_phrase(ABANDON)
            .unwrap()
            .master_xpriv(Network::Bitcoin)
            .unwrap();
        let imported = descriptors::import(&d, Network::Bitcoin).unwrap();
        let hot = descriptors::hot_single_sig(&master, Network::Bitcoin, 0).unwrap();
        assert!(descriptors::same_account(&imported, &hot));
        assert_eq!(
            descriptors::address_at(&imported, Network::Bitcoin, 0).unwrap(),
            "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu"
        );

        let tr_xpub = xpub_at("m/86'/0'/2'");
        let d = descriptor(ScriptKind::Tr, Network::Bitcoin, "73C5DA0A", 2, &tr_xpub).unwrap();
        assert_eq!(d, format!("tr([73c5da0a/86h/0h/2h]{tr_xpub}/<0;1>/*)"));
        let imported = descriptors::import(&d, Network::Bitcoin).unwrap();
        assert_eq!(descriptors::script_label(&imported), "Taproot");
        assert_eq!(descriptors::account_index(&imported.external), Some(2));
    }

    #[test]
    fn descriptors_from_jade_match_the_bitbox_form() {
        let d = descriptor_on(
            &JADE,
            ScriptKind::Wpkh,
            Network::Bitcoin,
            "73c5da0a",
            0,
            BIP84_XPUB,
        )
        .unwrap();
        assert_eq!(
            d,
            descriptor(
                ScriptKind::Wpkh,
                Network::Bitcoin,
                "73c5da0a",
                0,
                BIP84_XPUB
            )
            .unwrap()
        );
        let e = descriptor_on(
            &JADE,
            ScriptKind::Wpkh,
            Network::Bitcoin,
            "73c5da0a",
            1,
            BIP84_XPUB,
        )
        .unwrap_err();
        assert_eq!(e.to_string(), "Jade returned a key for a different account");
        let e = descriptor(
            ScriptKind::Wpkh,
            Network::Testnet,
            "73c5da0a",
            0,
            BIP84_XPUB,
        )
        .unwrap_err();
        assert!(e
            .to_string()
            .contains("the BitBox02 returned a key for a different network"));
    }

    #[test]
    fn descriptor_rejects_mismatched_keys() {
        // Account 0's key labelled as account 1.
        assert!(descriptor(
            ScriptKind::Wpkh,
            Network::Bitcoin,
            "73c5da0a",
            1,
            BIP84_XPUB
        )
        .is_err());
        // Mainnet key on testnet.
        assert!(descriptor(
            ScriptKind::Wpkh,
            Network::Testnet,
            "73c5da0a",
            0,
            BIP84_XPUB
        )
        .is_err());
        assert!(descriptor(
            ScriptKind::Wpkh,
            Network::Bitcoin,
            "nothex!!",
            0,
            BIP84_XPUB
        )
        .is_err());
        assert!(descriptor(ScriptKind::Wpkh, Network::Bitcoin, "73c5da0a", 0, "zpub123").is_err());
    }

    #[test]
    fn receive_paths_from_descriptors() {
        let ext = format!("wpkh([73c5da0a/84h/0h/0h]{BIP84_XPUB}/0/*)");
        let p = receive_path(&ext, 7).unwrap();
        assert_eq!(p.fingerprint.to_string(), "73c5da0a");
        assert_eq!(p.script, ScriptKind::Wpkh);
        assert_eq!(p.keypath, vec![84 | HARDENED, HARDENED, HARDENED, 0, 7]);

        let tr_xpub = xpub_at("m/86'/0'/1'");
        let ext = format!("tr([73c5da0a/86'/0'/1']{tr_xpub}/0/*)");
        let p = receive_path(&ext, 0).unwrap();
        assert_eq!(p.script, ScriptKind::Tr);
        assert_eq!(p.keypath, vec![86 | HARDENED, HARDENED, 1 | HARDENED, 0, 0]);

        // No origin, multisig: can't be shown on the device.
        assert!(receive_path(&format!("wpkh({BIP84_XPUB}/0/*)"), 0).is_err());
        let multi = format!(
            "wsh(sortedmulti(1,[73c5da0a/48h/0h/0h/2h]{BIP84_XPUB}/0/*,[73c5da0a/48h/0h/0h/2h]{BIP84_XPUB}/1/*))"
        );
        assert!(receive_path(&multi, 0).is_err());
    }

    // --- multisig -----------------------------------------------------------

    use super::vault_fixture as fx;

    #[test]
    fn multisig_keys_match_the_vault_form() {
        assert_eq!(
            multisig_account_keypath(Network::Bitcoin, 0).unwrap(),
            vec![48 | HARDENED, HARDENED, HARDENED, 2 | HARDENED]
        );
        assert_eq!(
            multisig_account_keypath(Network::Testnet, 3).unwrap(),
            vec![48 | HARDENED, 1 | HARDENED, 3 | HARDENED, 2 | HARDENED]
        );
        assert!(multisig_account_keypath(Network::Bitcoin, HARDENED).is_err());

        let xpub = xpub_at("m/48'/0'/0'/2'");
        let key = multisig_key_on(&BITBOX02, Network::Bitcoin, "73C5DA0A", 0, &xpub).unwrap();
        assert_eq!(key, format!("[73c5da0a/48h/0h/0h/2h]{xpub}"));
        // The same key the seed itself would give as a cosigner.
        let own = descriptors::multisig_cosigner_key(&fx::master(fx::ABANDON), Network::Bitcoin, 0)
            .unwrap();
        assert_eq!(
            crate::multisig::Cosigner::parse(&key, Network::Bitcoin).unwrap(),
            crate::multisig::Cosigner::parse(&own, Network::Bitcoin).unwrap()
        );
        // A single-sig account key, or one for another network, is refused.
        assert!(multisig_key_on(&JADE, Network::Bitcoin, "73c5da0a", 0, BIP84_XPUB).is_err());
        let e = multisig_key_on(&JADE, Network::Testnet, "73c5da0a", 0, &xpub).unwrap_err();
        assert!(e
            .to_string()
            .contains("Jade returned a key for a different network"));
    }

    #[test]
    fn finds_the_device_among_the_vault_keys() {
        let (config, _) = fx::vault();
        let fp = Fingerprint::from_str("73c5da0a").unwrap();
        let keys = vault_keys(&BITBOX02, &config, fp).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(
            keys[0].keypath,
            vec![48 | HARDENED, HARDENED, HARDENED, 2 | HARDENED]
        );
        assert_eq!(config.cosigners[keys[0].index].fingerprint, "73c5da0a");
        assert!(same_key(
            &keys[0].xpub,
            &Xpub::from_str(&xpub_at("m/48'/0'/0'/2'")).unwrap()
        ));
        let e = vault_keys(&BITBOX02, &config, Fingerprint::from([1, 2, 3, 4])).unwrap_err();
        assert_eq!(
            e.to_string(),
            "This BitBox02 isn't one of this vault's keys"
        );
    }

    #[test]
    fn vault_psbts_are_checked_per_device() {
        let (_, pair) = fx::vault();
        let psbt = fx::psbt(&pair);
        let ours = Fingerprint::from_str("73c5da0a").unwrap();
        check_multisig_psbt(&BITBOX02, &psbt, ours).unwrap();
        check_multisig_psbt(&JADE, &psbt, ours).unwrap();
        let stranger = Fingerprint::from([1, 2, 3, 4]);
        let e = check_multisig_psbt(&JADE, &psbt, stranger).unwrap_err();
        assert_eq!(e.to_string(), "This Jade isn't one of this vault's keys");

        // The BitBox02 checks amounts against the previous transactions.
        let mut stripped = psbt.clone();
        stripped.inputs[0].non_witness_utxo = None;
        assert!(check_multisig_psbt(&BITBOX02, &stripped, ours).is_err());
        check_multisig_psbt(&JADE, &stripped, ours).unwrap();
        // Without the witness script nobody can sign.
        let mut no_script = psbt.clone();
        no_script.inputs[0].witness_script = None;
        assert!(check_multisig_psbt(&JADE, &no_script, ours).is_err());

        // Signed: only once our key's signature is there.
        assert!(!signed_own_inputs(&psbt, ours));
        let mut signed = psbt.clone();
        let (pk, _) = signed.inputs[0]
            .bip32_derivation
            .iter()
            .find(|(_, (fp, _))| *fp == ours)
            .map(|(pk, s)| (*pk, s.clone()))
            .unwrap();
        let secp = bdk_wallet::bitcoin::secp256k1::Secp256k1::new();
        let sk = bdk_wallet::bitcoin::secp256k1::SecretKey::from_slice(&[3; 32]).unwrap();
        let sig = bdk_wallet::bitcoin::ecdsa::Signature::sighash_all(secp.sign_ecdsa(
            &bdk_wallet::bitcoin::secp256k1::Message::from_digest([4; 32]),
            &sk,
        ));
        signed.inputs[0]
            .partial_sigs
            .insert(bdk_wallet::bitcoin::PublicKey::new(pk), sig);
        assert!(signed_own_inputs(&signed, ours));
        // Our signature isn't another cosigner's, and a stranger signed nothing.
        let other = psbt.inputs[0]
            .bip32_derivation
            .values()
            .map(|(fp, _)| *fp)
            .find(|fp| *fp != ours)
            .unwrap();
        assert!(!signed_own_inputs(&signed, other));
        assert!(!signed_own_inputs(&signed, stranger));
    }

    #[test]
    fn vault_names_fit_each_device() {
        assert_eq!(bitbox02_vault_name("Family vault"), "Family vault");
        assert_eq!(
            bitbox02_vault_name("  Ben's   cold\tstorage — 2026 multisig savings "),
            "Ben's cold storage 2026 multis"
        );
        assert_eq!(bitbox02_vault_name("Ünïcödé"), "ncd");
        assert_eq!(bitbox02_vault_name("✨✨"), "Sessions vault");
        assert!(bitbox02_vault_name(&"x".repeat(50)).len() == BITBOX02_NAME_MAX);

        assert_eq!(jade_vault_name("Family vault", &[]), "Family_vault");
        assert_eq!(
            jade_vault_name("Ben's cold storage", &[]),
            "Ben's_cold_stor"
        );
        assert_eq!(jade_vault_name("   ", &[]), "Vault");
        let taken = vec!["Family_vault".to_string(), "Family_vault-2".to_string()];
        assert_eq!(jade_vault_name("Family vault", &taken), "Family_vault-3");
        let long = vec!["Ben's_cold_stor".to_string()];
        assert_eq!(
            jade_vault_name("Ben's cold storage", &long),
            "Ben's_cold_st-2"
        );
        for name in [
            jade_vault_name("Ben's cold storage", &long),
            jade_vault_name("a b\nc", &[]),
        ] {
            assert!(name.len() <= JADE_NAME_MAX);
            assert!(name.chars().all(|c| c.is_ascii_graphic()));
        }
    }
}

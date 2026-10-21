//! BitBox02 helpers that don't talk to the device.
//!
//! The BitBox02 signs PSBTs itself (firmware 9.15+). It finds its own inputs
//! and change outputs by the key origins in the PSBT, and needs the previous
//! transactions of SegWit v0 inputs to check the amounts. This module builds
//! the account descriptor from what the device returns, works out the keypath
//! of a receive address to show on the device, and checks a PSBT has what the
//! device needs before anyone is asked to confirm anything.

use std::str::FromStr;

use bdk_wallet::bitcoin::bip32::{ChildNumber, Fingerprint, Xpub};
use bdk_wallet::bitcoin::{NetworkKind, Psbt};
use bdk_wallet::miniscript::descriptor::{Descriptor, DescriptorPublicKey};
use bdk_wallet::miniscript::ForEachKey;
use sessions_core::{Error, Network, Result};

const HARDENED: u32 = 0x8000_0000;

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
    let fp = Fingerprint::from_str(fingerprint.trim())
        .map_err(|_| Error::Invalid("The BitBox02 returned an unreadable fingerprint".into()))?;
    let key = Xpub::from_str(xpub.trim())
        .map_err(|_| Error::Invalid("The BitBox02 returned an unreadable public key".into()))?;
    let want = if network == Network::Bitcoin {
        NetworkKind::Main
    } else {
        NetworkKind::Test
    };
    if key.network != want {
        return Err(Error::Network(
            "the BitBox02 returned a key for a different network".into(),
        ));
    }
    if key.depth != 3 || key.child_number != (ChildNumber::Hardened { index: account }) {
        return Err(Error::Invalid(
            "The BitBox02 returned a key for a different account".into(),
        ));
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
    let missing_prev = || {
        Error::Invalid(
            "This transaction is missing details the BitBox02 needs to check the amounts. \
             Refresh the account and try again"
                .into(),
        )
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
            return Err(Error::Invalid(
                "This BitBox02 doesn't hold the keys for this account. \
                 Plug in the BitBox02 you added it with"
                    .into(),
            ));
        }
        let nested_wpkh =
            spk.is_p2sh() && input.redeem_script.as_ref().is_some_and(|s| s.is_p2wpkh());
        if !(spk.is_p2wpkh() || taproot || nested_wpkh) {
            return Err(Error::Unsupported(
                "sign this kind of coin on a BitBox02".into(),
            ));
        }
        // SegWit v0 signatures don't commit to input amounts, so the device
        // checks them against the full previous transactions.
        if !taproot && input.non_witness_utxo.is_none() {
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
}

//! Descriptor building and import.
//!
//! The wallet database only ever stores **public** descriptors. Private keys
//! stay in the encrypted vault and are only used to sign PSBTs in memory.

use std::str::FromStr;

use bdk_wallet::bitcoin::bip32::{DerivationPath, Xpriv, Xpub};
use bdk_wallet::bitcoin::secp256k1::Secp256k1;
use bdk_wallet::bitcoin::NetworkKind;
use bdk_wallet::miniscript::descriptor::{Descriptor, DescriptorPublicKey};
use bdk_wallet::miniscript::ForEachKey;
use sessions_core::{Error, Network, Result};

/// External and internal (change) descriptor pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorPair {
    pub external: String,
    pub internal: String,
}

fn coin_type(network: Network) -> u32 {
    if network == Network::Bitcoin {
        0
    } else {
        1
    }
}

/// `[fingerprint/path]xpub` for `master` at `path`.
pub fn key_expression(master: &Xpriv, path: &str) -> Result<String> {
    let secp = Secp256k1::new();
    let path = DerivationPath::from_str(path).map_err(|e| Error::Crypto(e.to_string()))?;
    let derived = master.derive_priv(&secp, &path).map_err(|e| Error::Crypto(e.to_string()))?;
    let xpub = Xpub::from_priv(&secp, &derived);
    let fp = master.fingerprint(&secp);
    let origin = path.to_string();
    let origin = origin.trim_start_matches("m/").trim_start_matches('m');
    Ok(format!("[{fp}/{origin}]{xpub}"))
}

/// BIP84 native-segwit single-sig account from the app's own seed.
pub fn hot_single_sig(master: &Xpriv, network: Network, account: u32) -> Result<DescriptorPair> {
    let key = key_expression(master, &format!("m/84'/{}'/{account}'", coin_type(network)))?;
    Ok(DescriptorPair { external: format!("wpkh({key}/0/*)"), internal: format!("wpkh({key}/1/*)") })
}

/// Our cosigner key for a BIP48 P2WSH multisig (`m/48'/coin'/account'/2'`).
/// Share this with the other cosigners / hardware wallets.
pub fn multisig_cosigner_key(master: &Xpriv, network: Network, account: u32) -> Result<String> {
    key_expression(master, &format!("m/48'/{}'/{account}'/2'", coin_type(network)))
}

/// `wsh(sortedmulti(...))` from `[origin]xpub` key expressions.
pub fn multisig(threshold: u8, keys: &[String]) -> Result<DescriptorPair> {
    if threshold == 0 || threshold as usize > keys.len() || keys.len() > 15 {
        return Err(Error::Unsupported(format!("make a {threshold}-of-{} multisig", keys.len())));
    }
    let mut sorted = keys.to_vec();
    sorted.sort();
    sorted.dedup();
    if sorted.len() != keys.len() {
        return Err(Error::Unsupported("use the same cosigner key twice".into()));
    }
    let list = |branch: u8| sorted.iter().map(|k| format!("{k}/{branch}/*")).collect::<Vec<_>>().join(",");
    let pair = DescriptorPair {
        external: format!("wsh(sortedmulti({threshold},{}))", list(0)),
        internal: format!("wsh(sortedmulti({threshold},{}))", list(1)),
    };
    validate(&pair)?;
    Ok(pair)
}

/// Import whatever a hardware wallet or coordinator exports:
///
/// * a full descriptor, optionally with a `<0;1>` multipath branch
///   (Sparrow, Coldcard, Nunchuk, Bitcoin Core all export these),
/// * an external descriptor ending in `/0/*` (change is inferred as `/1/*`),
/// * a bare `[fingerprint/84'/0'/0']xpub…` key, treated as BIP84 single-sig.
pub fn import(input: &str, network: Network) -> Result<DescriptorPair> {
    let s: String = input.split_whitespace().collect();
    // Drop a trailing checksum; miniscript recomputes it.
    let s = match s.rsplit_once('#') {
        Some((d, sum)) if sum.len() == 8 => d.to_string(),
        _ => s,
    };

    let pair = if s.contains("<0;1>") {
        DescriptorPair { external: s.replace("<0;1>", "0"), internal: s.replace("<0;1>", "1") }
    } else if s.contains('(') {
        if !s.contains("/0/*") {
            return Err(Error::Unsupported("import a descriptor without a /0/* receive branch".into()));
        }
        DescriptorPair { external: s.clone(), internal: s.replace("/0/*", "/1/*") }
    } else if s.starts_with('[') {
        DescriptorPair { external: format!("wpkh({s}/0/*)"), internal: format!("wpkh({s}/1/*)") }
    } else {
        return Err(Error::Unsupported(
            "import a bare xpub without its [fingerprint/path] origin – export the descriptor from your device instead".into(),
        ));
    };

    validate(&pair)?;
    check_network(&pair, network)?;
    Ok(pair)
}

pub fn validate(pair: &DescriptorPair) -> Result<()> {
    for d in [&pair.external, &pair.internal] {
        let parsed = Descriptor::<DescriptorPublicKey>::from_str(d).map_err(|e| Error::Crypto(format!("bad descriptor: {e}")))?;
        if !parsed.has_wildcard() {
            return Err(Error::Unsupported("use a descriptor without a /* wildcard".into()));
        }
    }
    Ok(())
}

fn check_network(pair: &DescriptorPair, network: Network) -> Result<()> {
    let want = if network == Network::Bitcoin { NetworkKind::Main } else { NetworkKind::Test };
    let parsed = Descriptor::<DescriptorPublicKey>::from_str(&pair.external).map_err(|e| Error::Crypto(e.to_string()))?;
    let mut ok = true;
    parsed.for_each_key(|k| {
        if let DescriptorPublicKey::XPub(x) = k {
            ok &= x.xkey.network == want;
        }
        if let DescriptorPublicKey::MultiXPub(x) = k {
            ok &= x.xkey.network == want;
        }
        true
    });
    if ok {
        Ok(())
    } else {
        Err(Error::Network("these keys are for a different network".into()))
    }
}

/// Fingerprints of every key in a descriptor, for matching signers.
pub fn fingerprints(descriptor: &str) -> Result<Vec<String>> {
    let parsed = Descriptor::<DescriptorPublicKey>::from_str(descriptor).map_err(|e| Error::Crypto(e.to_string()))?;
    let mut out = Vec::new();
    parsed.for_each_key(|k| {
        out.push(k.master_fingerprint().to_string());
        true
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessions_core::keystore::Seed;

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn master() -> Xpriv {
        Seed::from_phrase(ABANDON).unwrap().master_xpriv(Network::Bitcoin).unwrap()
    }

    /// Known BIP84 vector for the "abandon … about" phrase.
    #[test]
    fn bip84_matches_reference_vector() {
        let pair = hot_single_sig(&master(), Network::Bitcoin, 0).unwrap();
        assert_eq!(
            pair.external,
            "wpkh([73c5da0a/84'/0'/0']xpub6CatWdiZiodmUeTDp8LT5or8nmbKNcuyvz7WyksVFkKB4RHwCD3XyuvPEbvqAQY3rAPshWcMLoP2fMFMKHPJ4ZeZXYVUhLv1VMrjPC7PW6V/0/*)"
        );
    }

    #[test]
    fn multisig_is_sorted_and_valid() {
        let m = master();
        let a = multisig_cosigner_key(&m, Network::Bitcoin, 0).unwrap();
        let b = multisig_cosigner_key(&m, Network::Bitcoin, 1).unwrap();
        let c = multisig_cosigner_key(&m, Network::Bitcoin, 2).unwrap();
        let p1 = multisig(2, &[a.clone(), b.clone(), c.clone()]).unwrap();
        let p2 = multisig(2, &[c, a.clone(), b]).unwrap();
        assert_eq!(p1, p2);
        assert!(p1.external.starts_with("wsh(sortedmulti(2,"));
        assert!(multisig(4, &[a.clone()]).is_err());
        assert!(multisig(1, &[a.clone(), a]).is_err());
    }

    #[test]
    fn import_multipath_and_bare_key() {
        let key = key_expression(&master(), "m/84'/0'/0'").unwrap();
        let multipath = format!("wpkh({key}/<0;1>/*)");
        let pair = import(&multipath, Network::Bitcoin).unwrap();
        assert!(pair.external.ends_with("/0/*)") && pair.internal.ends_with("/1/*)"));
        assert_eq!(import(&key, Network::Bitcoin).unwrap(), pair);
    }

    #[test]
    fn import_rejects_wrong_network_and_naked_xpub() {
        let key = key_expression(&master(), "m/84'/0'/0'").unwrap();
        assert!(matches!(import(&key, Network::Signet), Err(Error::Network(_))));
        let naked = key.split(']').nth(1).unwrap();
        assert!(import(naked, Network::Bitcoin).is_err());
    }
}

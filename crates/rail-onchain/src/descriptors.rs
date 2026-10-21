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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    let derived = master
        .derive_priv(&secp, &path)
        .map_err(|e| Error::Crypto(e.to_string()))?;
    let xpub = Xpub::from_priv(&secp, &derived);
    let fp = master.fingerprint(&secp);
    let origin = path.to_string();
    let origin = origin.trim_start_matches("m/").trim_start_matches('m');
    Ok(format!("[{fp}/{origin}]{xpub}"))
}

/// BIP84 native-segwit single-sig account from the app's own seed.
pub fn hot_single_sig(master: &Xpriv, network: Network, account: u32) -> Result<DescriptorPair> {
    let key = key_expression(master, &format!("m/84'/{}'/{account}'", coin_type(network)))?;
    Ok(DescriptorPair {
        external: format!("wpkh({key}/0/*)"),
        internal: format!("wpkh({key}/1/*)"),
    })
}

/// Our cosigner key for a BIP48 P2WSH multisig (`m/48'/coin'/account'/2'`).
/// Share this with the other cosigners / hardware wallets.
pub fn multisig_cosigner_key(master: &Xpriv, network: Network, account: u32) -> Result<String> {
    key_expression(
        master,
        &format!("m/48'/{}'/{account}'/2'", coin_type(network)),
    )
}

/// `wsh(sortedmulti(...))` from `[origin]xpub` key expressions.
pub fn multisig(threshold: u8, keys: &[String]) -> Result<DescriptorPair> {
    if threshold == 0 || threshold as usize > keys.len() || keys.len() > 15 {
        return Err(Error::Unsupported(format!(
            "make a {threshold}-of-{} multisig",
            keys.len()
        )));
    }
    let mut sorted = keys.to_vec();
    sorted.sort();
    sorted.dedup();
    if sorted.len() != keys.len() {
        return Err(Error::Unsupported("use the same cosigner key twice".into()));
    }
    let list = |branch: u8| {
        sorted
            .iter()
            .map(|k| format!("{k}/{branch}/*"))
            .collect::<Vec<_>>()
            .join(",")
    };
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
/// * a key with its origin, `[fingerprint/84'/0'/0']xpub…` (SeedSigner,
///   Specter format). SLIP-132 `zpub`/`ypub` keys are accepted; the script
///   type comes from the key prefix or the BIP44/49/84/86 path,
/// * a wallet-export JSON (`{"descriptor": …}` from Sparrow/Specter, or the
///   Coldcard/Passport generic export with `bip84` etc. sections).
pub fn import(input: &str, network: Network) -> Result<DescriptorPair> {
    let trimmed = input.trim();
    if trimmed.starts_with('{') {
        return import(&from_json(trimmed)?, network);
    }
    let s: String = trimmed.split_whitespace().collect();
    // Drop a trailing checksum; miniscript recomputes it.
    let s = match s.rsplit_once('#') {
        Some((d, sum)) if sum.len() == 8 => d.to_string(),
        _ => s,
    };
    let s = normalize_keys(&s)?;

    let pair = if s.contains('(') {
        if s.contains("<0;1>") {
            DescriptorPair {
                external: s.replace("<0;1>", "0"),
                internal: s.replace("<0;1>", "1"),
            }
        } else if s.contains("/0/*") {
            DescriptorPair {
                external: s.clone(),
                internal: s.replace("/0/*", "/1/*"),
            }
        } else {
            return Err(Error::Unsupported(
                "import a descriptor without a /0/* receive branch".into(),
            ));
        }
    } else if s.starts_with('[') {
        let (key, script) = origin_key(&s, trimmed)?;
        let wrap = |branch: u8| match script {
            Script::Wpkh => format!("wpkh({key}/{branch}/*)"),
            Script::ShWpkh => format!("sh(wpkh({key}/{branch}/*))"),
            Script::Tr => format!("tr({key}/{branch}/*)"),
            Script::Pkh => format!("pkh({key}/{branch}/*)"),
        };
        DescriptorPair {
            external: wrap(0),
            internal: wrap(1),
        }
    } else {
        return Err(Error::Unsupported(
            "import a bare xpub without its [fingerprint/path] origin – export the descriptor from your device instead".into(),
        ));
    };

    validate(&pair)?;
    check_network(&pair, network)?;
    Ok(pair)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Script {
    Wpkh,
    ShWpkh,
    Tr,
    Pkh,
}

/// `[fp/path]key[/0/*|/<0;1>/*]` → (`[fp/path]xpub`, script type).
fn origin_key(s: &str, original: &str) -> Result<(String, Script)> {
    let key = s
        .trim_end_matches("/<0;1>/*")
        .trim_end_matches("/0/*")
        .to_string();
    let (origin, _) = key
        .trim_start_matches('[')
        .split_once(']')
        .ok_or_else(|| Error::Invalid("the key origin is missing its closing ]".into()))?;
    let purpose = origin
        .split('/')
        .nth(1)
        .map(|p| p.trim_end_matches(['\'', 'h', 'H']));
    // The original prefix says what the device meant, when it's SLIP-132.
    let prefix = original
        .split(']')
        .nth(1)
        .map(|k| k.get(..4).unwrap_or_default().to_string())
        .unwrap_or_default();
    let script = match (prefix.as_str(), purpose) {
        ("zpub" | "vpub", _) => Script::Wpkh,
        ("ypub" | "upub", _) => Script::ShWpkh,
        (_, Some("84")) => Script::Wpkh,
        (_, Some("86")) => Script::Tr,
        (_, Some("49")) => Script::ShWpkh,
        (_, Some("44")) => Script::Pkh,
        (_, Some("48" | "45")) => {
            return Err(Error::Unsupported(
                "import a multisig key here – add it to a Vault instead".into(),
            ))
        }
        _ => Script::Wpkh,
    };
    Ok((key, script))
}

/// Replace SLIP-132 keys (`zpub`, `ypub`, `vpub`…) with plain `xpub`/`tpub`.
fn normalize_keys(s: &str) -> Result<String> {
    use bdk_wallet::bitcoin::base58;
    const SLIP132: [(&str, [u8; 4]); 8] = [
        ("ypub", [0x04, 0x88, 0xb2, 0x1e]),
        ("zpub", [0x04, 0x88, 0xb2, 0x1e]),
        ("Ypub", [0x04, 0x88, 0xb2, 0x1e]),
        ("Zpub", [0x04, 0x88, 0xb2, 0x1e]),
        ("upub", [0x04, 0x35, 0x87, 0xcf]),
        ("vpub", [0x04, 0x35, 0x87, 0xcf]),
        ("Upub", [0x04, 0x35, 0x87, 0xcf]),
        ("Vpub", [0x04, 0x35, 0x87, 0xcf]),
    ];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some((i, version)) = SLIP132
        .iter()
        .filter_map(|(p, v)| rest.find(p).map(|i| (i, *v)))
        .min_by_key(|(i, _)| *i)
    {
        out.push_str(&rest[..i]);
        let end = rest[i..]
            .find(|c: char| !c.is_ascii_alphanumeric())
            .map_or(rest.len(), |e| i + e);
        let mut data = base58::decode_check(&rest[i..end])
            .map_err(|e| Error::Crypto(format!("bad extended key: {e}")))?;
        if data.len() != 78 {
            return Err(Error::Crypto("bad extended key length".into()));
        }
        data[..4].copy_from_slice(&version);
        out.push_str(&base58::encode_check(&data));
        rest = &rest[end..];
    }
    out.push_str(rest);
    Ok(out)
}

/// A descriptor or origin key from a wallet-export JSON file.
fn from_json(s: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(s)
        .map_err(|e| Error::Invalid(format!("unreadable wallet file: {e}")))?;
    if let Some(d) = v.get("descriptor").and_then(|d| d.as_str()) {
        return Ok(d.to_string());
    }
    // Coldcard / Passport / Keystone "generic JSON".
    let xfp = v
        .get("xfp")
        .and_then(|x| x.as_str())
        .ok_or_else(|| Error::Invalid("this wallet file has no descriptor or xfp".into()))?
        .to_lowercase();
    for section in ["bip84", "bip86", "bip49", "bip44"] {
        let Some(sec) = v.get(section) else { continue };
        let (Some(xpub), Some(deriv)) = (
            sec.get("xpub").and_then(|x| x.as_str()),
            sec.get("deriv").and_then(|x| x.as_str()),
        ) else {
            continue;
        };
        let path = deriv.trim_start_matches("m/");
        let key = format!("[{xfp}/{path}]{xpub}");
        return Ok(match section {
            "bip84" => format!("wpkh({key}/<0;1>/*)"),
            "bip86" => format!("tr({key}/<0;1>/*)"),
            "bip49" => format!("sh(wpkh({key}/<0;1>/*))"),
            _ => format!("pkh({key}/<0;1>/*)"),
        });
    }
    Err(Error::Invalid(
        "this wallet file has no single-signature account".into(),
    ))
}

pub fn validate(pair: &DescriptorPair) -> Result<()> {
    for d in [&pair.external, &pair.internal] {
        let parsed = Descriptor::<DescriptorPublicKey>::from_str(d)
            .map_err(|e| Error::Crypto(format!("bad descriptor: {e}")))?;
        if !parsed.has_wildcard() {
            return Err(Error::Unsupported(
                "use a descriptor without a /* wildcard".into(),
            ));
        }
    }
    Ok(())
}

fn check_network(pair: &DescriptorPair, network: Network) -> Result<()> {
    let want = if network == Network::Bitcoin {
        NetworkKind::Main
    } else {
        NetworkKind::Test
    };
    let parsed = Descriptor::<DescriptorPublicKey>::from_str(&pair.external)
        .map_err(|e| Error::Crypto(e.to_string()))?;
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
        Err(Error::Network(
            "these keys are for a different network".into(),
        ))
    }
}

/// Receive address `index` of an imported wallet, to check against the device.
pub fn address_at(pair: &DescriptorPair, network: Network, index: u32) -> Result<String> {
    let d = Descriptor::<DescriptorPublicKey>::from_str(&pair.external)
        .map_err(|e| Error::Crypto(e.to_string()))?;
    let d = d
        .at_derivation_index(index)
        .map_err(|e| Error::Crypto(e.to_string()))?;
    d.address(network.to_bitcoin())
        .map(|a| a.to_string())
        .map_err(|e| Error::Crypto(e.to_string()))
}

/// Plain name for the script type.
pub fn script_label(pair: &DescriptorPair) -> &'static str {
    let e = pair.external.as_str();
    if e.starts_with("wpkh(") {
        "Native SegWit"
    } else if e.starts_with("tr(") {
        "Taproot"
    } else if e.starts_with("sh(wpkh(") {
        "Nested SegWit"
    } else if e.starts_with("pkh(") {
        "Legacy"
    } else if e.contains("multi(") {
        "Multisig"
    } else {
        "Custom"
    }
}

/// Fingerprints of every key in a descriptor, for matching signers.
pub fn fingerprints(descriptor: &str) -> Result<Vec<String>> {
    let parsed = Descriptor::<DescriptorPublicKey>::from_str(descriptor)
        .map_err(|e| Error::Crypto(e.to_string()))?;
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
        Seed::from_phrase(ABANDON)
            .unwrap()
            .master_xpriv(Network::Bitcoin)
            .unwrap()
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
        assert!(multisig(4, std::slice::from_ref(&a)).is_err());
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
    fn import_slip132_paths_and_json() {
        let want = import(
            &key_expression(&master(), "m/84'/0'/0'").unwrap(),
            Network::Bitcoin,
        )
        .unwrap();
        // SeedSigner / Specter style, with a zpub.
        let zpub = "[73c5da0a/84h/0h/0h]zpub6rFR7y4Q2AijBEqTUquhVz398htDFrtymD9xYYfG1m4wAcvPhXNfE3EfH1r1ADqtfSdVCToUG868RvUUkgDKf31mGDtKsAYz2oz2AGutZYs";
        let got = import(zpub, Network::Bitcoin).unwrap();
        assert_eq!(
            got.external.replace("84h/0h/0h", "84'/0'/0'"),
            want.external
        );
        // Coldcard generic JSON.
        let json = r#"{"xfp":"73C5DA0A","bip84":{"xpub":"xpub6CatWdiZiodmUeTDp8LT5or8nmbKNcuyvz7WyksVFkKB4RHwCD3XyuvPEbvqAQY3rAPshWcMLoP2fMFMKHPJ4ZeZXYVUhLv1VMrjPC7PW6V","deriv":"m/84'/0'/0'","_pub":"zpub"}}"#;
        assert_eq!(import(json, Network::Bitcoin).unwrap(), want);
        // Sparrow JSON with a multipath descriptor.
        let sparrow = format!(
            r#"{{"label":"x","descriptor":"{}"}}"#,
            want.external.replace("/0/*", "/<0;1>/*")
        );
        assert_eq!(import(&sparrow, Network::Bitcoin).unwrap(), want);
        // Taproot by path; multisig keys are refused.
        let tr = key_expression(&master(), "m/86'/0'/0'").unwrap();
        assert!(import(&tr, Network::Bitcoin)
            .unwrap()
            .external
            .starts_with("tr("));
        let ms = key_expression(&master(), "m/48'/0'/0'/2'").unwrap();
        assert!(import(&ms, Network::Bitcoin).is_err());
    }

    #[test]
    fn import_rejects_wrong_network_and_naked_xpub() {
        let key = key_expression(&master(), "m/84'/0'/0'").unwrap();
        assert!(matches!(
            import(&key, Network::Signet),
            Err(Error::Network(_))
        ));
        let naked = key.split(']').nth(1).unwrap();
        assert!(import(naked, Network::Bitcoin).is_err());
    }
}

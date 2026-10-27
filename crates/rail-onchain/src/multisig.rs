//! Multisig wallets: the configuration (quorum, cosigner keys, script type),
//! building its descriptors, and reading / writing the wallet files other
//! coordinators use (Sparrow, Nunchuk, Caravan, BlueWallet, Specter, BSMS,
//! the generic Coldcard-style text file that Keystone, Passport and Jade
//! also read).
//!
//! The descriptor pair stays the source of truth for the wallet database;
//! [`MultisigConfig`] is the structured view of it. Today that's
//! `sortedmulti` (BIP67) under P2WSH, created with BIP48 keys
//! (`m/48'/coin'/account'/2'`); importing also takes P2SH-P2WSH and P2SH.
//! The [`Policy`] enum leaves room for miniscript (timelocked recovery,
//! decaying quorums) later.

use std::str::FromStr;

use bdk_wallet::miniscript::descriptor::{Descriptor, DescriptorPublicKey, ShInner, WshInner};
use bdk_wallet::miniscript::ForEachKey;
use serde::{Deserialize, Serialize};
use sessions_core::{Error, Network, Result};

use crate::descriptors::{self, DescriptorPair};

/// Largest quorum (`sortedmulti` under P2WSH allows 20; wallets agree on 15).
pub const MAX_KEYS: usize = 15;

/// The BIP48 path for a P2WSH multisig key: `m/48'/coin'/account'/2'`.
pub fn bip48_path(network: Network, account: u32) -> String {
    let coin = if network == Network::Bitcoin { 0 } else { 1 };
    format!("m/48'/{coin}'/{account}'/2'")
}

/// One cosigner's key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cosigner {
    /// Master key fingerprint, 8 lowercase hex characters.
    pub fingerprint: String,
    /// Derivation path from the master key, `m/48'/0'/0'/2'` style
    /// (apostrophes, `m/` prefix).
    pub path: String,
    /// The account xpub (`xpub…` / `tpub…`, never SLIP-132).
    pub xpub: String,
    /// What the user calls this key, when a file carried a name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Cosigner {
    /// `[fingerprint/48h/0h/0h/2h]xpub`.
    pub fn key_expression(&self) -> String {
        let origin = self
            .path
            .trim_start_matches("m/")
            .trim_start_matches('m')
            .replace('\'', "h");
        if origin.is_empty() {
            format!("[{}]{}", self.fingerprint, self.xpub)
        } else {
            format!("[{}/{}]{}", self.fingerprint, origin, self.xpub)
        }
    }

    /// From a key expression `[fp/path]xpub` (`h` or `'`, SLIP-132
    /// `Zpub`/`Vpub`… accepted and converted, a trailing `/<0;1>/*` or
    /// `/0/*` ignored).
    pub fn parse(expr: &str, network: Network) -> Result<Self> {
        let s: String = expr.split_whitespace().collect();
        let s = s
            .trim_end_matches("/<0;1>/*")
            .trim_end_matches("/0/*")
            .trim_end_matches("/*");
        let s = descriptors::normalize_slip132(s)?;
        let rest = s.strip_prefix('[').ok_or_else(|| {
            Error::Invalid(
                "That key is missing its [fingerprint/path] origin. Export it from the device's multisig option".into(),
            )
        })?;
        let (origin, xpub) = rest
            .split_once(']')
            .ok_or_else(|| Error::Invalid("The key origin is missing its closing ]".into()))?;
        let mut parts = origin.split('/');
        let fingerprint = parts.next().unwrap_or_default().to_ascii_lowercase();
        if fingerprint.len() != 8 || !fingerprint.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::Invalid(format!(
                "\"{fingerprint}\" isn't a key fingerprint"
            )));
        }
        let levels: Vec<String> = parts
            .map(|p| p.replace(['h', 'H'], "'"))
            .collect();
        let path = if levels.is_empty() {
            "m".to_string()
        } else {
            format!("m/{}", levels.join("/"))
        };
        let key = Cosigner {
            fingerprint,
            path,
            xpub: xpub.to_string(),
            label: None,
        };
        // Let miniscript check the key and its network.
        let probe = format!("wsh(pk({}/0/*))", key.key_expression());
        let pair = DescriptorPair {
            external: probe.clone(),
            internal: probe,
        };
        descriptors::validate(&pair)?;
        descriptors::check_network(&pair, network)?;
        Ok(key)
    }
}

/// How the keys combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScriptType {
    /// Native SegWit, `wsh(...)`: what we create.
    P2wsh,
    /// Nested SegWit, `sh(wsh(...))`: imported only.
    P2shP2wsh,
    /// Legacy, `sh(...)`: imported only.
    P2sh,
}

impl ScriptType {
    /// The BIP48 script-type level (`2'` native, `1'` nested); legacy P2SH
    /// uses BIP45 instead.
    pub fn bip48_level(self) -> Option<u32> {
        match self {
            ScriptType::P2wsh => Some(2),
            ScriptType::P2shP2wsh => Some(1),
            ScriptType::P2sh => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ScriptType::P2wsh => "Native SegWit (P2WSH)",
            ScriptType::P2shP2wsh => "Nested SegWit (P2SH-P2WSH)",
            ScriptType::P2sh => "Legacy (P2SH)",
        }
    }
}

/// The spending policy. Only threshold multisig for now; miniscript
/// policies (a recovery key after a timelock, a quorum that relaxes over
/// time) slot in here later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum Policy {
    /// `threshold` of the keys, keys sorted per BIP67 (`sortedmulti`) or in
    /// the order given (`multi`, imported only).
    Multi { threshold: u8, sorted: bool },
}

/// A multisig wallet's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MultisigConfig {
    /// The wallet's name, when a file carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub policy: Policy,
    pub script: ScriptType,
    pub cosigners: Vec<Cosigner>,
}

impl MultisigConfig {
    /// A new `threshold`-of-N `wsh(sortedmulti)` wallet.
    pub fn sorted_p2wsh(threshold: u8, cosigners: Vec<Cosigner>) -> Self {
        MultisigConfig {
            name: None,
            policy: Policy::Multi {
                threshold,
                sorted: true,
            },
            script: ScriptType::P2wsh,
            cosigners,
        }
    }

    pub fn threshold(&self) -> u8 {
        match self.policy {
            Policy::Multi { threshold, .. } => threshold,
        }
    }

    pub fn total(&self) -> u8 {
        self.cosigners.len() as u8
    }

    /// Quorum and keys make sense: 1 ≤ M ≤ N ≤ 15, N ≥ 2, no key twice, every
    /// key valid for `network`.
    pub fn validate(&self, network: Network) -> Result<()> {
        let n = self.cosigners.len();
        let m = self.threshold() as usize;
        if n < 2 {
            return Err(Error::Invalid("A multisig wallet needs at least 2 keys".into()));
        }
        if n > MAX_KEYS {
            return Err(Error::Invalid(format!(
                "A multisig wallet can have at most {MAX_KEYS} keys"
            )));
        }
        if m == 0 || m > n {
            return Err(Error::Invalid(format!(
                "Signatures needed must be between 1 and {n}"
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for c in &self.cosigners {
            if !seen.insert(c.xpub.clone()) {
                return Err(Error::Invalid(format!(
                    "The key {} is in the wallet twice",
                    c.fingerprint
                )));
            }
        }
        let pair = self.descriptors()?;
        descriptors::validate(&pair)?;
        descriptors::check_network(&pair, network)?;
        Ok(())
    }

    /// The receive and change descriptors (no checksums).
    pub fn descriptors(&self) -> Result<DescriptorPair> {
        let Policy::Multi { threshold, sorted } = self.policy;
        let func = if sorted { "sortedmulti" } else { "multi" };
        let inner = |branch: u8| {
            let keys = self
                .cosigners
                .iter()
                .map(|c| format!("{}/{branch}/*", c.key_expression()))
                .collect::<Vec<_>>()
                .join(",");
            let multi = format!("{func}({threshold},{keys})");
            match self.script {
                ScriptType::P2wsh => format!("wsh({multi})"),
                ScriptType::P2shP2wsh => format!("sh(wsh({multi}))"),
                ScriptType::P2sh => format!("sh({multi})"),
            }
        };
        Ok(DescriptorPair {
            external: inner(0),
            internal: inner(1),
        })
    }

    /// The configuration of a multisig descriptor (receive branch, or a
    /// `<0;1>` multipath one). Other descriptors are refused.
    pub fn from_descriptor(descriptor: &str, network: Network) -> Result<Self> {
        let pair = descriptors::import(descriptor, network)?;
        let parsed = Descriptor::<DescriptorPublicKey>::from_str(&pair.external)
            .map_err(|e| Error::Crypto(format!("bad descriptor: {e}")))?;
        let (script, sorted, threshold) = match &parsed {
            Descriptor::Wsh(w) => match w.as_inner() {
                WshInner::SortedMulti(m) => (ScriptType::P2wsh, true, m.k()),
                WshInner::Ms(ms) => match multi_of(&ms.to_string()) {
                    Some(k) => (ScriptType::P2wsh, false, k),
                    None => return Err(not_multisig()),
                },
            },
            Descriptor::Sh(sh) => match sh.as_inner() {
                ShInner::Wsh(w) => match w.as_inner() {
                    WshInner::SortedMulti(m) => (ScriptType::P2shP2wsh, true, m.k()),
                    WshInner::Ms(ms) => match multi_of(&ms.to_string()) {
                        Some(k) => (ScriptType::P2shP2wsh, false, k),
                        None => return Err(not_multisig()),
                    },
                },
                ShInner::SortedMulti(m) => (ScriptType::P2sh, true, m.k()),
                ShInner::Ms(ms) => match multi_of(&ms.to_string()) {
                    Some(k) => (ScriptType::P2sh, false, k),
                    None => return Err(not_multisig()),
                },
                _ => return Err(not_multisig()),
            },
            _ => return Err(not_multisig()),
        };
        let mut cosigners = Vec::new();
        let mut bad = None;
        parsed.for_each_key(|k| {
            let s = k.to_string();
            match Cosigner::parse(&s, network) {
                Ok(c) => cosigners.push(c),
                Err(e) => bad = Some(e),
            }
            true
        });
        if let Some(e) = bad {
            return Err(e);
        }
        let config = MultisigConfig {
            name: None,
            policy: Policy::Multi {
                threshold: u8::try_from(threshold).map_err(|_| not_multisig())?,
                sorted,
            },
            script,
            cosigners,
        };
        config.validate(network)?;
        Ok(config)
    }
}

fn not_multisig() -> Error {
    Error::Unsupported("import this kind of wallet as a multisig – only M-of-N multisig is supported for now".into())
}

/// `k` of a plain `multi(k,…)` miniscript, else `None`.
fn multi_of(ms: &str) -> Option<usize> {
    let rest = ms.strip_prefix("multi(")?;
    rest.split(',').next()?.parse().ok()
}

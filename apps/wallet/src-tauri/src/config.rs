//! Non-secret wallet configuration, stored as `config.json` next to the vault.
//! It holds public descriptors and preferences only: never keys.

use std::path::{Path, PathBuf};

use rail_onchain::{descriptors, DescriptorPair};
use serde::{Deserialize, Serialize};
use sessions_core::modules::Layout;
use sessions_core::{AccountId, Error, Network, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AccountSpec {
    /// BIP84 single-sig from the app seed.
    Savings {
        id: AccountId,
        name: String,
        account_index: u32,
        /// Offer payjoin when receiving (BIP77). Off by default.
        #[serde(default)]
        payjoin_receive: bool,
    },
    /// Spark / Lightning, seeded from a BIP85 child.
    Spending { id: AccountId, name: String },
    Multisig {
        id: AccountId,
        name: String,
        threshold: u8,
        total: u8,
        descriptors: DescriptorPair,
        /// BIP48 account index of this device's key.
        local_account: u32,
    },
    Hardware {
        id: AccountId,
        name: String,
        device: String,
        fingerprint: String,
        descriptors: DescriptorPair,
    },
    WatchOnly {
        id: AccountId,
        name: String,
        descriptors: DescriptorPair,
    },
    Ecash {
        id: AccountId,
        name: String,
        mint_url: String,
    },
}

impl AccountSpec {
    pub fn id(&self) -> &AccountId {
        match self {
            AccountSpec::Savings { id, .. }
            | AccountSpec::Spending { id, .. }
            | AccountSpec::Multisig { id, .. }
            | AccountSpec::Hardware { id, .. }
            | AccountSpec::WatchOnly { id, .. }
            | AccountSpec::Ecash { id, .. } => id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            AccountSpec::Savings { name, .. }
            | AccountSpec::Spending { name, .. }
            | AccountSpec::Multisig { name, .. }
            | AccountSpec::Hardware { name, .. }
            | AccountSpec::WatchOnly { name, .. }
            | AccountSpec::Ecash { name, .. } => name,
        }
    }

    pub fn name_mut(&mut self) -> &mut String {
        match self {
            AccountSpec::Savings { name, .. }
            | AccountSpec::Spending { name, .. }
            | AccountSpec::Multisig { name, .. }
            | AccountSpec::Hardware { name, .. }
            | AccountSpec::WatchOnly { name, .. }
            | AccountSpec::Ecash { name, .. } => name,
        }
    }

    /// The on-chain wallet this account shares keys with: `"hot"` for every
    /// account from the app seed, `"hw:<fingerprint>"` per hardware wallet.
    pub fn group(&self) -> Option<String> {
        match self {
            AccountSpec::Savings { .. } => Some("hot".into()),
            AccountSpec::Hardware {
                fingerprint,
                descriptors: d,
                ..
            } => {
                let fp = if fingerprint.is_empty() {
                    descriptors::fingerprints(&d.external)
                        .ok()?
                        .into_iter()
                        .next()?
                } else {
                    fingerprint.clone()
                };
                Some(format!("hw:{}", fp.to_lowercase()))
            }
            _ => None,
        }
    }

    /// BIP44-style account number within its wallet.
    pub fn account_index(&self) -> Option<u32> {
        match self {
            AccountSpec::Savings { account_index, .. } => Some(*account_index),
            AccountSpec::Hardware { descriptors: d, .. } => descriptors::account_index(&d.external),
            _ => None,
        }
    }
}

/// Trimmed account name, 1 to 32 characters.
pub fn check_name(name: &str) -> Result<String> {
    let name = name.trim();
    if !(1..=32).contains(&name.chars().count()) {
        return Err(Error::Invalid("Use a name of 1 to 32 characters".into()));
    }
    Ok(name.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub version: u32,
    pub network: Network,
    /// Your own Esplora (Umbrel, Start9, mempool) for privacy.
    pub esplora_url: Option<String>,
    /// Your own Electrum server (`ssl://host:50002`, `tcp://host:50001`).
    /// Wins over `esplora_url`. With neither, mainnet uses the public
    /// Electrum servers (see `rail_onchain::ChainServer::choose`).
    #[serde(default)]
    pub electrum_url: Option<String>,
    /// Breez SDK key; falls back to the one baked in at build time.
    pub breez_api_key: Option<String>,
    pub accounts: Vec<AccountSpec>,
    pub layout: Layout,
    /// Next free BIP48 account index for new multisig vaults.
    pub next_multisig_account: u32,
    /// Monotonic counter for account ids.
    pub next_id: u32,
    /// How the wallet's seed is backed up (passkey, written-down phrase).
    #[serde(default = "BackupFlags::legacy")]
    pub backup: BackupFlags,
    /// Nostr relays that hold the encrypted backup.
    #[serde(default = "sessions_backup::default_relays")]
    pub backup_relays: Vec<String>,
    /// The UI's own preferences (theme, currencies, units…), opaque here.
    /// Kept in the config so they're in the backup.
    #[serde(default)]
    pub prefs: serde_json::Value,
    /// Nostr Wallet Connect: apps allowed to use Spending (see `nwc`).
    #[serde(default)]
    pub nwc: Vec<crate::nwc::NwcEntry>,
}

/// Where the wallet's seed came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeedOrigin {
    /// A random recovery phrase (new or restored).
    Phrase,
    /// Derived from a passkey's PRF output.
    Passkey,
}

/// How the seed is backed up. The wallet *state* is backed up to relays in
/// every case; these say who can bring the seed back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupFlags {
    /// A passkey restores the seed: a passkey wallet, or a phrase sealed in
    /// an envelope only the passkey opens.
    pub passkey: bool,
    /// The user has the phrase written down (confirmed), or restored from it.
    pub phrase: bool,
    pub seed_origin: SeedOrigin,
    /// The passkey's credential id (base64url), when one was made here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    /// Unix seconds of the last "Test my backup" that passed on this device.
    /// Local: left out of the online backup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tested_at: Option<u64>,
}

impl BackupFlags {
    /// Wallets made before passkeys: the phrase was shown at setup.
    pub fn legacy() -> Self {
        BackupFlags {
            passkey: false,
            phrase: true,
            seed_origin: SeedOrigin::Phrase,
            credential_id: None,
            last_tested_at: None,
        }
    }

    /// A new phrase wallet: backed up once the user confirms the phrase.
    pub fn new_phrase() -> Self {
        BackupFlags {
            phrase: false,
            ..BackupFlags::legacy()
        }
    }
}

impl Config {
    /// Where on-chain accounts get chain data (see `ChainServer::choose`).
    pub fn chain_server(&self) -> Result<rail_onchain::ChainServer> {
        rail_onchain::ChainServer::choose(
            self.network,
            self.electrum_url.as_deref(),
            self.esplora_url.as_deref(),
        )
    }

    pub fn new(network: Network) -> Self {
        Config {
            version: 1,
            network,
            esplora_url: None,
            electrum_url: None,
            breez_api_key: None,
            // Spending only. On-chain is for hardware wallets and vaults; a hot
            // on-chain wallet is something the user turns on themselves.
            accounts: vec![AccountSpec::Spending {
                id: "spending".into(),
                name: "Spending".into(),
            }],
            layout: Layout::default(),
            next_multisig_account: 0,
            next_id: 1,
            backup: BackupFlags::new_phrase(),
            backup_relays: sessions_backup::default_relays(),
            prefs: serde_json::Value::Null,
            nwc: Vec::new(),
        }
    }

    pub fn fresh_id(&mut self, prefix: &str) -> AccountId {
        let id = AccountId(format!("{prefix}-{}", self.next_id));
        self.next_id += 1;
        id
    }

    /// Refuse an account that's already in the wallet.
    pub fn check_new(&self, spec: &AccountSpec) -> Result<()> {
        let dup = || Err(Error::Invalid("That account is already added".into()));
        if self.accounts.iter().any(|a| a.id() == spec.id()) {
            return dup();
        }
        let watched = |a: &AccountSpec| match a {
            AccountSpec::Hardware { descriptors, .. }
            | AccountSpec::WatchOnly { descriptors, .. } => Some(descriptors.clone()),
            _ => None,
        };
        match spec {
            AccountSpec::Savings { account_index, .. } => {
                if *account_index >= 0x8000_0000 {
                    return Err(Error::Invalid("That account number is too large".into()));
                }
                if self.accounts.iter().any(|a| {
                    matches!(a, AccountSpec::Savings { account_index: i, .. } if i == account_index)
                }) {
                    return dup();
                }
            }
            AccountSpec::Hardware { .. } | AccountSpec::WatchOnly { .. } => {
                let new = watched(spec).expect("watched kind");
                if self
                    .accounts
                    .iter()
                    .filter_map(watched)
                    .any(|d| descriptors::same_account(&d, &new))
                {
                    return dup();
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Option<Config>> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<Config>(&bytes)
                .map(|c| Some(c.migrated()))
                .map_err(|e| Error::Storage(format!("config.json: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Storage(e.to_string())),
        }
    }

    /// Bring older configs up to date. Saved on the next write.
    fn migrated(mut self) -> Self {
        for a in &mut self.accounts {
            // The hot on-chain account was called "Savings" before 0.1.7.
            if let AccountSpec::Savings { name, .. } = a {
                if name == "Savings" {
                    *name = "On-Chain".into();
                }
            }
        }
        self
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(
            path,
            &serde_json::to_vec_pretty(self).map_err(|e| Error::Storage(e.to_string()))?,
        )
    }
}

/// Write to a temp file and rename, so a crash never leaves a half-written file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::Storage(e.to_string()))?;
    }
    let tmp: PathBuf = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| Error::Storage(e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| Error::Storage(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const XPUB: &str = "xpub6CatWdiZiodmUeTDp8LT5or8nmbKNcuyvz7WyksVFkKB4RHwCD3XyuvPEbvqAQY3rAPshWcMLoP2fMFMKHPJ4ZeZXYVUhLv1VMrjPC7PW6V";

    fn pair(origin: &str) -> DescriptorPair {
        descriptors::import(&format!("[{origin}]{XPUB}"), Network::Bitcoin).unwrap()
    }

    fn hardware(id: &str, fingerprint: &str, origin: &str) -> AccountSpec {
        AccountSpec::Hardware {
            id: id.into(),
            name: "Trezor".into(),
            device: "Trezor".into(),
            fingerprint: fingerprint.into(),
            descriptors: pair(origin),
        }
    }

    fn savings(id: &str, account_index: u32) -> AccountSpec {
        AccountSpec::Savings {
            id: id.into(),
            name: "On-Chain".into(),
            account_index,
            payjoin_receive: false,
        }
    }

    #[test]
    fn group_and_index_per_kind() {
        let s = savings("savings", 4);
        assert_eq!(
            (s.group(), s.account_index()),
            (Some("hot".into()), Some(4))
        );

        let h = hardware("hw-1", "73C5DA0A", "73c5da0a/84h/0h/3h");
        assert_eq!(
            (h.group(), h.account_index()),
            (Some("hw:73c5da0a".into()), Some(3))
        );
        // Fingerprint missing from the spec: read it from the descriptor.
        let h = hardware("hw-2", "", "73c5da0a/84'/0'/0'");
        assert_eq!(
            (h.group(), h.account_index()),
            (Some("hw:73c5da0a".into()), Some(0))
        );

        let w = AccountSpec::WatchOnly {
            id: "watch-1".into(),
            name: "Watch".into(),
            descriptors: pair("73c5da0a/84h/0h/0h"),
        };
        let m = AccountSpec::Multisig {
            id: "vault-1".into(),
            name: "Vault".into(),
            threshold: 2,
            total: 3,
            descriptors: pair("73c5da0a/84h/0h/0h"),
            local_account: 0,
        };
        let sp = AccountSpec::Spending {
            id: "spending".into(),
            name: "Spending".into(),
        };
        let e = AccountSpec::Ecash {
            id: "ecash-1".into(),
            name: "Ecash".into(),
            mint_url: "https://mint.example".into(),
        };
        for a in [w, m, sp, e] {
            assert_eq!((a.group(), a.account_index()), (None, None));
        }
    }

    #[test]
    fn duplicates_are_refused() {
        let mut c = Config::new(Network::Bitcoin);
        c.accounts.push(savings("savings", 0));
        c.accounts
            .push(hardware("hw-1", "73c5da0a", "73c5da0a/84h/0h/3h"));

        let msg = |r: Result<()>| r.unwrap_err().to_string();
        assert!(msg(c.check_new(&savings("savings-9", 0))).contains("already added"));
        assert!(c.check_new(&savings("savings-9", 1)).is_ok());
        assert!(c.check_new(&savings("savings-9", 0x8000_0000)).is_err());
        // Same account written with ' instead of h.
        assert!(
            msg(c.check_new(&hardware("hw-2", "73c5da0a", "73c5da0a/84'/0'/3'")))
                .contains("That account is already added")
        );
        // Another account of the same device is fine.
        assert!(c
            .check_new(&hardware("hw-2", "73c5da0a", "73c5da0a/84h/0h/4h"))
            .is_ok());
        // ...but not as watch-only either.
        let w = AccountSpec::WatchOnly {
            id: "watch-1".into(),
            name: "Watch".into(),
            descriptors: pair("73c5da0a/84h/0h/3h"),
        };
        assert!(c.check_new(&w).is_err());
    }

    #[test]
    fn older_configs_get_backup_defaults() {
        let mut v = serde_json::to_value(Config::new(Network::Bitcoin)).unwrap();
        let o = v.as_object_mut().unwrap();
        o.remove("backup");
        o.remove("backupRelays");
        o.remove("prefs");
        let c: Config = serde_json::from_value(v).unwrap();
        assert_eq!(c.backup, BackupFlags::legacy());
        assert!(c.backup_relays.len() >= 4);
        assert!(c.prefs.is_null());
        let json = serde_json::to_value(&c.backup).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "passkey": false, "phrase": true, "seedOrigin": "phrase" })
        );
    }

    #[test]
    fn names() {
        assert_eq!(check_name("  Rainy day ").unwrap(), "Rainy day");
        assert!(check_name("   ").is_err());
        assert!(check_name(&"x".repeat(33)).is_err());
        // Characters, not bytes.
        assert!(check_name(&"₿".repeat(32)).is_ok());
    }
}

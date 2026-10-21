//! Non-secret wallet configuration, stored as `config.json` next to the vault.
//! It holds public descriptors and preferences only: never keys.

use std::path::{Path, PathBuf};

use rail_onchain::DescriptorPair;
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub version: u32,
    pub network: Network,
    /// Your own Esplora (Umbrel, Start9, mempool) for privacy.
    pub esplora_url: Option<String>,
    /// Breez SDK key; falls back to the one baked in at build time.
    pub breez_api_key: Option<String>,
    pub accounts: Vec<AccountSpec>,
    pub layout: Layout,
    /// Next free BIP48 account index for new multisig vaults.
    pub next_multisig_account: u32,
    /// Monotonic counter for account ids.
    pub next_id: u32,
}

impl Config {
    pub fn new(network: Network) -> Self {
        Config {
            version: 1,
            network,
            esplora_url: None,
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
        }
    }

    pub fn fresh_id(&mut self, prefix: &str) -> AccountId {
        let id = AccountId(format!("{prefix}-{}", self.next_id));
        self.next_id += 1;
        id
    }

    pub fn load(path: &Path) -> Result<Option<Config>> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| Error::Storage(format!("config.json: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Storage(e.to_string())),
        }
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

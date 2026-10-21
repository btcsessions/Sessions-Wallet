use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rail_ecash::{EcashAccount, EcashConfig};
use rail_liquid::{BoltzConfig, BoltzSwaps};
use rail_onchain::{descriptors, OnchainAccount, OnchainConfig};
use rail_spark::{SparkAccount, SparkConfig};
use sessions_core::keystore::{child, KdfParams, Seed, Vault};
use sessions_core::{AccountId, Custody, Error, Network, Result, Wallet};
use tokio::sync::Mutex;

use crate::config::{write_atomic, AccountSpec, Config};

/// Mainnet. Change only for development builds.
pub const NETWORK: Network = Network::Bitcoin;

/// Breez issues free API keys; set BREEZ_API_KEY when building releases.
const BUILT_IN_BREEZ_KEY: Option<&str> = option_env!("BREEZ_API_KEY");

pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn vault(&self) -> PathBuf {
        self.root.join("vault.json")
    }
    pub fn config(&self) -> PathBuf {
        self.root.join("config.json")
    }
    fn onchain(&self, id: &AccountId) -> PathBuf {
        self.root.join("onchain").join(format!("{id}.db"))
    }
    fn spark(&self, id: &AccountId) -> PathBuf {
        self.root.join("spark").join(&id.0)
    }
    fn ecash(&self, id: &AccountId) -> PathBuf {
        self.root.join("ecash").join(format!("{id}.redb"))
    }
    fn swaps(&self) -> PathBuf {
        self.root.join("swaps")
    }
}

pub struct Unlocked {
    pub seed: Seed,
    pub wallet: Arc<Wallet>,
    pub config: Config,
    pub onchain: HashMap<AccountId, Arc<OnchainAccount>>,
    /// Accounts that failed to open (e.g. offline at startup), with the reason.
    pub unavailable: Vec<(AccountId, String)>,
}

pub struct AppState {
    pub paths: Paths,
    pub inner: Mutex<Option<Unlocked>>,
    failures: Mutex<(u32, Option<Instant>)>,
}

impl AppState {
    pub fn new(root: PathBuf) -> Self {
        AppState { paths: Paths { root }, inner: Mutex::new(None), failures: Mutex::new((0, None)) }
    }

    pub fn has_vault(&self) -> bool {
        self.paths.vault().exists()
    }

    fn read_vault(&self) -> Result<Vault> {
        let bytes = std::fs::read(self.paths.vault()).map_err(|e| Error::Storage(e.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|e| Error::Storage(e.to_string()))
    }

    /// Open the vault, slowing down repeated wrong guesses.
    pub async fn open_vault(&self, passcode: &str) -> Result<Seed> {
        {
            let f = self.failures.lock().await;
            if let (_, Some(until)) = *f {
                if Instant::now() < until {
                    let wait_secs = until.saturating_duration_since(Instant::now()).as_secs() + 1;
                    return Err(Error::TooManyAttempts { wait_secs });
                }
            }
        }
        let vault = self.read_vault()?;
        // Argon2 is CPU-heavy; keep it off the async workers.
        let pass = zeroize::Zeroizing::new(passcode.to_string());
        let res = tokio::task::spawn_blocking(move || vault.open(&pass)).await.map_err(|e| Error::Crypto(e.to_string()))?;
        let mut f = self.failures.lock().await;
        match res {
            Ok(seed) => {
                *f = (0, None);
                Ok(seed)
            }
            Err(e) => {
                f.0 += 1;
                if f.0 >= 3 {
                    // 2 s, 4 s, 8 s … capped at 15 minutes.
                    let wait = Duration::from_secs((1u64 << (f.0 - 2).min(10)).min(900));
                    f.1 = Some(Instant::now() + wait);
                }
                Err(e)
            }
        }
    }

    pub async fn create(&self, seed: Seed, passcode: &str) -> Result<()> {
        if self.has_vault() {
            return Err(Error::Invalid("A wallet already exists on this device".into()));
        }
        let pass = zeroize::Zeroizing::new(passcode.to_string());
        let phrase = seed.phrase();
        let vault = tokio::task::spawn_blocking(move || Vault::seal(&Seed::from_phrase(&phrase)?, &pass, KdfParams::default()))
            .await
            .map_err(|e| Error::Crypto(e.to_string()))??;
        let config = Config::load(&self.paths.config())?.unwrap_or_else(|| Config::new(NETWORK));
        config.save(&self.paths.config())?;
        write_atomic(&self.paths.vault(), &serde_json::to_vec(&vault).map_err(|e| Error::Storage(e.to_string()))?)?;
        self.unlock_with(seed).await
    }

    pub async fn unlock_with(&self, seed: Seed) -> Result<()> {
        let config = Config::load(&self.paths.config())?.unwrap_or_else(|| Config::new(NETWORK));
        let wallet = Arc::new(Wallet::new());
        let mut onchain = HashMap::new();
        let mut unavailable = Vec::new();

        let master = seed.master_xpriv(config.network)?;
        for spec in &config.accounts {
            match self.open_account(spec, &seed, &master, &config).await {
                Ok(Opened::Onchain(a)) => {
                    onchain.insert(spec.id().clone(), a.clone());
                    wallet.add_account(a).await;
                }
                Ok(Opened::Other(r)) => wallet.add_account(r).await,
                Err(e) => {
                    tracing::warn!(account = %spec.id(), "couldn't open account: {e}");
                    unavailable.push((spec.id().clone(), e.to_string()));
                }
            }
        }

        match BoltzSwaps::new(BoltzConfig {
            network: config.network,
            data_dir: self.paths.swaps(),
            mnemonic: seed.bip85_child(child::SWAPS)?.phrase().to_string(),
            liquid_esplora: None,
            bitcoin_esplora: config.esplora_url.clone(),
        }) {
            Ok(b) => wallet.add_swap_provider(Arc::new(b)).await,
            Err(e) => tracing::warn!("Liquid swaps unavailable: {e}"),
        }

        *self.inner.lock().await = Some(Unlocked { seed, wallet, config, onchain, unavailable });
        Ok(())
    }

    async fn open_account(&self, spec: &AccountSpec, seed: &Seed, master: &rail_onchain::Xpriv, cfg: &Config) -> Result<Opened> {
        let onchain = |id: &AccountId, name: &str, descriptors, custody, signer| -> Result<Opened> {
            Ok(Opened::Onchain(Arc::new(OnchainAccount::open(OnchainConfig {
                id: id.clone(),
                name: name.to_string(),
                network: cfg.network,
                db_path: self.paths.onchain(id),
                esplora_url: cfg.esplora_url.clone(),
                descriptors,
                custody,
                signer,
            })?)))
        };
        match spec {
            AccountSpec::Savings { id, name, account_index } => onchain(
                id,
                name,
                descriptors::hot_single_sig(master, cfg.network, *account_index)?,
                Custody::Hot,
                Some(*master),
            ),
            AccountSpec::Multisig { id, name, threshold, total, descriptors, .. } => onchain(
                id,
                name,
                descriptors.clone(),
                Custody::Multisig { threshold: *threshold, total: *total, local_signers: 1 },
                Some(*master),
            ),
            AccountSpec::Hardware { id, name, device, fingerprint, descriptors } => onchain(
                id,
                name,
                descriptors.clone(),
                Custody::Hardware { device: device.clone(), fingerprint: fingerprint.clone() },
                None,
            ),
            AccountSpec::WatchOnly { id, name, descriptors } => onchain(id, name, descriptors.clone(), Custody::WatchOnly, None),
            AccountSpec::Spending { id, name } => {
                let api_key = cfg
                    .breez_api_key
                    .clone()
                    .or(BUILT_IN_BREEZ_KEY.map(str::to_string))
                    .ok_or_else(|| Error::Invalid("It needs a Breez API key. Add one in Settings".into()))?;
                let account = SparkAccount::connect(SparkConfig {
                    id: id.clone(),
                    name: name.clone(),
                    mnemonic: seed.bip85_child(child::SPARK)?.phrase().to_string(),
                    network: cfg.network,
                    storage_dir: self.paths.spark(id),
                    api_key,
                })
                .await?;
                Ok(Opened::Other(Arc::new(account)))
            }
            AccountSpec::Ecash { id, name, mint_url } => {
                let account = EcashAccount::open(EcashConfig {
                    id: id.clone(),
                    name: name.clone(),
                    mint_url: mint_url.clone(),
                    mnemonic: seed.bip85_child(child::ECASH)?.phrase().to_string(),
                    db_path: self.paths.ecash(id),
                })
                .await?;
                Ok(Opened::Other(Arc::new(account)))
            }
        }
    }

    /// Add an account to the config and open it in the running wallet.
    pub async fn add_account(&self, spec: AccountSpec) -> Result<AccountId> {
        let mut guard = self.inner.lock().await;
        let u = guard.as_mut().ok_or(Error::Locked)?;
        let master = u.seed.master_xpriv(u.config.network)?;
        let opened = self.open_account(&spec, &u.seed, &master, &u.config).await?;
        let id = spec.id().clone();
        match opened {
            Opened::Onchain(a) => {
                u.onchain.insert(id.clone(), a.clone());
                u.wallet.add_account(a).await;
            }
            Opened::Other(r) => u.wallet.add_account(r).await,
        }
        u.config.accounts.push(spec);
        u.config.save(&self.paths.config())?;
        Ok(id)
    }

    pub async fn lock(&self) {
        // Dropping Unlocked drops the seed and all rails.
        *self.inner.lock().await = None;
    }
}

enum Opened {
    Onchain(Arc<OnchainAccount>),
    Other(Arc<dyn sessions_core::Rail>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessions_core::{Method, ReceiveRequest};

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[tokio::test]
    async fn create_lock_unlock_and_rate_limit() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(dir.path().to_path_buf());
        assert!(!state.has_vault());

        state.create(Seed::from_phrase(ABANDON).unwrap(), "123456").await.unwrap();
        assert!(state.has_vault());
        // A second wallet can't overwrite the first.
        assert!(state.create(Seed::generate(12).unwrap(), "654321").await.is_err());

        {
            let guard = state.inner.lock().await;
            let u = guard.as_ref().unwrap();
            // Savings opens offline with the BIP84 vector address.
            let savings = u.wallet.account(&"savings".into()).await.unwrap();
            let r = savings
                .receive(ReceiveRequest { method: Method::BitcoinAddress, amount_sat: None, description: None })
                .await
                .unwrap();
            assert_eq!(r.request, "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu");
            // Without a Breez key, Spending is reported, not fatal (unless one was baked in).
            if BUILT_IN_BREEZ_KEY.is_none() {
                assert!(u.unavailable.iter().any(|(id, _)| id.0 == "spending"));
            }
            // Config on disk holds no secrets.
            let cfg = std::fs::read_to_string(state.paths.config()).unwrap();
            assert!(!cfg.contains("abandon") && !cfg.contains("xprv"));
        }

        state.lock().await;
        assert!(state.inner.lock().await.is_none());

        // Three wrong guesses trigger a lockout, even for the right passcode.
        for _ in 0..3 {
            assert!(matches!(state.open_vault("000000").await, Err(Error::WrongPasscode)));
        }
        assert!(matches!(state.open_vault("123456").await, Err(Error::TooManyAttempts { .. })));

        // After the lockout the right passcode works and resets the counter.
        tokio::time::sleep(Duration::from_millis(2100)).await;
        let seed = state.open_vault("123456").await.unwrap();
        assert_eq!(seed.phrase().as_str(), ABANDON);
        state.unlock_with(seed).await.unwrap();
        assert!(state.inner.lock().await.is_some());
    }
}

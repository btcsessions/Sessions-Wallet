use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use serde::{Deserialize, Serialize};

use rail_ecash::{EcashAccount, EcashConfig};
use rail_liquid::{BoltzConfig, BoltzSwaps};
use rail_onchain::{descriptors, OnchainAccount, OnchainConfig};
use rail_spark::{SparkAccount, SparkChain, SparkConfig, SparkForwarder};
use sessions_core::keystore::{child, KdfParams, Seed, Vault};
use sessions_core::{AccountId, Custody, Error, Network, Result, Wallet};
use tokio::sync::Mutex;

use crate::backup::Backup;
use crate::config::{check_name, write_atomic, AccountSpec, BackupFlags, Config};
use crate::monitor::{Activity, Monitor, Watcher};

/// Called with an account's id when it reports a change (e.g. a payment
/// arrived, history finished loading).
pub type Notify = Arc<dyn Fn(&AccountId) + Send + Sync>;

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
    /// Wrong-passcode count and lockout, kept across restarts.
    pub fn lockout(&self) -> PathBuf {
        self.root.join("lockout.json")
    }
    pub fn config(&self) -> PathBuf {
        self.root.join("config.json")
    }
    fn onchain(&self, id: &AccountId) -> PathBuf {
        self.root.join("onchain").join(format!("{id}.db"))
    }
    /// An on-chain account's labels and frozen coins (BIP329 JSON), as
    /// `rail_onchain` keeps them next to its database.
    pub fn labels(&self, id: &AccountId) -> PathBuf {
        self.onchain(id).with_extension("labels.json")
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
    /// Journal of Lightning → on-chain forwards through a Spending account.
    fn forwards(&self, id: &AccountId) -> PathBuf {
        self.root.join("forwards").join(format!("{id}.json"))
    }
}

pub struct Unlocked {
    pub seed: Seed,
    pub wallet: Arc<Wallet>,
    pub config: Config,
    pub onchain: HashMap<AccountId, Arc<OnchainAccount>>,
    pub spark: HashMap<AccountId, Arc<SparkAccount>>,
    /// The Spending account that forwards Lightning payments to on-chain
    /// accounts, once one is open.
    pub forwarder: Option<AccountId>,
    /// Accounts that failed to open (e.g. offline at startup), with the reason.
    pub unavailable: Vec<(AccountId, String)>,
    /// Push updates for on-chain accounts (the running app only).
    pub monitor: Option<Monitor>,
    /// Nostr Wallet Connect, while any app is connected.
    pub nwc: Option<crate::nwc::Handle>,
}

impl Unlocked {
    /// Tell the monitor which on-chain accounts exist (after adding or
    /// removing one).
    pub fn refresh_monitor(&self) {
        if let Some(m) = &self.monitor {
            m.set_accounts(self.onchain.values().cloned().collect());
        }
    }
}

/// A platform key store that wraps and unwraps small secrets with a key
/// that never leaves the device (Android Keystore).
#[async_trait::async_trait]
pub trait DeviceKey: Send + Sync {
    async fn wrap(&self, secret: &[u8]) -> Result<Vec<u8>>;
    async fn unwrap(&self, wrapped: &[u8]) -> Result<Vec<u8>>;
}

/// Wrong passcodes so far and when the next try is allowed (Unix seconds).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Lockout {
    failures: u32,
    until: u64,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn device_key_gone() -> Error {
    Error::Invalid(
        "This phone's security key for the wallet is gone (the phone was reset or its lock screen changed). \
         Restore the wallet with your recovery phrase or passkey"
            .into(),
    )
}

pub struct AppState {
    pub paths: Paths,
    /// Called when an account reports a change (e.g. a payment arrived).
    /// Set in the running app; tests leave it out and stay offline (no
    /// background monitor).
    notify: Option<Notify>,
    pub inner: Mutex<Option<Unlocked>>,
    /// Held while a passcode is checked (one guess at a time).
    failures: Mutex<()>,
    device_key: std::sync::OnceLock<Arc<dyn DeviceKey>>,
    /// The encrypted state backup on relays.
    pub backup: Arc<Backup>,
    /// Payment alerts (new payments → UI event + notification).
    pub alerts: Arc<crate::alerts::Alerts>,
    /// The app's screen is locked while the wallet keeps running (so it can
    /// keep watching for payments): unlocking only checks the passcode.
    pub screen_locked: std::sync::atomic::AtomicBool,
    /// Saves a Nostr Wallet Connect connection's budget use (set by the app).
    pub nwc_saver: std::sync::OnceLock<Arc<dyn sessions_nwc::ConnectionStore>>,
}

impl AppState {
    /// The unlocked wallet's network.
    /// The wallet's network. Doesn't wait more than a moment for the wallet:
    /// a hardware wallet screen shouldn't hang behind a slow sync or open.
    pub async fn network(&self) -> Result<Network> {
        let g = match tokio::time::timeout(std::time::Duration::from_secs(3), self.inner.lock()).await {
            Ok(g) => g,
            Err(_) => {
                tracing::warn!("network: wallet busy for 3s, using the default network");
                return Err(Error::Locked);
            }
        };
        Ok(g.as_ref().ok_or(Error::Locked)?.config.network)
    }

    pub fn new(root: PathBuf, notify: Option<Notify>) -> Self {
        AppState {
            backup: Arc::new(Backup::new(&root, notify.is_some())),
            paths: Paths { root },
            notify,
            inner: Mutex::new(None),
            failures: Mutex::new(()),
            device_key: std::sync::OnceLock::new(),
            alerts: Arc::default(),
            screen_locked: std::sync::atomic::AtomicBool::new(false),
            nwc_saver: std::sync::OnceLock::new(),
        }
    }

    /// Refuse while the screen is locked (the wallet runs, the user hasn't
    /// entered the passcode).
    pub fn check_screen(&self) -> Result<()> {
        if self.screen_locked.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::Locked);
        }
        Ok(())
    }

    /// Where the NWC service reports budget use (a no-op in tests).
    pub fn nwc_store(&self) -> Arc<dyn sessions_nwc::ConnectionStore> {
        self.nwc_saver
            .get()
            .cloned()
            .unwrap_or_else(|| Arc::new(|_: &sessions_nwc::Connection| {}))
    }

    pub fn has_vault(&self) -> bool {
        self.paths.vault().exists()
    }

    /// The platform key store that binds the vault to this device (Android
    /// Keystore). Set once at startup; absent on desktop for now.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn set_device_key(&self, key: Arc<dyn DeviceKey>) {
        let _ = self.device_key.set(key);
    }

    /// A new device secret for sealing, wrapped by the key store (`None`
    /// where there's no key store).
    async fn new_device_secret(&self) -> Result<Option<sessions_core::keystore::DeviceSecret>> {
        let Some(k) = self.device_key.get().cloned() else { return Ok(None) };
        let secret = sessions_core::keystore::new_device_secret();
        let wrapped = k.wrap(&secret[..]).await?;
        Ok(Some(sessions_core::keystore::DeviceSecret { secret, wrapped }))
    }

    /// The device secret a version 2 vault needs.
    async fn device_secret_for(&self, vault: &Vault) -> Result<Option<zeroize::Zeroizing<[u8; 32]>>> {
        let Some(wrapped) = &vault.device else { return Ok(None) };
        let k = self.device_key.get().cloned().ok_or_else(device_key_gone)?;
        let raw = zeroize::Zeroizing::new(k.unwrap(wrapped).await.map_err(|_| device_key_gone())?);
        let secret: [u8; 32] = raw.as_slice().try_into().map_err(|_| device_key_gone())?;
        Ok(Some(zeroize::Zeroizing::new(secret)))
    }

    fn read_lockout(&self) -> Lockout {
        std::fs::read(self.paths.lockout())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn write_lockout(&self, l: &Lockout) {
        if let Ok(b) = serde_json::to_vec(l) {
            let _ = write_atomic(&self.paths.lockout(), &b);
        }
    }

    fn read_vault(&self) -> Result<Vault> {
        let bytes = std::fs::read(self.paths.vault()).map_err(|e| Error::Storage(e.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|e| Error::Storage(e.to_string()))
    }

    /// Open the vault, slowing down repeated wrong guesses.
    pub async fn open_vault(&self, passcode: &str) -> Result<Seed> {
        // One guess at a time, and the count survives closing the app.
        let _one = self.failures.lock().await;
        let mut lock = self.read_lockout();
        let now = unix_now();
        if lock.until > now {
            return Err(Error::TooManyAttempts { wait_secs: lock.until - now + 1 });
        }
        let vault = self.read_vault()?;
        let device = self.device_secret_for(&vault).await?;
        // Argon2 is CPU-heavy; keep it off the async workers.
        let pass = zeroize::Zeroizing::new(passcode.to_string());
        let v = vault.clone();
        let res = tokio::task::spawn_blocking(move || v.open_with(&pass, device.as_deref()))
            .await
            .map_err(|e| Error::Crypto(e.to_string()))?;
        match res {
            Ok(seed) => {
                if lock.failures > 0 {
                    self.write_lockout(&Lockout::default());
                }
                // Older vaults: bind to this device's key store now.
                if vault.device.is_none() {
                    if let Err(e) = self.reseal(&seed, passcode).await {
                        tracing::warn!("vault: couldn't bind to the device key store: {e}");
                    }
                }
                Ok(seed)
            }
            Err(e) => {
                lock.failures += 1;
                if lock.failures >= 3 {
                    // 2 s, 4 s, 8 s … capped at 15 minutes.
                    lock.until = unix_now() + (1u64 << (lock.failures - 2).min(10)).min(900);
                }
                self.write_lockout(&lock);
                Err(e)
            }
        }
    }

    /// Seal the vault again (same passcode), bound to the device key store.
    async fn reseal(&self, seed: &Seed, passcode: &str) -> Result<()> {
        let Some(device) = self.new_device_secret().await? else { return Ok(()) };
        let pass = zeroize::Zeroizing::new(passcode.to_string());
        let phrase = seed.phrase();
        let vault = tokio::task::spawn_blocking(move || {
            Vault::seal_with(&Seed::from_phrase(&phrase)?, &pass, KdfParams::default(), Some(&device))
        })
        .await
        .map_err(|e| Error::Crypto(e.to_string()))??;
        write_atomic(
            &self.paths.vault(),
            &serde_json::to_vec(&vault).map_err(|e| Error::Storage(e.to_string()))?,
        )
    }

    /// A new phrase wallet (not yet confirmed written down).
    pub async fn create(&self, seed: Seed, passcode: &str) -> Result<()> {
        self.create_with(seed, passcode, BackupFlags::new_phrase(), false)
            .await
    }

    /// Seal `seed` under the passcode and open the wallet. `flags` say how
    /// it's backed up; `restored` holds state uploads until its old backup
    /// is found or ruled out (see `backup::restore_state`).
    pub async fn create_with(
        &self,
        seed: Seed,
        passcode: &str,
        flags: BackupFlags,
        restored: bool,
    ) -> Result<()> {
        if self.has_vault() {
            return Err(Error::Invalid(
                "A wallet already exists on this device".into(),
            ));
        }
        let pass = zeroize::Zeroizing::new(passcode.to_string());
        let phrase = seed.phrase();
        let device = self.new_device_secret().await?;
        let vault = tokio::task::spawn_blocking(move || {
            Vault::seal_with(&Seed::from_phrase(&phrase)?, &pass, KdfParams::default(), device.as_ref())
        })
        .await
        .map_err(|e| Error::Crypto(e.to_string()))??;
        let mut config =
            Config::load(&self.paths.config())?.unwrap_or_else(|| Config::new(NETWORK));
        config.backup = flags;
        config.save(&self.paths.config())?;
        self.backup.reset(restored);
        let _ = std::fs::remove_file(self.paths.lockout());
        write_atomic(
            &self.paths.vault(),
            &serde_json::to_vec(&vault).map_err(|e| Error::Storage(e.to_string()))?,
        )?;
        self.unlock_with(seed).await
    }

    pub async fn unlock_with(&self, seed: Seed) -> Result<()> {
        let config = Config::load(&self.paths.config())?.unwrap_or_else(|| Config::new(NETWORK));
        self.alerts.reset();
        self.alerts
            .apply(crate::alerts::Settings::from_prefs(&config.prefs), false);
        let wallet = Arc::new(Wallet::new());
        let mut onchain = HashMap::new();
        let mut spark = HashMap::new();
        let mut forwarder = None;
        let mut unavailable = Vec::new();

        let master = seed.master_xpriv(config.network)?;
        let backup_keys = sessions_backup::keys::StateKeys::from_master(&master)?;
        for spec in &config.accounts {
            match self.open_account(spec, &seed, &master, &config).await {
                Ok(Opened::Onchain(a)) => {
                    onchain.insert(spec.id().clone(), a.clone());
                    wallet.add_account(a).await;
                }
                Ok(Opened::Other(r)) => wallet.add_account(r).await,
                Ok(Opened::Spark(a)) => {
                    spark.insert(spec.id().clone(), a.clone());
                    if forwarder.is_none() {
                        forwarder = self.add_forwarder(&wallet, spec.id(), &a).await;
                    }
                    wallet.add_account(a).await;
                }
                Err(e) => {
                    tracing::warn!(account = %spec.id(), "couldn't open account: {e}");
                    unavailable.push((spec.id().clone(), e.to_string()));
                }
            }
        }

        // Registered after the Spending forwarder: Boltz now only bridges
        // Liquid (its Lightning → on-chain bridge is withdrawn while paused).
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

        let monitor = match (&self.notify, config.chain_server()) {
            (Some(_), Ok(server)) => {
                Some(Monitor::start(&server, onchain.values().cloned().collect()))
            }
            (Some(_), Err(e)) => {
                tracing::warn!("no push updates: {e}");
                None
            }
            (None, _) => None,
        };
        if let Some(m) = &monitor {
            for (id, a) in &spark {
                let name = config
                    .accounts
                    .iter()
                    .find(|s| s.id() == id)
                    .map(|s| s.name().to_string())
                    .unwrap_or_default();
                watch_deposits(m.watcher(), id.clone(), name, a, self.alerts.clone());
            }
        }
        let backup_config = config.clone();
        *self.inner.lock().await = Some(Unlocked {
            seed,
            wallet,
            config,
            onchain,
            spark,
            forwarder,
            unavailable,
            monitor,
            nwc: None,
        });
        if self.notify.is_some() {
            if let Some(u) = self.inner.lock().await.as_mut() {
                crate::nwc::sync_service(u, self.nwc_store());
            }
        }
        // `screen_locked` stays as it is: only the passcode clears it (a
        // restore applied in the background must not open the screen).
        self.backup.unlocked(backup_keys, &backup_config);
        self.alerts.apply(
            crate::alerts::Settings::from_prefs(&backup_config.prefs),
            true,
        );
        Ok(())
    }

    /// Save the config, and back it up.
    pub fn save_config(&self, config: &Config) -> Result<()> {
        config.save(&self.paths.config())?;
        self.backup.config_saved(config);
        Ok(())
    }

    /// Let on-chain accounts receive over Lightning through this Spending
    /// account. Returns the account id when registered.
    async fn add_forwarder(
        &self,
        wallet: &Wallet,
        id: &AccountId,
        account: &Arc<SparkAccount>,
    ) -> Option<AccountId> {
        match SparkForwarder::new(account.clone(), self.paths.forwards(id)) {
            Ok(f) => {
                wallet.add_swap_provider(Arc::new(f)).await;
                Some(id.clone())
            }
            Err(e) => {
                tracing::warn!(account = %id, "Lightning → on-chain forwarding unavailable: {e}");
                None
            }
        }
    }

    async fn open_account(
        &self,
        spec: &AccountSpec,
        seed: &Seed,
        master: &rail_onchain::Xpriv,
        cfg: &Config,
    ) -> Result<Opened> {
        let (group, account_index) = (spec.group(), spec.account_index());
        let onchain = |id: &AccountId,
                       name: &str,
                       descriptors,
                       custody,
                       signer,
                       payjoin_receive: bool|
         -> Result<Opened> {
            let account = OnchainAccount::open(OnchainConfig {
                id: id.clone(),
                name: name.to_string(),
                network: cfg.network,
                db_path: self.paths.onchain(id),
                esplora_url: cfg.esplora_url.clone(),
                electrum_url: cfg.electrum_url.clone(),
                descriptors,
                custody,
                signer,
                payjoin_receive,
                payjoin_dir: Some(self.paths.root.join("payjoin").join(&id.0)),
                group: group.clone(),
                account_index,
            })?;
            if let Some(notify) = self.notify.clone() {
                account.on_change(notify);
            }
            Ok(Opened::Onchain(Arc::new(account)))
        };
        match spec {
            AccountSpec::Savings {
                id,
                name,
                account_index,
                payjoin_receive,
            } => onchain(
                id,
                name,
                descriptors::hot_single_sig(master, cfg.network, *account_index)?,
                Custody::Hot,
                Some(*master),
                *payjoin_receive,
            ),
            AccountSpec::Multisig {
                id,
                name,
                threshold,
                total,
                descriptors,
                local_account,
                ..
            } => onchain(
                id,
                name,
                descriptors.clone(),
                Custody::Multisig {
                    threshold: *threshold,
                    total: *total,
                    local_signers: u8::from(local_account.is_some()),
                },
                local_account.map(|_| *master),
                false,
            ),
            AccountSpec::Hardware {
                id,
                name,
                device,
                fingerprint,
                descriptors,
            } => onchain(
                id,
                name,
                descriptors.clone(),
                Custody::Hardware {
                    device: device.clone(),
                    fingerprint: fingerprint.clone(),
                },
                None,
                false,
            ),
            AccountSpec::WatchOnly {
                id,
                name,
                descriptors,
            } => onchain(
                id,
                name,
                descriptors.clone(),
                Custody::WatchOnly,
                None,
                false,
            ),
            AccountSpec::Spending { id, name } => {
                let api_key = cfg
                    .breez_api_key
                    .clone()
                    .or(BUILT_IN_BREEZ_KEY.map(str::to_string))
                    .ok_or_else(|| {
                        Error::Invalid("It needs a Breez API key. Add one in Settings".into())
                    })?;
                let account = SparkAccount::connect(SparkConfig {
                    id: id.clone(),
                    name: name.clone(),
                    mnemonic: seed.bip85_child(child::SPARK)?.phrase().to_string(),
                    network: cfg.network,
                    storage_dir: self.paths.spark(id),
                    api_key,
                    chain: spark_chain(cfg),
                })
                .await?;
                if let Some(notify) = self.notify.clone() {
                    let id = id.clone();
                    account.on_change(move || notify(&id)).await;
                }
                Ok(Opened::Spark(Arc::new(account)))
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
        u.config.check_new(&spec)?;
        let master = u.seed.master_xpriv(u.config.network)?;
        let opened = self
            .open_account(&spec, &u.seed, &master, &u.config)
            .await?;
        let id = spec.id().clone();
        match opened {
            Opened::Onchain(a) => {
                u.onchain.insert(id.clone(), a.clone());
                u.wallet.add_account(a).await;
                u.refresh_monitor();
            }
            Opened::Other(r) => u.wallet.add_account(r).await,
            Opened::Spark(a) => {
                u.spark.insert(id.clone(), a.clone());
                if u.forwarder.is_none() {
                    u.forwarder = self.add_forwarder(&u.wallet, &id, &a).await;
                }
                if let Some(m) = &u.monitor {
                    watch_deposits(
                        m.watcher(),
                        id.clone(),
                        spec.name().to_string(),
                        &a,
                        self.alerts.clone(),
                    );
                }
                u.wallet.add_account(a).await;
            }
        }
        u.config.accounts.push(spec);
        self.save_config(&u.config)?;
        Ok(id)
    }

    /// Rename an account, in the config and in the running wallet.
    pub async fn rename_account(&self, id: &AccountId, name: &str) -> Result<()> {
        let name = check_name(name)?;
        let mut guard = self.inner.lock().await;
        let u = guard.as_mut().ok_or(Error::Locked)?;
        let spec = u
            .config
            .accounts
            .iter_mut()
            .find(|a| a.id() == id)
            .ok_or_else(|| Error::AccountNotFound(id.0.clone()))?;
        // An account that failed to open only has its config entry.
        if let Ok(live) = u.wallet.account(id).await {
            live.set_name(&name)?;
        }
        *spec.name_mut() = name;
        self.save_config(&u.config)
    }

    /// Turn payjoin receiving on or off for a hot on-chain account.
    pub async fn set_payjoin_receive(&self, id: &AccountId, enabled: bool) -> Result<()> {
        let mut guard = self.inner.lock().await;
        let u = guard.as_mut().ok_or(Error::Locked)?;
        let spec = u
            .config
            .accounts
            .iter_mut()
            .find(|a| a.id() == id)
            .ok_or_else(|| Error::AccountNotFound(id.0.clone()))?;
        let AccountSpec::Savings {
            payjoin_receive, ..
        } = spec
        else {
            return Err(Error::Invalid(
                "Payjoin receiving is only for hot on-chain wallets".into(),
            ));
        };
        let account = u
            .onchain
            .get(id)
            .cloned()
            .ok_or_else(|| Error::AccountNotFound(id.0.clone()))?;
        *payjoin_receive = account.set_payjoin_receive(enabled);
        self.save_config(&u.config)
    }

    /// Hot on-chain accounts. Their payjoin sessions are driven even after
    /// receiving is switched off, so in-flight payjoins still finish.
    pub async fn payjoin_receivers(&self) -> Vec<Arc<OnchainAccount>> {
        let guard = self.inner.lock().await;
        let Some(u) = guard.as_ref() else {
            return vec![];
        };
        u.config
            .accounts
            .iter()
            .filter_map(|a| match a {
                AccountSpec::Savings { id, .. } => u.onchain.get(id).cloned(),
                _ => None,
            })
            .collect()
    }

    pub async fn lock(&self) {
        // A change still waiting goes out now; the keys go with the lock.
        crate::backup::flush_on_lock(self).await;
        self.backup.locked();
        // Dropping Unlocked drops the seed and all rails.
        *self.inner.lock().await = None;
        self.screen_locked
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.alerts.reset();
        let settings = self.alerts.settings();
        self.alerts.apply(settings, false);
    }
}

/// Where a Spending account's SDK reads the chain: the wallet's Electrum
/// servers (the default on mainnet, and a server of your own), or your own
/// Esplora. Otherwise (test networks) the SDK's built-in default.
fn spark_chain(cfg: &Config) -> SparkChain {
    match cfg.chain_server() {
        Ok(rail_onchain::ChainServer::Electrum(servers)) => {
            match rail_onchain::electrum_chain::ElectrumChain::for_servers(&servers, cfg.network) {
                Ok(c) => SparkChain::Electrum(c),
                Err(e) => {
                    tracing::warn!("Spending: no Electrum chain backend ({e}); using Breez's");
                    SparkChain::Default
                }
            }
        }
        Ok(rail_onchain::ChainServer::Esplora(url))
            if !url.eq_ignore_ascii_case(rail_onchain::default_esplora(cfg.network)) =>
        {
            SparkChain::Esplora {
                mempool: crate::monitor::ws_url(&url).is_some(),
                url,
            }
        }
        _ => SparkChain::Default,
    }
}

/// Watch a Spending account's bitcoin deposit address on the chain server:
/// a payment to it makes the SDK look for (and claim) the deposit at once,
/// and new blocks while one waits for confirmations claim it as soon as it
/// matures. The address may need the Spark operators, so it's found in the
/// background.
fn watch_deposits(
    watcher: Watcher,
    id: AccountId,
    name: String,
    account: &Arc<SparkAccount>,
    alerts: Arc<crate::alerts::Alerts>,
) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let weak = Arc::downgrade(account);
    rt.spawn(async move {
        let Some(account) = weak.upgrade() else {
            return;
        };
        let address = match account.deposit_address().await {
            Ok(a) => a,
            Err(e) => {
                tracing::info!(account = %id, "not watching the deposit address: {e}");
                return;
            }
        };
        // What's there already isn't news.
        // (Nothing known if that fails: then the next look announces all.)
        let outputs = account.deposit_outputs().await.unwrap_or_default();
        alerts.deposits(&id, &name, &outputs);
        drop(account);
        let owner = id.clone();
        let hook = Arc::new(move |activity: Activity| {
            if let Some(a) = weak.upgrade() {
                let (alerts, owner, name) = (alerts.clone(), owner.clone(), name.clone());
                tokio::spawn(async move {
                    if activity == Activity::Payment {
                        // Tell the user first: the SDK's sync can take a while.
                        if let Ok(outputs) = a.deposit_outputs().await {
                            alerts.deposits(&owner, &name, &outputs);
                        }
                    }
                    a.chain_activity(activity == Activity::Payment).await
                });
            }
        });
        watcher.watch_external(id, address, hook);
    });
}

enum Opened {
    Onchain(Arc<OnchainAccount>),
    Spark(Arc<SparkAccount>),
    Other(Arc<dyn sessions_core::Rail>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessions_core::{Method, ReceiveRequest};

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    /// Stands in for the Android Keystore: XOR with a key only it holds.
    struct FakeKeystore([u8; 32]);

    #[async_trait::async_trait]
    impl DeviceKey for FakeKeystore {
        async fn wrap(&self, secret: &[u8]) -> Result<Vec<u8>> {
            Ok(secret.iter().zip(self.0.iter().cycle()).map(|(a, b)| a ^ b).collect())
        }
        async fn unwrap(&self, wrapped: &[u8]) -> Result<Vec<u8>> {
            self.wrap(wrapped).await
        }
    }

    #[tokio::test]
    async fn vault_is_bound_to_the_device_and_lockout_survives_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let ks = Arc::new(FakeKeystore([0x5a; 32]));
        let a = AppState::new(dir.path().to_path_buf(), None);
        a.set_device_key(ks.clone());
        a.create(Seed::from_phrase(ABANDON).unwrap(), "123456").await.unwrap();
        let vault: Vault = serde_json::from_slice(&std::fs::read(a.paths.vault()).unwrap()).unwrap();
        assert_eq!(vault.version, 2);
        assert!(vault.device.is_some());
        // The file alone (no key store) doesn't open, even with the passcode.
        assert!(vault.open("123456").is_err());
        assert_eq!(a.open_vault("123456").await.unwrap().phrase().as_str(), ABANDON);

        // Wrong guesses count across a restart.
        for _ in 0..3 {
            assert!(a.open_vault("000000").await.is_err());
        }
        let b = AppState::new(dir.path().to_path_buf(), None);
        b.set_device_key(ks.clone());
        assert!(matches!(b.open_vault("123456").await, Err(Error::TooManyAttempts { .. })));

        // Without the key store (another device), a clear message.
        let c = AppState::new(dir.path().to_path_buf(), None);
        std::fs::remove_file(c.paths.lockout()).unwrap();
        let err = c.open_vault("123456").await.err().unwrap();
        assert!(err.to_string().contains("security key"), "{err}");
    }

    #[tokio::test]
    async fn old_vaults_are_bound_on_the_next_unlock() {
        let dir = tempfile::tempdir().unwrap();
        let a = AppState::new(dir.path().to_path_buf(), None);
        a.create(Seed::from_phrase(ABANDON).unwrap(), "123456").await.unwrap();
        let v1: Vault = serde_json::from_slice(&std::fs::read(a.paths.vault()).unwrap()).unwrap();
        assert_eq!(v1.version, 1);
        let b = AppState::new(dir.path().to_path_buf(), None);
        b.set_device_key(Arc::new(FakeKeystore([7; 32])));
        b.open_vault("123456").await.unwrap();
        let v2: Vault = serde_json::from_slice(&std::fs::read(b.paths.vault()).unwrap()).unwrap();
        assert_eq!(v2.version, 2);
        assert_eq!(b.open_vault("123456").await.unwrap().phrase().as_str(), ABANDON);
    }

    #[tokio::test]
    async fn create_lock_unlock_and_rate_limit() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(dir.path().to_path_buf(), None);
        assert!(!state.has_vault());

        state
            .create(Seed::from_phrase(ABANDON).unwrap(), "123456")
            .await
            .unwrap();
        assert!(state.has_vault());
        // A second wallet can't overwrite the first.
        assert!(state
            .create(Seed::generate(12).unwrap(), "654321")
            .await
            .is_err());

        {
            let guard = state.inner.lock().await;
            let u = guard.as_ref().unwrap();
            // No hot on-chain account by default.
            assert!(u.wallet.account(&"savings".into()).await.is_err());
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
            assert!(matches!(
                state.open_vault("000000").await,
                Err(Error::WrongPasscode)
            ));
        }
        assert!(matches!(
            state.open_vault("123456").await,
            Err(Error::TooManyAttempts { .. })
        ));

        // After the lockout the right passcode works and resets the counter.
        tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
        let seed = state.open_vault("123456").await.unwrap();
        assert_eq!(seed.phrase().as_str(), ABANDON);
        state.unlock_with(seed).await.unwrap();
        assert!(state.inner.lock().await.is_some());

        // Opting in to a hot on-chain account gives the BIP84 vector address, offline.
        let id = state
            .add_account(AccountSpec::Savings {
                id: "savings".into(),
                name: "Savings".into(),
                account_index: 0,
                payjoin_receive: false,
            })
            .await
            .unwrap();
        let savings = state
            .inner
            .lock()
            .await
            .as_ref()
            .unwrap()
            .wallet
            .account(&id)
            .await
            .unwrap();
        let r = savings
            .receive(ReceiveRequest {
                method: Method::BitcoinAddress,
                amount_sat: None,
                description: None,
            })
            .await
            .unwrap();
        assert_eq!(r.request, "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu");
    }

    #[tokio::test]
    async fn accounts_in_one_wallet_rename_and_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(dir.path().to_path_buf(), None);
        state
            .create(Seed::from_phrase(ABANDON).unwrap(), "123456")
            .await
            .unwrap();
        let savings = |id: &str, account_index| AccountSpec::Savings {
            id: id.into(),
            name: "On-Chain".into(),
            account_index,
            payjoin_receive: false,
        };
        let a = state.add_account(savings("savings", 0)).await.unwrap();
        let b = state.add_account(savings("savings-7", 3)).await.unwrap();
        // Same account number twice is refused.
        let e = state
            .add_account(savings("savings-8", 3))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("already added"));

        let info = |id: AccountId| {
            let state = &state;
            async move {
                let w = state.inner.lock().await.as_ref().unwrap().wallet.clone();
                w.account(&id).await.unwrap().info().await.unwrap()
            }
        };
        let (ia, ib) = (info(a.clone()).await, info(b.clone()).await);
        assert_eq!(
            (ia.group.as_deref(), ia.account_index),
            (Some("hot"), Some(0))
        );
        assert_eq!(
            (ib.group.as_deref(), ib.account_index),
            (Some("hot"), Some(3))
        );
        let json = serde_json::to_value(&ib).unwrap();
        assert_eq!(json["group"], "hot");
        assert_eq!(json["accountIndex"], 3);

        // Rename: live account and config on disk.
        state.rename_account(&b, "  Rainy day  ").await.unwrap();
        assert_eq!(info(b.clone()).await.name, "Rainy day");
        let cfg = Config::load(&state.paths.config()).unwrap().unwrap();
        assert!(cfg
            .accounts
            .iter()
            .any(|s| matches!(s, AccountSpec::Savings { id, name, .. } if *id == b && name == "Rainy day")));
        assert!(state.rename_account(&b, "   ").await.is_err());
        assert!(state.rename_account(&"nope".into(), "X").await.is_err());

        // Survives a lock/unlock.
        state.lock().await;
        state
            .unlock_with(Seed::from_phrase(ABANDON).unwrap())
            .await
            .unwrap();
        assert_eq!(info(b).await.name, "Rainy day");
    }
}

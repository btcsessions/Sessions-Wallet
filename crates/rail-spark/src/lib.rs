//! Spark / Lightning spending account for Sessions Wallet, built on the
//! Breez SDK - Spark (pinned to release 0.26.1).
//!
//! One [`SparkAccount`] wraps one SDK instance, keyed by a BIP85 child phrase
//! supplied by the caller. The SDK keeps its own SQLite database under
//! `storage_dir`; see the crate README for the layout.

mod forward;
mod map;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use breez_sdk_spark::{
    AssetFilter, BreezSdk, ConnectRequest, FeePolicy, GetInfoRequest, InputType,
    ListPaymentsRequest, ListUnclaimedDepositsRequest, LnurlPayRequest, OnchainConfirmationSpeed,
    PaymentStatus as SdkStatus, PaymentType, PrepareLnurlPayRequest, PrepareLnurlPayResponse,
    PrepareSendPaymentRequest, PrepareSendPaymentResponse, ReceivePaymentMethod,
    ReceivePaymentRequest, SdkError, Seed, SendPaymentMethod, SendPaymentOptions,
    SendPaymentRequest, SyncWalletRequest,
};
use sessions_core::{
    AccountId, AccountInfo, Balance, Custody, Error, Method, Network, Payment, Rail, RailKind,
    ReceiveRequest, ReceiveTarget, Result, Sats, SendQuote, SendRequest,
};

pub use forward::{ForwardRecord, ForwardStatus, ForwardStore, SparkForwarder};
pub use map::{PAYS, RECEIVES};

/// A claimed Lightning address.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LightningAddress {
    pub address: String,
    pub username: String,
    /// bech32 LNURL for wallets that don't support Lightning addresses.
    pub lnurl: String,
}

impl From<breez_sdk_spark::LightningAddressInfo> for LightningAddress {
    fn from(i: breez_sdk_spark::LightningAddressInfo) -> Self {
        LightningAddress {
            address: i.lightning_address,
            username: i.username,
            lnurl: i.lnurl.bech32,
        }
    }
}
use map::{QuoteHandle, RAIL};

/// Quotes older than this are dropped from memory (the SDK's own fee quotes
/// expire far sooner; this only bounds abandoned entries).
const QUOTE_TTL: Duration = Duration::from_secs(60 * 60);
/// How long a Lightning send waits for settlement before returning `Pending`.
const LIGHTNING_COMPLETION_TIMEOUT_SECS: u32 = 20;

pub struct SparkConfig {
    pub id: AccountId,
    pub name: String,
    /// A BIP85 child phrase supplied by the caller (`keystore::child::SPARK`).
    pub mnemonic: String,
    pub network: Network,
    pub storage_dir: PathBuf,
    /// Breez API key. Required on mainnet; free from Breez (see README).
    pub api_key: String,
}

#[derive(Clone)]
enum Prepared {
    Send {
        response: PrepareSendPaymentResponse,
        speed: Option<OnchainConfirmationSpeed>,
    },
    Lnurl {
        response: PrepareLnurlPayResponse,
    },
}

struct PendingQuote {
    prepared: Prepared,
    created: Instant,
}

pub struct SparkAccount {
    id: AccountId,
    name: std::sync::RwLock<String>,
    network: breez_sdk_spark::Network,
    sdk: BreezSdk,
    quotes: Mutex<HashMap<String, PendingQuote>>,
    /// A background sync is running; don't start another.
    syncing: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

fn rail_err(e: SdkError) -> Error {
    Error::rail(RAIL, e)
}

impl SparkAccount {
    pub async fn connect(config: SparkConfig) -> Result<Self> {
        let network = map::sdk_network(config.network)?;

        let phrase = config
            .mnemonic
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        bip39::Mnemonic::parse_normalized(&phrase)
            .map_err(|e| Error::InvalidMnemonic(e.to_string()))?;

        let api_key = config.api_key.trim().to_string();
        if api_key.is_empty() && network == breez_sdk_spark::Network::Mainnet {
            return Err(Error::rail(RAIL, "a Breez API key is required on mainnet"));
        }

        std::fs::create_dir_all(&config.storage_dir).map_err(|e| Error::Storage(e.to_string()))?;
        let storage_dir = config
            .storage_dir
            .to_str()
            .ok_or_else(|| Error::Storage("storage path is not valid UTF-8".into()))?
            .to_string();

        let mut sdk_config = breez_sdk_spark::default_config(network);
        sdk_config.api_key = (!api_key.is_empty()).then_some(api_key);
        // The SDK's default only auto-claims deposits at <= 1 sat/vB, which would
        // leave Savings -> Spending moves stuck whenever fees are higher. Claim at
        // the network's recommended rate (+1 sat/vB) so moves complete on their own.
        sdk_config.max_deposit_claim_fee = Some(breez_sdk_spark::MaxFee::NetworkRecommended {
            leeway_sat_per_vbyte: 1,
        });

        let sdk = breez_sdk_spark::connect(ConnectRequest {
            config: sdk_config,
            seed: Seed::Mnemonic {
                mnemonic: phrase,
                passphrase: None,
            },
            storage_dir,
        })
        .await
        .map_err(rail_err)?;

        tracing::info!(account = %config.id, ?network, "spark account connected");
        Ok(Self {
            id: config.id,
            name: std::sync::RwLock::new(config.name),
            network,
            sdk,
            quotes: Mutex::new(HashMap::new()),
            syncing: Default::default(),
        })
    }

    /// Stop the SDK's background tasks. Call before dropping the account.
    pub async fn disconnect(&self) -> Result<()> {
        self.sdk.disconnect().await.map_err(rail_err)
    }

    /// Direct access for features the `Rail` trait doesn't cover (Lightning
    /// address registration, manual deposit claims, refunds).
    pub fn sdk(&self) -> &BreezSdk {
        &self.sdk
    }

    /// The account's Lightning address (e.g. `ben@breez.tips`), if claimed.
    /// Recovered from the server on a new device.
    pub async fn lightning_address(&self) -> Result<Option<LightningAddress>> {
        Ok(self
            .sdk
            .get_lightning_address()
            .await
            .map_err(rail_err)?
            .map(LightningAddress::from))
    }

    /// Whether `username` is free on the Lightning address server.
    pub async fn lightning_address_available(&self, username: &str) -> Result<bool> {
        self.sdk
            .check_lightning_address_available(breez_sdk_spark::CheckLightningAddressRequest {
                username: username.to_string(),
            })
            .await
            .map_err(rail_err)
    }

    /// Claim `username@<domain>`. Payments to it arrive in this account,
    /// even while the app is closed.
    pub async fn register_lightning_address(&self, username: &str) -> Result<LightningAddress> {
        let info = self
            .sdk
            .register_lightning_address(breez_sdk_spark::RegisterLightningAddressRequest {
                username: username.to_string(),
                description: None,
            })
            .await
            .map_err(rail_err)?;
        Ok(LightningAddress::from(info))
    }

    /// Call `f` whenever a payment or balance changes, so the UI can update
    /// immediately instead of waiting for the next sync. Returns a listener id.
    pub async fn on_change(&self, f: impl Fn() + Send + Sync + 'static) -> String {
        struct Listener<F>(F);
        #[async_trait]
        impl<F: Fn() + Send + Sync> breez_sdk_spark::EventListener for Listener<F> {
            async fn on_event(&self, event: breez_sdk_spark::SdkEvent) {
                use breez_sdk_spark::SdkEvent::*;
                if matches!(
                    event,
                    Synced
                        | PaymentSucceeded { .. }
                        | PaymentPending { .. }
                        | PaymentFailed { .. }
                        | ClaimedDeposits { .. }
                ) {
                    (self.0)();
                }
            }
        }
        self.sdk.add_event_listener(Box::new(Listener(f))).await
    }

    async fn balance_sat(&self, ensure_synced: bool) -> Result<Sats> {
        let info = self
            .sdk
            .get_info(GetInfoRequest {
                ensure_synced: Some(ensure_synced),
            })
            .await
            .map_err(rail_err)?;
        Ok(info.balance_sats)
    }

    /// Map an SDK error from a spend, turning the SDK's insufficient-funds
    /// variant into the core one with real numbers.
    async fn spend_err(&self, e: SdkError, needed: Sats) -> Error {
        if matches!(e, SdkError::InsufficientFunds { .. }) {
            if let Ok(available) = self.balance_sat(false).await {
                return Error::InsufficientFunds { needed, available };
            }
        }
        rail_err(e)
    }

    fn check_network(&self, found: breez_sdk_spark::BitcoinNetwork) -> Result<()> {
        let want = map::parsed_network(self.network);
        if found != want {
            return Err(Error::Network(format!(
                "this request is for {found:?}, the account is on {want:?}"
            )));
        }
        Ok(())
    }

    fn store_quote(&self, prepared: Prepared) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let mut quotes = self.quotes.lock().expect("quote map poisoned");
        quotes.retain(|_, q| q.created.elapsed() < QUOTE_TTL);
        quotes.insert(
            id.clone(),
            PendingQuote {
                prepared,
                created: Instant::now(),
            },
        );
        id
    }

    #[allow(clippy::too_many_arguments)]
    fn quote(
        &self,
        method: Method,
        amount: Sats,
        fee: Sats,
        policy: FeePolicy,
        balance: Sats,
        prepared: Prepared,
    ) -> Result<SendQuote> {
        let (amount_sat, fee_sat) = map::split(amount, fee, policy)?;
        let needed = amount_sat.saturating_add(fee_sat);
        if needed > balance {
            return Err(Error::InsufficientFunds {
                needed,
                available: balance,
            });
        }
        let id = self.store_quote(prepared);
        Ok(SendQuote {
            account: self.id.clone(),
            method,
            amount_sat,
            fee_sat,
            handle: QuoteHandle::new(self.id.clone(), id).encode(),
            needs_signature: None,
            private: false,
            payjoin: false,
            notes: vec![],
        })
    }
}

#[async_trait]
impl Rail for SparkAccount {
    fn id(&self) -> &AccountId {
        &self.id
    }

    fn kind(&self) -> RailKind {
        RailKind::Spark
    }

    fn set_name(&self, name: &str) -> Result<()> {
        *self.name.write().unwrap_or_else(|e| e.into_inner()) = name.to_string();
        Ok(())
    }

    async fn info(&self) -> Result<AccountInfo> {
        let confirmed = self.balance_sat(false).await?;

        let pending = self
            .sdk
            .list_payments(ListPaymentsRequest {
                status_filter: Some(vec![SdkStatus::Pending]),
                asset_filter: Some(AssetFilter::Bitcoin),
                ..Default::default()
            })
            .await
            .map_err(rail_err)?;
        let (mut pending_in, mut pending_out) = (0u64, 0u64);
        for p in pending.payments.iter().filter(|p| !p.is_conversion_child()) {
            let amount = map::sats(p.amount)?;
            match p.payment_type {
                PaymentType::Receive => pending_in = pending_in.saturating_add(amount),
                PaymentType::Send => {
                    pending_out = pending_out
                        .saturating_add(amount)
                        .saturating_add(map::sats(p.fees)?)
                }
            }
        }
        // On-chain deposits the SDK has seen but not yet claimed into Spark.
        let deposits = self
            .sdk
            .list_unclaimed_deposits(ListUnclaimedDepositsRequest {})
            .await
            .map_err(rail_err)?;
        for d in deposits
            .deposits
            .iter()
            .filter(|d| d.refund_tx_id.is_none())
        {
            pending_in = pending_in.saturating_add(d.amount_sats);
        }

        Ok(AccountInfo {
            id: self.id.clone(),
            name: self.name.read().unwrap_or_else(|e| e.into_inner()).clone(),
            rail: RailKind::Spark,
            custody: Custody::Hot,
            balance: Balance {
                confirmed,
                pending_in,
                pending_out,
            },
            receives: RECEIVES.to_vec(),
            pays: PAYS.to_vec(),
            group: None,
            account_index: None,
        })
    }

    /// The SDK keeps itself in sync and tells us when anything changes (see
    /// `on_change`). A manual sync just nudges it, in the background, so a
    /// slow Spark operator never holds up (or times out) a refresh.
    async fn sync(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        if self.syncing.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let sdk = self.sdk.clone();
        let flag = self.syncing.clone();
        tokio::spawn(async move {
            if let Err(e) = sdk.sync_wallet(SyncWalletRequest {}).await {
                tracing::info!("spark background sync: {e}");
            }
            flag.store(false, Ordering::SeqCst);
        });
        Ok(())
    }

    async fn receive(&self, req: ReceiveRequest) -> Result<ReceiveTarget> {
        let payment_method = match req.method {
            // A plain address carries no amount; with one, issue a Spark invoice.
            Method::SparkAddress => match req.amount_sat {
                None => ReceivePaymentMethod::SparkAddress,
                Some(amount) => ReceivePaymentMethod::SparkInvoice {
                    amount: Some(u128::from(amount)),
                    token_identifier: None,
                    expiry_time: None,
                    description: req.description.clone(),
                    sender_public_key: None,
                },
            },
            Method::Bolt11 => ReceivePaymentMethod::Bolt11Invoice {
                description: req.description.clone().unwrap_or_default(),
                amount_sats: req.amount_sat,
                expiry_secs: None,
                payment_hash: None,
                receiver_identity_public_key: None,
            },
            // The SDK's static deposit address; amounts are not encoded.
            Method::BitcoinAddress => ReceivePaymentMethod::BitcoinAddress { new_address: None },
            other => return Err(Error::Unsupported(format!("receive via {other:?}"))),
        };

        let resp = self
            .sdk
            .receive_payment(ReceivePaymentRequest { payment_method })
            .await
            .map_err(rail_err)?;
        let fee = map::sats(resp.fee)?;

        let mut target = ReceiveTarget {
            method: req.method,
            request: resp.payment_request,
            amount_sat: None,
            fee_sat: None,
            expires_at: None,
            watch_id: None,
        };
        match req.method {
            Method::SparkAddress => {
                // Spark-to-Spark transfers are free to receive.
                target.amount_sat = req.amount_sat;
                target.fee_sat = Some(fee);
            }
            Method::Bolt11 => {
                // 0.26.1 always reports 0 here; a non-zero value is real.
                target.fee_sat = (fee > 0).then_some(fee);
                if let Ok(InputType::Bolt11Invoice(d)) = self.sdk.parse(&target.request).await {
                    target.amount_sat = d.amount_msat.map(|m| m / 1000);
                    target.expires_at = Some(d.timestamp.saturating_add(d.expiry));
                    target.watch_id = Some(d.payment_hash);
                } else {
                    target.amount_sat = req.amount_sat;
                }
            }
            Method::BitcoinAddress => {
                // The claim fee is set by on-chain conditions when funds arrive.
                target.watch_id = Some(target.request.clone());
            }
            _ => unreachable!("rejected above"),
        }
        Ok(target)
    }

    async fn prepare_send(&self, req: SendRequest) -> Result<SendQuote> {
        let (method, input) = map::destination(&req.destination)?;
        let parsed = self.sdk.parse(&input).await.map_err(rail_err)?;

        let requested = req.amount_sat.or_else(|| req.destination.amount_sat());
        let balance = self.balance_sat(true).await?;
        let policy = if req.send_max {
            FeePolicy::FeesIncluded
        } else {
            FeePolicy::FeesExcluded
        };
        // "Send everything" without an amount spends the whole balance.
        let amount = match (requested, req.send_max) {
            (None, true) => Some(balance),
            (a, _) => a,
        };

        match (method, &parsed) {
            (Method::LightningAddress, InputType::LightningAddress(d)) => {
                self.prepare_lnurl(method, d.pay_request.clone(), amount, policy, balance)
                    .await
            }
            (Method::Lnurl, InputType::LnurlPay(pay_request)) => {
                self.prepare_lnurl(method, pay_request.clone(), amount, policy, balance)
                    .await
            }
            (Method::Lnurl, InputType::LightningAddress(d)) => {
                self.prepare_lnurl(method, d.pay_request.clone(), amount, policy, balance)
                    .await
            }
            (Method::BitcoinAddress, InputType::BitcoinAddress(d)) => {
                self.check_network(d.network)?;
                self.prepare_generic(
                    method,
                    input,
                    amount,
                    policy,
                    Some(map::confirmation_speed(req.fee)?),
                    balance,
                )
                .await
            }
            (Method::Bolt11, InputType::Bolt11Invoice(d)) => {
                self.check_network(d.network)?;
                // A fixed-amount invoice can't absorb fees ("send max" doesn't apply).
                let policy = if d.amount_msat.is_some() {
                    FeePolicy::FeesExcluded
                } else {
                    policy
                };
                let amount = if d.amount_msat.is_some() {
                    requested
                } else {
                    amount
                };
                self.prepare_generic(method, input, amount, policy, None, balance)
                    .await
            }
            (Method::SparkAddress, InputType::SparkAddress(d)) => {
                self.check_network(d.network)?;
                self.prepare_generic(method, input, amount, policy, None, balance)
                    .await
            }
            (Method::SparkAddress, InputType::SparkInvoice(d)) => {
                self.check_network(d.network)?;
                if d.token_identifier.is_some() {
                    return Err(Error::Unsupported("pay Spark token invoices".into()));
                }
                let policy = if d.amount.is_some() {
                    FeePolicy::FeesExcluded
                } else {
                    policy
                };
                let amount = if d.amount.is_some() {
                    requested
                } else {
                    amount
                };
                self.prepare_generic(method, input, amount, policy, None, balance)
                    .await
            }
            (_, other) => Err(Error::rail(
                RAIL,
                format!(
                    "expected a {method:?} but the SDK read this as {}",
                    input_kind(other)
                ),
            )),
        }
    }

    async fn send(&self, quote: SendQuote, signed_psbt: Option<String>) -> Result<Payment> {
        if signed_psbt.is_some() {
            return Err(Error::Unsupported("use an externally signed PSBT".into()));
        }
        let handle = QuoteHandle::decode(&quote.handle, &self.id)?;
        let prepared = {
            let quotes = self.quotes.lock().expect("quote map poisoned");
            quotes
                .get(&handle.id)
                .map(|q| q.prepared.clone())
                .ok_or_else(|| {
                    Error::rail(
                        RAIL,
                        "this quote has expired or was already sent; prepare it again",
                    )
                })?
        };
        let needed = quote.amount_sat.saturating_add(quote.fee_sat);

        // The quote id doubles as the SDK idempotency key, so retrying a
        // failed or interrupted send cannot pay twice.
        let result = match prepared {
            Prepared::Lnurl { response } => self
                .sdk
                .lnurl_pay(LnurlPayRequest {
                    prepare_response: response,
                    idempotency_key: Some(handle.id.clone()),
                })
                .await
                .map(|r| r.payment),
            Prepared::Send { response, speed } => {
                let options = match &response.payment_method {
                    SendPaymentMethod::BitcoinAddress { .. } => {
                        Some(SendPaymentOptions::BitcoinAddress {
                            confirmation_speed: speed.unwrap_or(OnchainConfirmationSpeed::Medium),
                        })
                    }
                    SendPaymentMethod::Bolt11Invoice { .. } => {
                        Some(SendPaymentOptions::Bolt11Invoice {
                            prefer_spark: false,
                            completion_timeout_secs: Some(LIGHTNING_COMPLETION_TIMEOUT_SECS),
                        })
                    }
                    _ => None,
                };
                self.sdk
                    .send_payment(SendPaymentRequest {
                        prepare_response: response,
                        options,
                        idempotency_key: Some(handle.id.clone()),
                    })
                    .await
                    .map(|r| r.payment)
            }
        };

        let payment = match result {
            Ok(p) => p,
            Err(e) => return Err(self.spend_err(e, needed).await),
        };
        self.quotes
            .lock()
            .expect("quote map poisoned")
            .remove(&handle.id);

        let mut out = map::payment(&self.id, &payment)?;
        out.method = quote.method;
        Ok(out)
    }

    async fn payments(&self, limit: usize) -> Result<Vec<Payment>> {
        let limit = u32::try_from(limit).unwrap_or(u32::MAX);
        let resp = self
            .sdk
            .list_payments(ListPaymentsRequest {
                asset_filter: Some(AssetFilter::Bitcoin),
                limit: Some(limit),
                sort_ascending: Some(false),
                ..Default::default()
            })
            .await
            .map_err(rail_err)?;
        resp.payments
            .iter()
            .filter(|p| !p.is_conversion_child())
            .map(|p| map::payment(&self.id, p))
            .collect()
    }
}

impl SparkAccount {
    async fn prepare_lnurl(
        &self,
        method: Method,
        pay_request: breez_sdk_spark::LnurlPayRequestDetails,
        amount: Option<Sats>,
        policy: FeePolicy,
        balance: Sats,
    ) -> Result<SendQuote> {
        let amount = amount.ok_or(Error::AmountRequired)?;
        let response = self
            .sdk
            .prepare_lnurl_pay(PrepareLnurlPayRequest {
                amount: u128::from(amount),
                pay_request,
                // The note stays local: LNURL comments are sent to the payee.
                comment: None,
                validate_success_action_url: None,
                token_identifier: None,
                conversion_options: None,
                fee_policy: Some(policy),
            })
            .await
            .map_err(rail_err)?;
        let (total, fee, applied) = (response.amount_sats, response.fee_sats, response.fee_policy);
        self.quote(
            method,
            total,
            fee,
            applied,
            balance,
            Prepared::Lnurl { response },
        )
    }

    async fn prepare_generic(
        &self,
        method: Method,
        input: String,
        amount: Option<Sats>,
        policy: FeePolicy,
        speed: Option<OnchainConfirmationSpeed>,
        balance: Sats,
    ) -> Result<SendQuote> {
        let response = self
            .sdk
            .prepare_send_payment(PrepareSendPaymentRequest {
                payment_request: breez_sdk_spark::PaymentRequest::Input { input },
                amount: amount.map(u128::from),
                token_identifier: None,
                conversion_options: None,
                fee_policy: Some(policy),
            })
            .await
            .map_err(rail_err)?;

        if response.token_identifier.is_some() || response.conversion_estimate.is_some() {
            return Err(Error::Unsupported(
                "send tokens or token conversions".into(),
            ));
        }
        let fee = match (&response.payment_method, &speed) {
            (SendPaymentMethod::BitcoinAddress { fee_quote, .. }, Some(speed)) => match speed {
                OnchainConfirmationSpeed::Fast => fee_quote.speed_fast.total_fee_sat(),
                OnchainConfirmationSpeed::Medium => fee_quote.speed_medium.total_fee_sat(),
                OnchainConfirmationSpeed::Slow => fee_quote.speed_slow.total_fee_sat(),
            },
            // We always send with `prefer_spark: false`, so the Lightning fee applies.
            (
                SendPaymentMethod::Bolt11Invoice {
                    lightning_fee_sats, ..
                },
                None,
            ) => *lightning_fee_sats,
            (
                SendPaymentMethod::SparkAddress { fee, .. }
                | SendPaymentMethod::SparkInvoice { fee, .. },
                None,
            ) => map::sats(*fee)?,
            (SendPaymentMethod::CrossChainAddress { .. }, _) => {
                return Err(Error::Unsupported("send cross-chain".into()));
            }
            _ => {
                return Err(Error::rail(
                    RAIL,
                    "the SDK prepared a different payment type than requested",
                ))
            }
        };
        let amount = map::sats(response.amount)?;
        let applied = response.fee_policy;
        self.quote(
            method,
            amount,
            fee,
            applied,
            balance,
            Prepared::Send { response, speed },
        )
    }
}

fn input_kind(i: &InputType) -> &'static str {
    match i {
        InputType::BitcoinAddress(_) => "a bitcoin address",
        InputType::Bolt11Invoice(_) => "a BOLT11 invoice",
        InputType::Bolt12Invoice(_)
        | InputType::Bolt12Offer(_)
        | InputType::Bolt12InvoiceRequest(_) => "BOLT12",
        InputType::LightningAddress(_) => "a Lightning address",
        InputType::LnurlPay(_) => "an LNURL-pay link",
        InputType::LnurlWithdraw(_) => "an LNURL-withdraw link",
        InputType::LnurlAuth(_) => "an LNURL-auth link",
        InputType::SilentPaymentAddress(_) => "a silent payment address",
        InputType::Url(_) => "a URL",
        InputType::Bip21(_) => "a BIP21 URI",
        InputType::SparkAddress(_) => "a Spark address",
        InputType::SparkInvoice(_) => "a Spark invoice",
        InputType::CrossChainAddress(_) => "a cross-chain address",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn config(network: Network, mnemonic: &str, api_key: &str) -> SparkConfig {
        SparkConfig {
            id: "spend".into(),
            name: "Spending".into(),
            mnemonic: mnemonic.into(),
            network,
            storage_dir: std::env::temp_dir().join("rail-spark-never-created"),
            api_key: api_key.into(),
        }
    }

    // These all fail before any file or network access.
    #[tokio::test]
    async fn connect_rejects_unsupported_networks() {
        for n in [Network::Testnet, Network::Signet] {
            assert!(matches!(
                SparkAccount::connect(config(n, PHRASE, "k")).await,
                Err(Error::Network(_))
            ));
        }
    }

    #[tokio::test]
    async fn connect_rejects_bad_mnemonic() {
        let r = SparkAccount::connect(config(Network::Regtest, "not a real phrase", "k")).await;
        assert!(matches!(r, Err(Error::InvalidMnemonic(_))));
    }

    #[tokio::test]
    async fn connect_requires_api_key_on_mainnet() {
        let r = SparkAccount::connect(config(Network::Bitcoin, PHRASE, "  ")).await;
        assert!(matches!(r, Err(Error::Rail { .. })));
    }
}

//! Lightning → on-chain receives through the user's own Spending account.
//!
//! Boltz suspended its swap service on 3 Aug 2026, so an on-chain account
//! (hot savings, hardware, multisig, watch-only) can no longer be paid over
//! Lightning through a reverse swap. [`SparkForwarder`] replaces that bridge:
//!
//! 1. `bridge_receive` issues a Spark Lightning invoice for the payer's amount
//!    and journals a [`ForwardRecord`] naming the on-chain address it is for.
//! 2. Once the invoice is paid into Spending, `tick` withdraws the whole
//!    received amount to that address (a Spark cooperative exit at the
//!    "medium" speed), so the address gets `received − on-chain fee`.
//!
//! Nothing here is custodial beyond Spark itself: if the app closes after the
//! payment arrives, the sats simply wait in Spending and are forwarded on the
//! next unlock. If they can't be forwarded (the fee is more than the payment),
//! they stay in Spending and the record says so.
//!
//! Sending is idempotent: the record is journaled as `sending` with a fixed
//! idempotency key (a UUID the SDK uses as the Spark transfer id) *before* the
//! SDK is asked to send, and a retry, including after a restart, first looks
//! that key up among the SDK's payments and reuses it for any resend.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use breez_sdk_spark::{
    AssetFilter, FeePolicy, GetPaymentRequest, InputType, ListPaymentsRequest,
    OnchainConfirmationSpeed, PaymentDetails, PaymentStatus as SdkStatus, PaymentType,
    PrepareSendPaymentRequest, ReceivePaymentMethod, ReceivePaymentRequest, SendPaymentMethod,
    SendPaymentOptions, SendPaymentRequest,
};
use serde::{Deserialize, Serialize};
use sessions_core::rail::{BridgedPayment, SwapStatus};
use sessions_core::{
    Error, Method, Network, PaymentRequest, ReceiveQuote, ReceiveTarget, Result, Sats, SwapProvider,
};
use tokio::sync::Mutex;

use crate::{map, SparkAccount};

/// Provider name, shown to the user as who does the conversion.
pub const PROVIDER: &str = "Spending";

/// How long the Lightning invoice stays payable.
const INVOICE_EXPIRY_SECS: u32 = 24 * 3600;
/// Keep watching an invoice this long past its expiry before giving up on it.
const EXPIRY_GRACE_SECS: u64 = 10 * 60;
/// Smallest payment we bridge. A Spark withdrawal costs a flat ~750 sat
/// service fee plus ~250 vB of miner fee (see [`estimate_medium_fee`]), so
/// below this most of the payment would go to fees. The live minimum is the
/// larger of this, twice the current fee, and fee + dust (see [`min_sat`]).
pub const MIN_FORWARD_SAT: Sats = 10_000;
/// Largest payment we bridge. Neither the SDK nor Spark publish a Lightning
/// receive limit; this keeps a single forward within what Spark's Lightning
/// service routes reliably.
pub const MAX_FORWARD_SAT: Sats = 10_000_000;
/// Amount used to price the fee when the user hasn't entered one yet.
const FEE_SAMPLE_SAT: Sats = 100_000;
/// Retry backoff: 30 s, doubling, capped at an hour.
const RETRY_BASE_SECS: u64 = 30;
const RETRY_MAX_SECS: u64 = 3600;
/// Give up after this many attempts that found Spending short of the amount
/// (the sats were spent elsewhere), rather than retrying forever.
const SHORT_FUNDS_ATTEMPTS: u32 = 3;

// The SDK's local fee estimate for a withdrawal it can't price yet, copied
// from breez-sdk-spark 0.26.1, crates/breez-sdk/core/src/sdk/payments/prepare/
// bitcoin_address.rs (`ESTIMATE_*`): the provider prices every withdrawal at
// 250 vB plus a flat 750 sat service fee; "medium" sits 2 sat/vB above the
// fastest network rate, plus 1 sat/vB of headroom.
const ESTIMATE_USER_FEE_SAT: Sats = 750;
const ESTIMATE_VSIZE_VBYTES: u64 = 250;
const ESTIMATE_MEDIUM_PREMIUM_SAT_PER_VBYTE: u64 = 2;
const ESTIMATE_DRIFT_HEADROOM_SAT_PER_VBYTE: u64 = 1;

/// Well-formed P2WPKH addresses used only to price a withdrawal before the
/// real destination is known (the provider prices every withdrawal at the
/// same size, whatever the address type). Never sent to.
const SAMPLE_MAINNET_ADDRESS: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
const SAMPLE_REGTEST_ADDRESS: &str = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";

const STORE_VERSION: u32 = 1;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn rail(e: impl std::fmt::Display) -> Error {
    Error::rail(PROVIDER, e)
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ForwardStatus {
    /// Invoice issued, not paid yet.
    AwaitingPayment,
    /// Paid into Spending; the withdrawal hasn't started.
    Received,
    /// Journaled before asking the SDK to send. The send may or may not have
    /// gone out; `send_key` settles which.
    Sending,
    /// Withdrawal handed to Spark.
    Sent,
    /// Won't be forwarded; any received sats stay in Spending.
    Failed,
    /// The invoice expired unpaid.
    Expired,
}

impl ForwardStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ForwardStatus::AwaitingPayment => "awaitingPayment",
            ForwardStatus::Received => "received",
            ForwardStatus::Sending => "sending",
            ForwardStatus::Sent => "sent",
            ForwardStatus::Failed => "failed",
            ForwardStatus::Expired => "expired",
        }
    }

    pub fn finished(self) -> bool {
        matches!(
            self,
            ForwardStatus::Sent | ForwardStatus::Failed | ForwardStatus::Expired
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardRecord {
    /// The invoice's payment hash; also the receive target's `watch_id`.
    pub id: String,
    pub invoice: String,
    pub payment_hash: String,
    /// The on-chain address the sats are forwarded to.
    pub destination: String,
    /// Invoice amount: what the payer pays.
    pub amount_sat: Sats,
    pub created_at: u64,
    pub expires_at: u64,
    pub status: ForwardStatus,
    /// What actually arrived in Spending.
    #[serde(default)]
    pub received_sat: Option<Sats>,
    /// On-chain fee of the withdrawal (last quote, or what was paid).
    #[serde(default)]
    pub fee_sat: Option<Sats>,
    /// Idempotency key (Spark transfer id) of the withdrawal.
    #[serde(default)]
    pub send_key: Option<String>,
    #[serde(default)]
    pub txid: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub next_attempt_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

impl ForwardRecord {
    fn swap_status(&self) -> SwapStatus {
        SwapStatus {
            swap_id: self.id.clone(),
            state: self.status.as_str().to_string(),
            finished: self.status.finished(),
        }
    }

    /// Schedule the next try after a failure.
    fn back_off(&mut self, at: u64, error: String) {
        self.attempts = self.attempts.saturating_add(1);
        let exp = self.attempts.saturating_sub(1).min(16);
        let wait = RETRY_BASE_SECS
            .saturating_mul(1u64 << exp)
            .min(RETRY_MAX_SECS);
        self.next_attempt_at = at.saturating_add(wait);
        self.error = Some(error);
    }

    fn fail(&mut self, error: String) {
        self.status = ForwardStatus::Failed;
        self.error = Some(error);
    }
}

/// The JSON journal of forwards.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardStore {
    pub version: u32,
    pub network: Network,
    pub forwards: Vec<ForwardRecord>,
}

impl ForwardStore {
    pub fn new(network: Network) -> Self {
        ForwardStore {
            version: STORE_VERSION,
            network,
            forwards: Vec::new(),
        }
    }

    /// A missing file is an empty journal; an unreadable one is an error
    /// (never start over silently: a record may describe sats to forward).
    pub fn load(path: &Path, network: Network) -> Result<Self> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ForwardStore::new(network))
            }
            Err(e) => return Err(Error::Storage(format!("read {}: {e}", path.display()))),
        };
        let store: ForwardStore = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Storage(format!("parse {}: {e}", path.display())))?;
        if store.version > STORE_VERSION {
            return Err(Error::Storage(format!(
                "{} was written by a newer version of this app",
                path.display()
            )));
        }
        if store.network != network {
            return Err(Error::Network(format!(
                "forward journal is for {:?} but the wallet is on {network:?}",
                store.network
            )));
        }
        Ok(store)
    }

    /// Atomic write: temp file, fsync, rename.
    pub fn save(&self, path: &Path) -> Result<()> {
        let storage = |what: &str, e: std::io::Error| Error::Storage(format!("{what}: {e}"));
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| storage("create journal dir", e))?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(|e| Error::Storage(e.to_string()))?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .map_err(|e| storage("open temp journal", e))?;
        f.write_all(&json)
            .map_err(|e| storage("write temp journal", e))?;
        f.sync_all().map_err(|e| storage("sync temp journal", e))?;
        drop(f);
        fs::rename(&tmp, path).map_err(|e| storage("replace journal", e))
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// The address in an on-chain receive request: a plain address or a
/// `bitcoin:` BIP21 URI (any query, e.g. `amount` or `pj`, is dropped).
pub(crate) fn address_from_request(request: &str) -> &str {
    let r = request.trim();
    let r = match r.get(..8) {
        Some(scheme) if scheme.eq_ignore_ascii_case("bitcoin:") => &r[8..],
        _ => r,
    };
    r.split('?').next().unwrap_or(r)
}

/// Parse `request` as an address on `network`, returning the normalised
/// address and its dust limit. Accepts the upper-case bech32 QR form.
pub(crate) fn parse_address(request: &str, network: Network) -> Result<(String, Sats)> {
    use bitcoin::address::{Address, NetworkUnchecked};
    let raw = address_from_request(request);
    let unchecked = Address::<NetworkUnchecked>::from_str(raw)
        .or_else(|_| Address::<NetworkUnchecked>::from_str(&raw.to_ascii_lowercase()))
        .map_err(|e| Error::Invalid(format!("That isn't a valid Bitcoin address: {e}")))?;
    let address = unchecked
        .require_network(network.to_bitcoin())
        .map_err(|e| Error::Network(format!("Bitcoin address is for the wrong network: {e}")))?;
    let dust = address.script_pubkey().minimal_non_dust().to_sat();
    Ok((address.to_string(), dust))
}

/// What reaches the address when `received` sats are withdrawn and `fee` comes
/// out of them. `Err` holds a plain explanation when the fee eats it all.
pub(crate) fn forward_amount(received: Sats, fee: Sats, dust: Sats) -> Result<Sats, String> {
    match received.checked_sub(fee) {
        Some(out) if out >= dust => Ok(out),
        _ => Err(format!(
            "{received} sats arrived, but the on-chain fee to forward them is {fee} sats, \
             which leaves too little to send. They were kept in Spending."
        )),
    }
}

/// The SDK's own estimate of a "medium" withdrawal fee at `fastest_sat_per_vb`.
pub(crate) fn estimate_medium_fee(fastest_sat_per_vb: u64) -> Sats {
    fastest_sat_per_vb
        .saturating_add(ESTIMATE_MEDIUM_PREMIUM_SAT_PER_VBYTE)
        .saturating_add(ESTIMATE_DRIFT_HEADROOM_SAT_PER_VBYTE)
        .saturating_mul(ESTIMATE_VSIZE_VBYTES)
        .saturating_add(ESTIMATE_USER_FEE_SAT)
}

/// Smallest amount worth forwarding at `fee`: [`MIN_FORWARD_SAT`], at least
/// twice the fee (so most of it arrives), and always above fee + dust.
pub(crate) fn min_sat(fee: Sats, dust: Sats) -> Sats {
    MIN_FORWARD_SAT
        .max(fee.saturating_mul(2))
        .max(fee.saturating_add(dust).saturating_add(1))
}

pub(crate) fn sample_address(network: Network) -> &'static str {
    match network {
        Network::Regtest => SAMPLE_REGTEST_ADDRESS,
        _ => SAMPLE_MAINNET_ADDRESS,
    }
}

// ---------------------------------------------------------------------------
// What the forwarder needs from Spark (mocked in tests)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Invoice {
    pub bolt11: String,
    pub payment_hash: String,
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Received {
    pub payment_hash: String,
    pub amount_sat: Sats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SentStatus {
    Pending,
    Complete,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sent {
    pub txid: Option<String>,
    pub fee_sat: Sats,
    pub status: SentStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Withdrawal {
    Sent(Sent),
    /// The fee is now above the one we checked; nothing was sent.
    FeeRose(Sats),
}

#[async_trait]
pub(crate) trait ForwardBackend: Send + Sync {
    fn network(&self) -> Network;
    async fn create_invoice(
        &self,
        amount: Sats,
        description: &str,
        expiry_secs: u32,
    ) -> Result<Invoice>;
    /// Completed incoming Lightning payments since `since` (unix seconds).
    async fn received(&self, since: u64) -> Result<Vec<Received>>;
    /// "Medium" fee to withdraw `amount` (fees included) to `address`.
    async fn withdraw_fee(&self, address: &str, amount: Sats) -> Result<Sats>;
    /// The current fastest network fee rate, sat/vB.
    async fn fastest_fee_rate(&self) -> Result<u64>;
    /// Withdraw `amount` (fees included) at medium speed, unless the fee is
    /// now above `max_fee`. `key` makes the send idempotent.
    async fn withdraw(
        &self,
        address: &str,
        amount: Sats,
        max_fee: Sats,
        key: &str,
    ) -> Result<Withdrawal>;
    /// The payment sent with idempotency key `key`, if the SDK has it.
    async fn find_sent(&self, key: &str) -> Result<Option<Sent>>;
}

// ---------------------------------------------------------------------------
// The provider
// ---------------------------------------------------------------------------

pub struct SparkForwarder {
    backend: Arc<dyn ForwardBackend>,
    network: Network,
    store_path: PathBuf,
    store: Mutex<ForwardStore>,
    /// Serialises `tick` so a record is driven by one task at a time.
    driver: Mutex<()>,
}

impl SparkForwarder {
    /// `store_path` is the JSON journal file (created on first use).
    pub fn new(account: Arc<SparkAccount>, store_path: PathBuf) -> Result<Self> {
        Self::with_backend(account, store_path)
    }

    pub(crate) fn with_backend(
        backend: Arc<dyn ForwardBackend>,
        store_path: PathBuf,
    ) -> Result<Self> {
        let network = backend.network();
        let store = ForwardStore::load(&store_path, network)?;
        Ok(Self {
            backend,
            network,
            store_path,
            store: Mutex::new(store),
            driver: Mutex::new(()),
        })
    }

    /// Every journaled forward, oldest first.
    pub async fn forwards(&self) -> Vec<ForwardRecord> {
        self.store.lock().await.forwards.clone()
    }

    /// Journal one record (insert or replace by id) and write the file.
    async fn put(&self, r: &ForwardRecord) -> Result<()> {
        let mut store = self.store.lock().await;
        let mut next = store.clone();
        match next.forwards.iter_mut().find(|x| x.id == r.id) {
            Some(slot) => *slot = r.clone(),
            None => next.forwards.push(r.clone()),
        }
        next.save(&self.store_path)?;
        *store = next;
        Ok(())
    }

    /// The withdrawal fee for `amount` to `address`: Spark's live quote, or
    /// the SDK's own estimate when Spark can't price it (e.g. Spending holds
    /// less than `amount` yet).
    async fn quote_fee(&self, address: &str, amount: Sats) -> Result<Sats> {
        match self.backend.withdraw_fee(address, amount).await {
            Ok(fee) => Ok(fee),
            Err(e) => {
                tracing::debug!("spark withdrawal quote unavailable ({e}); estimating");
                Ok(estimate_medium_fee(self.backend.fastest_fee_rate().await?))
            }
        }
    }

    pub async fn tick_at(&self, at: u64) -> Result<Vec<SwapStatus>> {
        let _driver = self.driver.lock().await;
        let active: Vec<ForwardRecord> = self
            .store
            .lock()
            .await
            .forwards
            .iter()
            .filter(|r| !r.status.finished())
            .cloned()
            .collect();
        if active.is_empty() {
            return Ok(vec![]);
        }

        // One listing covers every unpaid invoice.
        let waiting = active
            .iter()
            .filter(|r| r.status == ForwardStatus::AwaitingPayment)
            .map(|r| r.created_at)
            .min();
        let received = match waiting {
            // Allow for clock skew between us and the payment timestamps.
            Some(since) => match self.backend.received(since.saturating_sub(3600)).await {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("couldn't list Spending payments: {e}");
                    None
                }
            },
            None => None,
        };

        let mut out = Vec::with_capacity(active.len());
        for mut r in active {
            let before = r.clone();
            if let Err(e) = self.drive(&mut r, received.as_deref(), at).await {
                tracing::warn!(forward = %r.id, "forward step failed: {e}");
            }
            if r != before {
                r.updated_at = at;
                self.put(&r).await?;
            }
            out.push(r.swap_status());
        }
        Ok(out)
    }

    /// Move one record forward. Errors are already recorded on `r` (with a
    /// retry time); they are returned only for logging.
    async fn drive(
        &self,
        r: &mut ForwardRecord,
        received: Option<&[Received]>,
        at: u64,
    ) -> Result<()> {
        match r.status {
            ForwardStatus::AwaitingPayment => {
                if let Some(p) = received
                    .unwrap_or_default()
                    .iter()
                    .find(|p| p.payment_hash.eq_ignore_ascii_case(&r.payment_hash))
                {
                    r.status = ForwardStatus::Received;
                    r.received_sat = Some(p.amount_sat);
                    r.next_attempt_at = 0;
                    r.error = None;
                    // Forward straight away.
                    return Box::pin(self.drive(r, received, at)).await;
                }
                // Only give up once a listing has confirmed it's unpaid.
                if received.is_some() && at > r.expires_at.saturating_add(EXPIRY_GRACE_SECS) {
                    r.status = ForwardStatus::Expired;
                }
                Ok(())
            }
            ForwardStatus::Received | ForwardStatus::Sending => {
                if at < r.next_attempt_at {
                    return Ok(());
                }
                match self.forward(r, at).await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let short = matches!(e, Error::InsufficientFunds { .. });
                        r.back_off(at, e.to_string());
                        if short && r.attempts >= SHORT_FUNDS_ATTEMPTS {
                            r.fail(format!(
                                "Spending no longer holds the {} sats to forward (they were \
                                 spent), so nothing was sent on-chain.",
                                r.received_sat.unwrap_or(r.amount_sat)
                            ));
                        }
                        Err(e)
                    }
                }
            }
            ForwardStatus::Sent | ForwardStatus::Failed | ForwardStatus::Expired => Ok(()),
        }
    }

    /// Withdraw a received record's sats to its address, exactly once.
    async fn forward(&self, r: &mut ForwardRecord, at: u64) -> Result<()> {
        let received = r
            .received_sat
            .ok_or_else(|| Error::Storage(format!("forward {} has no received amount", r.id)))?;
        let (address, dust) = parse_address(&r.destination, self.network)?;

        // A send may already have gone out (e.g. the app closed mid-send).
        if let Some(key) = r.send_key.clone() {
            if let Some(sent) = self.backend.find_sent(&key).await? {
                return self.settle(r, sent, at).await;
            }
        }

        let mut fee = self.backend.withdraw_fee(&address, received).await?;
        // Re-quote once if the fee moves between our check and the send.
        for round in 0..2 {
            if let Err(msg) = forward_amount(received, fee, dust) {
                r.fee_sat = Some(fee);
                r.fail(msg);
                return Ok(());
            }
            // Journal "sending" with its key before the SDK can move funds.
            let key = r
                .send_key
                .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
                .clone();
            r.status = ForwardStatus::Sending;
            r.fee_sat = Some(fee);
            r.updated_at = at;
            self.put(r).await?;

            match self.backend.withdraw(&address, received, fee, &key).await? {
                Withdrawal::Sent(sent) => return self.settle(r, sent, at).await,
                Withdrawal::FeeRose(new) if round == 0 => fee = new,
                Withdrawal::FeeRose(new) => {
                    r.fee_sat = Some(new);
                    return Err(rail(format!(
                        "the on-chain fee keeps changing (now {new} sats); will try again"
                    )));
                }
            }
        }
        unreachable!("the loop returns on its second round")
    }

    /// Record what the SDK did with our withdrawal.
    async fn settle(&self, r: &mut ForwardRecord, sent: Sent, at: u64) -> Result<()> {
        if sent.status == SentStatus::Failed {
            // Spark rejected it and the sats are back in Spending: try again
            // later under a new key (the old one is spent).
            r.status = ForwardStatus::Received;
            r.send_key = None;
            r.back_off(at, "Spark couldn't complete the withdrawal".into());
            return Ok(());
        }
        r.status = ForwardStatus::Sent;
        r.fee_sat = Some(sent.fee_sat);
        r.txid = sent.txid;
        r.error = None;
        Ok(())
    }
}

#[async_trait]
impl SwapProvider for SparkForwarder {
    fn name(&self) -> &str {
        PROVIDER
    }

    fn pay_bridges(&self) -> Vec<(Method, Method)> {
        vec![]
    }

    fn receive_bridges(&self) -> Vec<(Method, Method)> {
        vec![(Method::Bolt11, Method::BitcoinAddress)]
    }

    async fn bridge_payment(
        &self,
        _target: PaymentRequest,
        _amount_sat: Sats,
        _payer_method: Method,
        _refund_address: Option<String>,
    ) -> Result<BridgedPayment> {
        Err(Error::Unsupported("pay through Spending forwarding".into()))
    }

    async fn bridge_receive(&self, ours: ReceiveTarget, outside: Method) -> Result<ReceiveTarget> {
        if (outside, ours.method) != (Method::Bolt11, Method::BitcoinAddress) {
            return Err(Error::Unsupported(format!(
                "receive {outside:?} into {:?}",
                ours.method
            )));
        }
        // Checked before any network call.
        let amount = ours
            .amount_sat
            .filter(|a| *a > 0)
            .ok_or(Error::AmountRequired)?;
        let (address, dust) = parse_address(&ours.request, self.network)?;
        if amount > MAX_FORWARD_SAT {
            return Err(Error::Invalid(format!(
                "Lightning payments into an on-chain account are limited to {MAX_FORWARD_SAT} sats"
            )));
        }
        let fee = self.quote_fee(&address, amount).await?;
        let min = min_sat(fee, dust);
        if amount < min {
            return Err(Error::Invalid(format!(
                "Receive at least {min} sats over Lightning into an on-chain account: \
                 moving it on-chain costs about {fee} sats"
            )));
        }

        let invoice = self
            .backend
            .create_invoice(amount, "Sessions Wallet", INVOICE_EXPIRY_SECS)
            .await?;
        let t = now();
        let record = ForwardRecord {
            id: invoice.payment_hash.clone(),
            invoice: invoice.bolt11.clone(),
            payment_hash: invoice.payment_hash.clone(),
            destination: address,
            amount_sat: amount,
            created_at: t,
            expires_at: invoice.expires_at,
            status: ForwardStatus::AwaitingPayment,
            received_sat: None,
            fee_sat: Some(fee),
            send_key: None,
            txid: None,
            error: None,
            attempts: 0,
            next_attempt_at: 0,
            updated_at: t,
        };
        self.put(&record).await?;
        Ok(ReceiveTarget {
            method: Method::Bolt11,
            request: invoice.bolt11,
            amount_sat: Some(amount),
            fee_sat: Some(fee),
            expires_at: Some(invoice.expires_at),
            watch_id: Some(invoice.payment_hash),
        })
    }

    async fn receive_quote(
        &self,
        outside: Method,
        ours: Method,
        amount_sat: Option<Sats>,
    ) -> Result<Option<ReceiveQuote>> {
        if (outside, ours) != (Method::Bolt11, Method::BitcoinAddress) {
            return Ok(None);
        }
        let sample = sample_address(self.network);
        let (_, dust) = parse_address(sample, self.network)?;
        let priced = amount_sat.filter(|a| *a > 0).unwrap_or(FEE_SAMPLE_SAT);
        let fee = self.quote_fee(sample, priced).await?;
        Ok(Some(ReceiveQuote {
            provider: PROVIDER.into(),
            min_sat: min_sat(fee, dust),
            max_sat: MAX_FORWARD_SAT,
            fee_percent: 0.0,
            network_fee_sat: fee,
            total_fee_sat: amount_sat.map(|_| fee),
            receive_sat: amount_sat.map(|a| a.saturating_sub(fee)),
        }))
    }

    async fn tick(&self) -> Result<Vec<SwapStatus>> {
        self.tick_at(now()).await
    }
}

// ---------------------------------------------------------------------------
// The real backend: a Spark account
// ---------------------------------------------------------------------------

fn sent_from(p: &breez_sdk_spark::Payment) -> Result<Sent> {
    Ok(Sent {
        txid: match &p.details {
            Some(PaymentDetails::Withdraw { tx_id }) if !tx_id.is_empty() => Some(tx_id.clone()),
            _ => None,
        },
        fee_sat: map::sats(p.fees)?,
        status: match p.status {
            SdkStatus::Pending => SentStatus::Pending,
            SdkStatus::Completed => SentStatus::Complete,
            SdkStatus::Failed => SentStatus::Failed,
        },
    })
}

impl SparkAccount {
    async fn prepare_withdrawal(
        &self,
        address: &str,
        amount: Sats,
    ) -> Result<(breez_sdk_spark::PrepareSendPaymentResponse, Sats)> {
        let response = match self
            .sdk
            .prepare_send_payment(PrepareSendPaymentRequest {
                payment_request: breez_sdk_spark::PaymentRequest::Input {
                    input: address.to_string(),
                },
                amount: Some(u128::from(amount)),
                token_identifier: None,
                conversion_options: None,
                // The whole received amount leaves Spending; the fee comes out of it.
                fee_policy: Some(FeePolicy::FeesIncluded),
            })
            .await
        {
            Ok(r) => r,
            Err(e) => return Err(self.spend_err(e, amount).await),
        };
        let SendPaymentMethod::BitcoinAddress { fee_quote, .. } = &response.payment_method else {
            return Err(rail(
                "Spark prepared something other than an on-chain withdrawal",
            ));
        };
        let fee = fee_quote.speed_medium.total_fee_sat();
        Ok((response, fee))
    }
}

#[async_trait]
impl ForwardBackend for SparkAccount {
    fn network(&self) -> Network {
        match self.network {
            breez_sdk_spark::Network::Mainnet => Network::Bitcoin,
            breez_sdk_spark::Network::Regtest => Network::Regtest,
            // `connect` refuses Signet (see `map::sdk_network`).
            breez_sdk_spark::Network::Signet => Network::Signet,
        }
    }

    async fn create_invoice(
        &self,
        amount: Sats,
        description: &str,
        expiry_secs: u32,
    ) -> Result<Invoice> {
        let resp = self
            .sdk
            .receive_payment(ReceivePaymentRequest {
                payment_method: ReceivePaymentMethod::Bolt11Invoice {
                    description: description.to_string(),
                    amount_sats: Some(amount),
                    expiry_secs: Some(expiry_secs),
                    payment_hash: None,
                    receiver_identity_public_key: None,
                },
            })
            .await
            .map_err(crate::rail_err)?;
        let InputType::Bolt11Invoice(d) = self
            .sdk
            .parse(&resp.payment_request)
            .await
            .map_err(crate::rail_err)?
        else {
            return Err(rail(
                "Spark returned something other than a Lightning invoice",
            ));
        };
        if d.amount_msat != Some(amount.saturating_mul(1000)) {
            return Err(rail(format!(
                "Spark's invoice is for {:?} msat, expected {amount} sats",
                d.amount_msat
            )));
        }
        Ok(Invoice {
            bolt11: resp.payment_request,
            payment_hash: d.payment_hash,
            expires_at: d.timestamp.saturating_add(d.expiry),
        })
    }

    async fn received(&self, since: u64) -> Result<Vec<Received>> {
        let resp = self
            .sdk
            .list_payments(ListPaymentsRequest {
                type_filter: Some(vec![PaymentType::Receive]),
                status_filter: Some(vec![SdkStatus::Completed]),
                asset_filter: Some(AssetFilter::Bitcoin),
                from_timestamp: Some(since),
                ..Default::default()
            })
            .await
            .map_err(crate::rail_err)?;
        resp.payments
            .iter()
            .filter_map(|p| match &p.details {
                Some(PaymentDetails::Lightning { htlc_details, .. }) => {
                    Some(map::sats(p.amount).map(|amount_sat| Received {
                        payment_hash: htlc_details.payment_hash.clone(),
                        amount_sat,
                    }))
                }
                _ => None,
            })
            .collect()
    }

    async fn withdraw_fee(&self, address: &str, amount: Sats) -> Result<Sats> {
        Ok(self.prepare_withdrawal(address, amount).await?.1)
    }

    async fn fastest_fee_rate(&self) -> Result<u64> {
        Ok(self
            .sdk
            .recommended_fees()
            .await
            .map_err(crate::rail_err)?
            .fastest_fee)
    }

    async fn withdraw(
        &self,
        address: &str,
        amount: Sats,
        max_fee: Sats,
        key: &str,
    ) -> Result<Withdrawal> {
        let (response, fee) = self.prepare_withdrawal(address, amount).await?;
        if fee > max_fee {
            return Ok(Withdrawal::FeeRose(fee));
        }
        let sent = self
            .sdk
            .send_payment(SendPaymentRequest {
                prepare_response: response,
                options: Some(SendPaymentOptions::BitcoinAddress {
                    confirmation_speed: OnchainConfirmationSpeed::Medium,
                }),
                // Used as the Spark transfer id, so a repeat can't pay twice.
                idempotency_key: Some(key.to_string()),
            })
            .await;
        match sent {
            Ok(r) => Ok(Withdrawal::Sent(sent_from(&r.payment)?)),
            Err(e) => Err(self.spend_err(e, amount).await),
        }
    }

    async fn find_sent(&self, key: &str) -> Result<Option<Sent>> {
        // Local storage lookup; a miss means the SDK never recorded a send
        // under this key, and a resend reuses the key anyway.
        match self
            .sdk
            .get_payment(GetPaymentRequest {
                payment_id: key.to_string(),
            })
            .await
        {
            Ok(r) => Ok(Some(sent_from(&r.payment)?)),
            Err(_) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    const ADDR: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
    const T0: u64 = 1_790_000_000;

    #[derive(Default)]
    struct Mock {
        received: StdMutex<Vec<Received>>,
        /// Fees returned by successive quotes (last one repeats).
        fees: StdMutex<Vec<Sats>>,
        fail_quote: StdMutex<bool>,
        short_funds: StdMutex<bool>,
        /// Make the next `withdraw` error after recording the send (as if
        /// the reply was lost).
        lose_reply: StdMutex<bool>,
        /// Sends by key: (address, amount, fee, status).
        sends: StdMutex<Vec<(String, String, Sats, Sats)>>,
        sent_status: StdMutex<Option<SentStatus>>,
        invoices: StdMutex<u32>,
    }

    impl Mock {
        fn fee(&self) -> Sats {
            let mut f = self.fees.lock().unwrap();
            if f.len() > 1 {
                f.remove(0)
            } else {
                f.first().copied().unwrap_or(1_000)
            }
        }
        fn pay(&self, hash: &str, amount: Sats) {
            self.received.lock().unwrap().push(Received {
                payment_hash: hash.into(),
                amount_sat: amount,
            });
        }
        fn sent(&self, fee: Sats) -> Sent {
            Sent {
                txid: Some("ab".repeat(32)),
                fee_sat: fee,
                status: self
                    .sent_status
                    .lock()
                    .unwrap()
                    .unwrap_or(SentStatus::Pending),
            }
        }
    }

    #[async_trait]
    impl ForwardBackend for Mock {
        fn network(&self) -> Network {
            Network::Bitcoin
        }
        async fn create_invoice(&self, amount: Sats, _d: &str, expiry: u32) -> Result<Invoice> {
            let mut n = self.invoices.lock().unwrap();
            *n += 1;
            Ok(Invoice {
                bolt11: format!("lnbc{amount}n1mock{n}"),
                payment_hash: format!("{:064x}", *n),
                expires_at: now() + u64::from(expiry),
            })
        }
        async fn received(&self, _since: u64) -> Result<Vec<Received>> {
            Ok(self.received.lock().unwrap().clone())
        }
        async fn withdraw_fee(&self, _a: &str, amount: Sats) -> Result<Sats> {
            if *self.short_funds.lock().unwrap() {
                return Err(Error::InsufficientFunds {
                    needed: amount,
                    available: 0,
                });
            }
            if *self.fail_quote.lock().unwrap() {
                return Err(rail("offline"));
            }
            Ok(self.fee())
        }
        async fn fastest_fee_rate(&self) -> Result<u64> {
            Ok(4)
        }
        async fn withdraw(
            &self,
            address: &str,
            amount: Sats,
            max_fee: Sats,
            key: &str,
        ) -> Result<Withdrawal> {
            let fee = self.fee();
            if fee > max_fee {
                return Ok(Withdrawal::FeeRose(fee));
            }
            let mut sends = self.sends.lock().unwrap();
            // The SDK's idempotency: a known key returns the earlier payment.
            if sends.iter().any(|(k, ..)| k == key) {
                return Ok(Withdrawal::Sent(self.sent(fee)));
            }
            sends.push((key.into(), address.into(), amount - fee, fee));
            drop(sends);
            if std::mem::take(&mut *self.lose_reply.lock().unwrap()) {
                return Err(rail("connection reset"));
            }
            Ok(Withdrawal::Sent(self.sent(fee)))
        }
        async fn find_sent(&self, key: &str) -> Result<Option<Sent>> {
            let sends = self.sends.lock().unwrap();
            Ok(sends
                .iter()
                .find(|(k, ..)| k == key)
                .map(|(.., fee)| self.sent(*fee)))
        }
    }

    fn forwarder(dir: &Path) -> (SparkForwarder, Arc<Mock>) {
        let mock = Arc::new(Mock::default());
        let f = SparkForwarder::with_backend(mock.clone(), dir.join("forwards.json")).unwrap();
        (f, mock)
    }

    fn ours(request: &str, amount_sat: Option<Sats>) -> ReceiveTarget {
        ReceiveTarget {
            method: Method::BitcoinAddress,
            request: request.into(),
            amount_sat,
            fee_sat: None,
            expires_at: None,
            watch_id: None,
        }
    }

    #[test]
    fn address_extraction() {
        assert_eq!(address_from_request(ADDR), ADDR);
        assert_eq!(address_from_request(&format!("  {ADDR}\n")), ADDR);
        assert_eq!(
            address_from_request(&format!(
                "BITCOIN:{}?amount=0.001&pj=https://x.example/a",
                ADDR.to_uppercase()
            )),
            ADDR.to_uppercase()
        );
        assert_eq!(address_from_request("bitcoin€x"), "bitcoin€x");

        let (a, dust) = parse_address(
            &format!("bitcoin:{}?amount=1", ADDR.to_uppercase()),
            Network::Bitcoin,
        )
        .unwrap();
        assert_eq!((a.as_str(), dust), (ADDR, 294));
        let (_, dust) =
            parse_address("1BoatSLRHtKNngkdXEeobR76b53LETtpyT", Network::Bitcoin).unwrap();
        assert_eq!(dust, 546);
        assert!(matches!(
            parse_address(SAMPLE_REGTEST_ADDRESS, Network::Bitcoin),
            Err(Error::Network(_))
        ));
        assert!(parse_address(SAMPLE_REGTEST_ADDRESS, Network::Regtest).is_ok());
        assert!(parse_address("bc1qnope", Network::Bitcoin).is_err());
        for n in [Network::Bitcoin, Network::Regtest] {
            assert!(parse_address(sample_address(n), n).is_ok());
        }
    }

    #[test]
    fn amounts_and_fees() {
        assert_eq!(forward_amount(50_000, 1_750, 294), Ok(48_250));
        assert_eq!(forward_amount(2_044, 1_750, 294), Ok(294));
        assert!(forward_amount(2_043, 1_750, 294).is_err());
        let msg = forward_amount(1_000, 1_750, 294).unwrap_err();
        assert!(msg.contains("kept in Spending"), "{msg}");

        // 4 sat/vB fastest → (4 + 2 + 1) × 250 + 750.
        assert_eq!(estimate_medium_fee(4), 2_500);
        assert_eq!(estimate_medium_fee(u64::MAX), u64::MAX);

        assert_eq!(min_sat(1_750, 294), MIN_FORWARD_SAT);
        assert_eq!(min_sat(8_000, 294), 16_000);
        assert!(min_sat(u64::MAX / 2 + 1, 294) > 0);
    }

    #[test]
    fn store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("forwards.json");
        assert_eq!(
            ForwardStore::load(&path, Network::Bitcoin).unwrap(),
            ForwardStore::new(Network::Bitcoin)
        );
        let mut s = ForwardStore::new(Network::Bitcoin);
        s.forwards.push(ForwardRecord {
            id: "h".into(),
            invoice: "lnbc1".into(),
            payment_hash: "h".into(),
            destination: ADDR.into(),
            amount_sat: 50_000,
            created_at: T0,
            expires_at: T0 + 10,
            status: ForwardStatus::Sending,
            received_sat: Some(50_000),
            fee_sat: Some(1_750),
            send_key: Some("k".into()),
            txid: None,
            error: Some("x".into()),
            attempts: 2,
            next_attempt_at: T0 + 60,
            updated_at: T0,
        });
        s.save(&path).unwrap();
        assert_eq!(ForwardStore::load(&path, Network::Bitcoin).unwrap(), s);
        let json = fs::read_to_string(&path).unwrap();
        assert!(json.contains("\"status\": \"sending\""), "{json}");
        assert!(json.contains("\"sendKey\""), "{json}");
        assert!(matches!(
            ForwardStore::load(&path, Network::Regtest),
            Err(Error::Network(_))
        ));
        fs::write(&path, b"{not json").unwrap();
        assert!(matches!(
            ForwardStore::load(&path, Network::Bitcoin),
            Err(Error::Storage(_))
        ));
    }

    #[tokio::test]
    async fn bridges_and_bad_requests() {
        let dir = tempfile::tempdir().unwrap();
        let (f, mock) = forwarder(dir.path());
        assert_eq!(f.name(), "Spending");
        assert!(f.pay_bridges().is_empty());
        assert_eq!(
            f.receive_bridges(),
            vec![(Method::Bolt11, Method::BitcoinAddress)]
        );
        assert!(matches!(
            f.bridge_payment(
                PaymentRequest::Bolt11 {
                    invoice: "lnbc1".into()
                },
                1,
                Method::Bolt11,
                None
            )
            .await,
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            f.bridge_receive(ours(ADDR, Some(50_000)), Method::LiquidAddress)
                .await,
            Err(Error::Unsupported(_))
        ));
        for amount in [None, Some(0)] {
            assert!(matches!(
                f.bridge_receive(ours(ADDR, amount), Method::Bolt11).await,
                Err(Error::AmountRequired)
            ));
        }
        assert!(matches!(
            f.bridge_receive(ours(SAMPLE_REGTEST_ADDRESS, Some(50_000)), Method::Bolt11)
                .await,
            Err(Error::Network(_))
        ));
        // Too small to be worth the on-chain fee, or above the cap.
        for amount in [5_000, MAX_FORWARD_SAT + 1] {
            assert!(matches!(
                f.bridge_receive(ours(ADDR, Some(amount)), Method::Bolt11)
                    .await,
                Err(Error::Invalid(_))
            ));
        }
        assert_eq!(*mock.invoices.lock().unwrap(), 0);
        assert!(f.forwards().await.is_empty());
        assert!(!dir.path().join("forwards.json").exists());
    }

    #[tokio::test]
    async fn quote_uses_live_fee_or_sdk_estimate() {
        let dir = tempfile::tempdir().unwrap();
        let (f, mock) = forwarder(dir.path());
        assert_eq!(
            f.receive_quote(Method::LiquidAddress, Method::BitcoinAddress, None)
                .await
                .unwrap(),
            None
        );
        mock.fees.lock().unwrap().push(1_750);
        let q = f
            .receive_quote(Method::Bolt11, Method::BitcoinAddress, Some(50_000))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            q,
            ReceiveQuote {
                provider: "Spending".into(),
                min_sat: 10_000,
                max_sat: MAX_FORWARD_SAT,
                fee_percent: 0.0,
                network_fee_sat: 1_750,
                total_fee_sat: Some(1_750),
                receive_sat: Some(48_250),
            }
        );
        // Spark can't price it (e.g. empty Spending): the SDK's estimate.
        *mock.fail_quote.lock().unwrap() = true;
        let q = f
            .receive_quote(Method::Bolt11, Method::BitcoinAddress, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((q.network_fee_sat, q.total_fee_sat), (2_500, None));
    }

    #[tokio::test]
    async fn paid_invoice_is_forwarded_once() {
        let dir = tempfile::tempdir().unwrap();
        let (f, mock) = forwarder(dir.path());
        mock.fees.lock().unwrap().push(1_750);
        let t = f
            .bridge_receive(
                ours(
                    &format!("bitcoin:{}?amount=0.0005", ADDR.to_uppercase()),
                    Some(50_000),
                ),
                Method::Bolt11,
            )
            .await
            .unwrap();
        assert_eq!(t.method, Method::Bolt11);
        assert!(t.request.starts_with("lnbc50000n1mock"));
        assert_eq!((t.amount_sat, t.fee_sat), (Some(50_000), Some(1_750)));
        let hash = t.watch_id.clone().unwrap();
        let rec = &f.forwards().await[0];
        assert_eq!(rec.status, ForwardStatus::AwaitingPayment);
        assert_eq!(rec.destination, ADDR);
        assert_eq!(Some(rec.expires_at), t.expires_at);

        // Not paid yet.
        let st = f.tick_at(now()).await.unwrap();
        assert_eq!(st[0].state, "awaitingPayment");
        assert!(!st[0].finished);

        mock.pay(&hash, 50_000);
        let st = f.tick_at(now()).await.unwrap();
        assert_eq!(st[0].state, "sent");
        assert!(st[0].finished);
        let sends = mock.sends.lock().unwrap().clone();
        assert_eq!(sends.len(), 1);
        assert_eq!(
            (sends[0].1.as_str(), sends[0].2, sends[0].3),
            (ADDR, 48_250, 1_750)
        );
        let rec = &f.forwards().await[0];
        assert_eq!(rec.txid.as_deref(), Some("ab".repeat(32).as_str()));
        assert_eq!(rec.received_sat, Some(50_000));

        // Finished records are left alone, and it survives a reload.
        assert!(f.tick_at(now()).await.unwrap().is_empty());
        let (again, _) = forwarder(dir.path());
        assert_eq!(again.forwards().await[0].status, ForwardStatus::Sent);
        assert_eq!(mock.sends.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fee_above_received_fails_and_keeps_funds() {
        let dir = tempfile::tempdir().unwrap();
        let (f, mock) = forwarder(dir.path());
        mock.fees.lock().unwrap().push(1_000);
        let t = f
            .bridge_receive(ours(ADDR, Some(10_000)), Method::Bolt11)
            .await
            .unwrap();
        // Fees spiked between the invoice and the payment.
        *mock.fees.lock().unwrap() = vec![12_000];
        mock.pay(t.watch_id.as_ref().unwrap(), 10_000);
        let st = f.tick_at(now()).await.unwrap();
        assert_eq!((st[0].state.as_str(), st[0].finished), ("failed", true));
        let rec = &f.forwards().await[0];
        assert!(rec.error.as_ref().unwrap().contains("kept in Spending"));
        assert!(mock.sends.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn fee_change_is_requoted_once() {
        let dir = tempfile::tempdir().unwrap();
        let (f, mock) = forwarder(dir.path());
        mock.fees.lock().unwrap().push(1_750);
        let t = f
            .bridge_receive(ours(ADDR, Some(50_000)), Method::Bolt11)
            .await
            .unwrap();
        mock.pay(t.watch_id.as_ref().unwrap(), 50_000);
        // Quote 1 750, then 2 000 at send (re-quote), then 2 000 again.
        *mock.fees.lock().unwrap() = vec![1_750, 2_000, 2_000];
        f.tick_at(now()).await.unwrap();
        let sends = mock.sends.lock().unwrap().clone();
        assert_eq!(sends.len(), 1);
        assert_eq!((sends[0].2, sends[0].3), (48_000, 2_000));

        // A fee that keeps rising is retried later, not chased.
        let dir = tempfile::tempdir().unwrap();
        let (f, mock) = forwarder(dir.path());
        mock.fees.lock().unwrap().push(1_750);
        let t = f
            .bridge_receive(ours(ADDR, Some(50_000)), Method::Bolt11)
            .await
            .unwrap();
        mock.pay(t.watch_id.as_ref().unwrap(), 50_000);
        *mock.fees.lock().unwrap() = vec![1_750, 2_000, 2_500, 1_000];
        let at = now();
        f.tick_at(at).await.unwrap();
        let rec = f.forwards().await[0].clone();
        assert_eq!(rec.status, ForwardStatus::Sending);
        assert_eq!(rec.next_attempt_at, at + RETRY_BASE_SECS);
        assert!(mock.sends.lock().unwrap().is_empty());
        // Backoff respected, then it goes through under the same key.
        f.tick_at(at + 1).await.unwrap();
        assert!(mock.sends.lock().unwrap().is_empty());
        f.tick_at(at + RETRY_BASE_SECS).await.unwrap();
        let sends = mock.sends.lock().unwrap().clone();
        assert_eq!(sends.len(), 1);
        assert_eq!(Some(&sends[0].0), rec.send_key.as_ref());
    }

    #[tokio::test]
    async fn sending_is_idempotent_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let (f, mock) = forwarder(dir.path());
        mock.fees.lock().unwrap().push(1_750);
        let t = f
            .bridge_receive(ours(ADDR, Some(50_000)), Method::Bolt11)
            .await
            .unwrap();
        mock.pay(t.watch_id.as_ref().unwrap(), 50_000);
        // The send goes out but the reply is lost.
        *mock.lose_reply.lock().unwrap() = true;
        let at = now();
        f.tick_at(at).await.unwrap();
        let rec = f.forwards().await[0].clone();
        assert_eq!(rec.status, ForwardStatus::Sending);
        assert!(rec.send_key.is_some());
        assert_eq!(rec.attempts, 1);
        assert_eq!(mock.sends.lock().unwrap().len(), 1);

        // "Restart": a new forwarder over the same journal and wallet finds
        // the earlier send by its key instead of sending again.
        drop(f);
        let f =
            SparkForwarder::with_backend(mock.clone(), dir.path().join("forwards.json")).unwrap();
        let st = f.tick_at(at + RETRY_BASE_SECS).await.unwrap();
        assert_eq!(st[0].state, "sent");
        assert_eq!(mock.sends.lock().unwrap().len(), 1);
        assert_eq!(f.forwards().await[0].send_key, rec.send_key);

        // A journal stuck in "sending" whose send never reached the SDK is
        // sent under the key it already has.
        let mut stuck = rec.clone();
        stuck.id = "other".into();
        stuck.payment_hash = "other".into();
        stuck.send_key = Some("fixed-key".into());
        stuck.next_attempt_at = 0;
        f.put(&stuck).await.unwrap();
        f.tick_at(at + RETRY_BASE_SECS).await.unwrap();
        let sends = mock.sends.lock().unwrap().clone();
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[1].0, "fixed-key");
    }

    #[tokio::test]
    async fn spark_rejection_retries_under_a_new_key() {
        let dir = tempfile::tempdir().unwrap();
        let (f, mock) = forwarder(dir.path());
        mock.fees.lock().unwrap().push(1_750);
        let t = f
            .bridge_receive(ours(ADDR, Some(50_000)), Method::Bolt11)
            .await
            .unwrap();
        mock.pay(t.watch_id.as_ref().unwrap(), 50_000);
        *mock.sent_status.lock().unwrap() = Some(SentStatus::Failed);
        let at = now();
        f.tick_at(at).await.unwrap();
        let rec = f.forwards().await[0].clone();
        assert_eq!(rec.status, ForwardStatus::Received);
        assert_eq!(rec.send_key, None);
        *mock.sent_status.lock().unwrap() = Some(SentStatus::Complete);
        f.tick_at(rec.next_attempt_at).await.unwrap();
        let sends = mock.sends.lock().unwrap().clone();
        assert_eq!(sends.len(), 2);
        assert_ne!(sends[0].0, sends[1].0);
        assert_eq!(f.forwards().await[0].status, ForwardStatus::Sent);
    }

    #[tokio::test]
    async fn spent_funds_fail_after_a_few_tries() {
        let dir = tempfile::tempdir().unwrap();
        let (f, mock) = forwarder(dir.path());
        mock.fees.lock().unwrap().push(1_750);
        let t = f
            .bridge_receive(ours(ADDR, Some(50_000)), Method::Bolt11)
            .await
            .unwrap();
        mock.pay(t.watch_id.as_ref().unwrap(), 50_000);
        *mock.short_funds.lock().unwrap() = true;
        let mut at = now();
        for _ in 0..SHORT_FUNDS_ATTEMPTS {
            f.tick_at(at).await.unwrap();
            at = f.forwards().await[0].next_attempt_at;
        }
        let rec = &f.forwards().await[0];
        assert_eq!(rec.status, ForwardStatus::Failed);
        assert!(rec.error.as_ref().unwrap().contains("no longer holds"));
    }

    #[tokio::test]
    async fn unpaid_invoice_expires() {
        let dir = tempfile::tempdir().unwrap();
        let (f, _) = forwarder(dir.path());
        f.bridge_receive(ours(ADDR, Some(50_000)), Method::Bolt11)
            .await
            .unwrap();
        let exp = f.forwards().await[0].expires_at;
        assert_eq!(
            f.tick_at(exp + 1).await.unwrap()[0].state,
            "awaitingPayment"
        );
        let st = f.tick_at(exp + EXPIRY_GRACE_SECS + 1).await.unwrap();
        assert_eq!((st[0].state.as_str(), st[0].finished), ("expired", true));
    }

    #[test]
    fn backoff_grows_and_caps() {
        let mut r = ForwardRecord {
            id: String::new(),
            invoice: String::new(),
            payment_hash: String::new(),
            destination: String::new(),
            amount_sat: 0,
            created_at: 0,
            expires_at: 0,
            status: ForwardStatus::Received,
            received_sat: None,
            fee_sat: None,
            send_key: None,
            txid: None,
            error: None,
            attempts: 0,
            next_attempt_at: 0,
            updated_at: 0,
        };
        let waits: Vec<u64> = (0..10)
            .map(|_| {
                r.back_off(0, "x".into());
                r.next_attempt_at
            })
            .collect();
        assert_eq!(&waits[..4], &[30, 60, 120, 240]);
        assert_eq!(*waits.last().unwrap(), RETRY_MAX_SECS);
    }
}

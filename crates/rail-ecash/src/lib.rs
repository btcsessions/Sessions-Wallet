//! Cashu ecash account for Sessions Wallet, built on CDK (`cdk` 0.18).
//!
//! One [`EcashAccount`] = one mint, unit = sats. The mint custodies the
//! bitcoin; this account holds bearer ecash proofs in a local redb file.
//!
//! * Lightning in: [`Rail::receive`] creates a NUT-04 mint quote, [`Rail::sync`]
//!   mints any quote that has been paid.
//! * Lightning out: [`Rail::prepare_send`] creates a NUT-05 melt quote,
//!   [`Rail::send`] executes it.
//! * Tokens: [`Rail::redeem`] swaps an incoming `cashuA…`/`cashuB…` token from
//!   this mint into fresh proofs; [`Rail::create_token`] produces one.
//! * History comes straight from CDK's own transaction log in the same redb
//!   file (`Wallet::list_transactions`); this crate keeps no separate log.

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bip39::{Language, Mnemonic};
use cdk::mint_url::MintUrl;
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::{
    CurrencyUnit, MeltOptions, MeltQuoteState, PaymentMethod, ProofsMethods, State, Token,
};
use cdk::wallet::types::{Transaction, TransactionDirection, TransactionId, TransactionStatus};
use cdk::wallet::{MintConnector, ReceiveOptions, SendMemo, SendOptions, Wallet, WalletBuilder};
use cdk::{Amount, Bolt11Invoice};
use cdk_redb::WalletRedbDatabase;
use sessions_core::{
    AccountId, AccountInfo, Balance, Custody, Direction, Error, Method, Payment, PaymentRequest,
    PaymentStatus, RailKind, ReceiveRequest, ReceiveTarget, Result, Sats, SendQuote, SendRequest,
};
use zeroize::Zeroize;

const RAIL: &str = "ecash";

/// Metadata key under which we store our own payment id on CDK transactions
/// whose CDK id we can't learn from the call (token redeems).
const META_PAYMENT_ID: &str = "sessions_payment_id";

fn rail_err(e: impl std::fmt::Display) -> Error {
    Error::rail(RAIL, e)
}

/// Everything needed to open (or create) an ecash account.
pub struct EcashConfig {
    pub id: AccountId,
    pub name: String,
    /// Mint base URL, e.g. `https://mint.example.com`.
    pub mint_url: String,
    /// BIP85 child phrase supplied by the caller (see `sessions_core::keystore::child::ECASH`).
    /// The cdk wallet seed is its BIP39 seed (empty passphrase).
    pub mnemonic: String,
    /// redb file holding proofs, quotes, sagas and transaction history.
    pub db_path: PathBuf,
}

impl std::fmt::Debug for EcashConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EcashConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("mint_url", &self.mint_url)
            .field("mnemonic", &"[redacted]")
            .field("db_path", &self.db_path)
            .finish()
    }
}

/// A Cashu ecash balance held against a single mint.
pub struct EcashAccount {
    id: AccountId,
    name: String,
    wallet: Wallet,
    /// Set when the database file was just created (new device or restore
    /// from the recovery phrase): the next sync scans the mint (NUT-09/13) for
    /// ecash this seed already owns. Stays set until a scan succeeds.
    needs_restore: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for EcashAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EcashAccount")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("mint_url", &self.wallet.mint_url.to_string())
            .finish()
    }
}

impl EcashAccount {
    /// Open the account. Does not touch the network; call [`Rail::sync`]
    /// afterwards to recover interrupted operations and mint paid quotes.
    pub async fn open(config: EcashConfig) -> Result<Self> {
        Self::open_inner(config, None).await
    }

    /// Shared constructor. `client` replaces CDK's HTTP client (tests use an
    /// in-process mint).
    async fn open_inner(
        config: EcashConfig,
        client: Option<Arc<dyn MintConnector + Send + Sync>>,
    ) -> Result<Self> {
        let mint_url = MintUrl::from_str(config.mint_url.trim())
            .map_err(|e| Error::rail(RAIL, format!("invalid mint URL: {e}")))?;

        let normalised = config
            .mnemonic
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let mnemonic = Mnemonic::parse_in_normalized(Language::English, &normalised)
            .map_err(|e| Error::InvalidMnemonic(e.to_string()))?;
        let mut seed: [u8; 64] = mnemonic.to_seed("");

        if let Some(parent) = config.db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| Error::Storage(e.to_string()))?;
            }
        }
        // A brand-new database means we may be restoring from the phrase.
        // (The in-process test mint doesn't implement restore, so tests skip it.)
        let fresh = !config.db_path.exists() && client.is_none();
        // redb opens the file synchronously (and runs migrations); keep that
        // off the async executor.
        let db_path = config.db_path.clone();
        let db = tokio::task::spawn_blocking(move || {
            ensure_redb_tables(&db_path)?;
            WalletRedbDatabase::new(&db_path).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| Error::Storage(e.to_string()))?
        .map_err(|e| Error::Storage(e.to_string()))?;

        let mut builder = WalletBuilder::new()
            .mint_url(mint_url)
            .unit(CurrencyUnit::Sat)
            .localstore(Arc::new(db))
            .seed(seed);
        seed.zeroize();
        if let Some(client) = client {
            builder = builder.shared_client(client);
        }
        let wallet = builder.build().map_err(rail_err)?;

        Ok(Self {
            id: config.id,
            name: config.name,
            wallet,
            needs_restore: std::sync::atomic::AtomicBool::new(fresh),
        })
    }

    pub fn mint_url(&self) -> String {
        self.wallet.mint_url.to_string()
    }

    async fn spendable(&self) -> Result<Sats> {
        Ok(self
            .wallet
            .total_balance()
            .await
            .map_err(rail_err)?
            .to_u64())
    }

    /// Turn a CDK error from a spending call into `InsufficientFunds` when it is one.
    async fn spend_err(&self, e: cdk::Error, needed: Sats) -> Error {
        if matches!(e, cdk::Error::InsufficientFunds) {
            let available = self.spendable().await.unwrap_or(0);
            Error::InsufficientFunds { needed, available }
        } else {
            rail_err(e)
        }
    }
}

/// cdk-redb 0.18.2 creates every table at init except `wallet_sagas`, but
/// reads it in `get_incomplete_sagas` (called by `recover_incomplete_sagas`,
/// `finalize_pending_melts`, `get_pending_sends`). On a fresh file that read
/// fails with "Table 'wallet_sagas' does not exist" until the first saga is
/// written. Create it up front (idempotent; same name and key/value types as
/// cdk-redb's `SAGAS_TABLE`). redb locks the file, so this runs and closes
/// before cdk-redb opens it.
fn ensure_redb_tables(path: &std::path::Path) -> std::result::Result<(), String> {
    const SAGAS: redb::TableDefinition<&str, &str> = redb::TableDefinition::new("wallet_sagas");
    let db = redb::Database::create(path).map_err(|e| e.to_string())?;
    let tx = db.begin_write().map_err(|e| e.to_string())?;
    tx.open_table(SAGAS).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn method_from_cdk(m: Option<&PaymentMethod>) -> Method {
    match m {
        // No payment method on the transaction = a token sent or redeemed.
        None => Method::CashuToken,
        Some(PaymentMethod::Known(KnownMethod::Bolt11)) => Method::Bolt11,
        Some(PaymentMethod::Known(KnownMethod::Bolt12)) => Method::Bolt12,
        Some(PaymentMethod::Known(KnownMethod::Onchain)) => Method::BitcoinAddress,
        // This account never creates custom-method quotes; there is no matching
        // `Method`, so fall back to the closest thing that is still ecash-native.
        Some(PaymentMethod::Custom(_)) => Method::CashuToken,
    }
}

fn status_from_cdk(s: TransactionStatus) -> PaymentStatus {
    match s {
        TransactionStatus::Pending => PaymentStatus::Pending,
        TransactionStatus::Completed => PaymentStatus::Complete,
        TransactionStatus::Failed => PaymentStatus::Failed,
    }
}

/// Stable id used for a CDK transaction in `payments()`:
/// our own id if we stored one, else the mint/melt quote id, else CDK's tx id.
fn payment_id(tx: &Transaction) -> String {
    if let Some(id) = tx.metadata.get(META_PAYMENT_ID) {
        return id.clone();
    }
    if let Some(q) = &tx.quote_id {
        return q.clone();
    }
    tx.id().to_string()
}

fn payment_from_tx(account: &AccountId, tx: &Transaction) -> Payment {
    Payment {
        id: payment_id(tx),
        account: account.clone(),
        direction: match tx.direction {
            TransactionDirection::Incoming => Direction::Incoming,
            TransactionDirection::Outgoing => Direction::Outgoing,
        },
        amount_sat: tx.amount.to_u64(),
        fee_sat: tx.fee.to_u64(),
        status: status_from_cdk(tx.status),
        method: method_from_cdk(tx.payment_method.as_ref()),
        timestamp: tx.timestamp,
        description: tx.memo.clone(),
        // CDK does not keep the encoded token after the send saga completes.
        token: None,
    }
}

/// Deterministic id for a redeemed token: CDK's legacy proof-derived id over
/// the token's proof Ys.
fn token_payment_id(token: &Token) -> Result<String> {
    let ys = token
        .token_secrets()
        .into_iter()
        .map(|s| cdk::dhke::hash_to_curve(s.as_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(rail_err)?;
    Ok(TransactionId::new(ys).to_string())
}

/// Pick the BOLT11 option out of a (possibly unified) destination.
fn bolt11_of(dest: &PaymentRequest) -> Result<String> {
    dest.options()
        .into_iter()
        .find_map(|o| match o {
            PaymentRequest::Bolt11 { invoice } => Some(invoice),
            _ => None,
        })
        .ok_or_else(|| {
            let what = dest
                .method()
                .map(|m| format!("{m:?}"))
                .unwrap_or_else(|| "this request".into());
            Error::Unsupported(format!(
                "pay {what} directly (ecash pays BOLT11 invoices only)"
            ))
        })
}

#[async_trait]
impl sessions_core::Rail for EcashAccount {
    fn id(&self) -> &AccountId {
        &self.id
    }

    fn kind(&self) -> RailKind {
        RailKind::Ecash
    }

    async fn info(&self) -> Result<AccountInfo> {
        let confirmed = self.spendable().await?;

        // Paid-but-not-yet-minted Lightning deposits, as last seen locally.
        let pending_in = self
            .wallet
            .get_unissued_mint_quotes()
            .await
            .map_err(rail_err)?
            .iter()
            .map(|q| q.amount_mintable().to_u64())
            .sum();

        // Melts in flight, proofs reserved by an in-progress operation, and
        // tokens handed out but not yet claimed by the recipient.
        let pending_out = self
            .wallet
            .get_proofs_by_states(vec![State::Pending, State::Reserved, State::PendingSpent])
            .await
            .map_err(rail_err)?
            .total_amount()
            .map_err(rail_err)?
            .to_u64();

        Ok(AccountInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            rail: RailKind::Ecash,
            custody: Custody::Mint {
                url: self.wallet.mint_url.to_string(),
            },
            balance: Balance {
                confirmed,
                pending_in,
                pending_out,
            },
            receives: vec![Method::Bolt11],
            pays: vec![Method::Bolt11],
        })
    }

    async fn sync(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        // 0. First sync on a new database: recover ecash this seed already
        //    owns at this mint, so the recovery phrase alone restores it.
        if self.needs_restore.load(Ordering::SeqCst) {
            let restored = self.wallet.restore().await.map_err(rail_err)?;
            self.needs_restore.store(false, Ordering::SeqCst);
            tracing::info!(account = %self.id, ?restored, "ecash: restore scan complete");
        }

        // 1. Finish or roll back anything interrupted (swap/send/receive/melt/mint).
        let report = self
            .wallet
            .recover_incomplete_sagas()
            .await
            .map_err(rail_err)?;
        if !report.is_empty() {
            tracing::info!(account = %self.id, ?report, "ecash: recovered incomplete operations");
        }
        if report.failed > 0 {
            tracing::warn!(account = %self.id, failed = report.failed, "ecash: some operations could not be recovered");
        }

        // 2. Mint every quote the mint now reports as paid.
        let minted = self.wallet.mint_unissued_quotes().await.map_err(rail_err)?;
        if minted > Amount::ZERO {
            tracing::info!(account = %self.id, sats = minted.to_u64(), "ecash: minted paid quotes");
        }

        // 3. Settle Lightning payments that were left pending.
        let finalized = self
            .wallet
            .finalize_pending_melts()
            .await
            .map_err(rail_err)?;
        if !finalized.is_empty() {
            tracing::info!(account = %self.id, count = finalized.len(), "ecash: finalized pending melts");
        }

        // 4. Mark tokens we handed out as complete once the recipient claims them.
        for op in self.wallet.get_pending_sends().await.map_err(rail_err)? {
            if let Err(e) = self.wallet.check_send_status(op).await {
                tracing::warn!(account = %self.id, %op, "ecash: could not check token status: {e}");
            }
        }
        Ok(())
    }

    async fn receive(&self, req: ReceiveRequest) -> Result<ReceiveTarget> {
        if req.method != Method::Bolt11 {
            return Err(Error::Unsupported(format!(
                "receive via {:?} (ecash receives BOLT11 only)",
                req.method
            )));
        }
        let amount = req
            .amount_sat
            .filter(|a| *a > 0)
            .ok_or(Error::AmountRequired)?;

        let quote = match self
            .wallet
            .mint_quote(
                PaymentMethod::BOLT11,
                Some(Amount::from(amount)),
                req.description.clone(),
                None,
            )
            .await
        {
            Err(cdk::Error::InvoiceDescriptionUnsupported) if req.description.is_some() => {
                tracing::debug!(
                    "ecash: mint does not support invoice descriptions; retrying without"
                );
                self.wallet
                    .mint_quote(
                        PaymentMethod::BOLT11,
                        Some(Amount::from(amount)),
                        None,
                        None,
                    )
                    .await
                    .map_err(rail_err)?
            }
            r => r.map_err(rail_err)?,
        };

        Ok(ReceiveTarget {
            method: Method::Bolt11,
            request: quote.request,
            amount_sat: Some(amount),
            // NUT-04 issuance has no protocol fee; we don't claim to know the mint's.
            fee_sat: None,
            expires_at: (quote.expiry > 0).then_some(quote.expiry),
            watch_id: Some(quote.id),
        })
    }

    async fn prepare_send(&self, req: SendRequest) -> Result<SendQuote> {
        let invoice_str = bolt11_of(&req.destination)?;
        if req.send_max {
            return Err(Error::Unsupported("send max over Lightning".into()));
        }
        let invoice = Bolt11Invoice::from_str(invoice_str.trim())
            .map_err(|e| Error::rail(RAIL, format!("invalid BOLT11 invoice: {e}")))?;

        let options = match (invoice.amount_milli_satoshis(), req.amount_sat) {
            (Some(msat), Some(sat)) if msat != sat.saturating_mul(1000) => {
                return Err(Error::rail(
                    RAIL,
                    format!(
                        "amount {sat} sats does not match the invoice ({} msat)",
                        msat
                    ),
                ));
            }
            (Some(_), _) => None,
            (None, Some(sat)) if sat > 0 => Some(MeltOptions::new_amountless(Amount::from(
                sat.saturating_mul(1000),
            ))),
            (None, _) => return Err(Error::AmountRequired),
        };

        let quote = self
            .wallet
            .melt_quote(PaymentMethod::BOLT11, invoice_str.trim(), options, None)
            .await
            .map_err(rail_err)?;

        let amount_sat = quote.amount.to_u64();
        let fee_sat = quote.fee_reserve.to_u64();
        let needed = amount_sat.saturating_add(fee_sat);
        let available = self.spendable().await?;
        if available < needed {
            return Err(Error::InsufficientFunds { needed, available });
        }

        Ok(SendQuote {
            account: self.id.clone(),
            method: Method::Bolt11,
            amount_sat,
            fee_sat,
            handle: quote.id,
            needs_signature: None,
        })
    }

    async fn send(&self, quote: SendQuote, signed_psbt: Option<String>) -> Result<Payment> {
        if signed_psbt.is_some() {
            return Err(Error::Unsupported("use a PSBT".into()));
        }
        if quote.account != self.id {
            return Err(Error::rail(RAIL, "quote belongs to a different account"));
        }
        if quote.method != Method::Bolt11 {
            return Err(Error::Unsupported(format!("send via {:?}", quote.method)));
        }
        let needed = quote.amount_sat.saturating_add(quote.fee_sat);

        let prepared = match self
            .wallet
            .prepare_melt(&quote.handle, HashMap::new())
            .await
        {
            Ok(p) => p,
            Err(e) => return Err(self.spend_err(e, needed).await),
        };
        let finalized = prepared.confirm().await.map_err(rail_err)?;

        let status = match finalized.state() {
            MeltQuoteState::Paid => PaymentStatus::Complete,
            // Unknown is treated as in-flight: the funds are not known to be back.
            MeltQuoteState::Pending | MeltQuoteState::Unknown => PaymentStatus::Pending,
            s @ (MeltQuoteState::Unpaid | MeltQuoteState::Failed) => {
                return Err(Error::rail(
                    RAIL,
                    format!("Lightning payment failed (quote state {s})"),
                ));
            }
        };

        Ok(Payment {
            id: finalized.quote_id().to_string(),
            account: self.id.clone(),
            direction: Direction::Outgoing,
            amount_sat: finalized.amount().to_u64(),
            fee_sat: finalized.fee_paid().to_u64(),
            status,
            method: Method::Bolt11,
            timestamp: now(),
            description: None,
            token: None,
        })
    }

    async fn redeem(&self, request: PaymentRequest) -> Result<Payment> {
        let PaymentRequest::CashuToken { token: encoded } = request else {
            return Err(Error::Unsupported(
                "redeem anything but Cashu tokens".into(),
            ));
        };
        let encoded = encoded.trim();
        let token = Token::from_str(encoded).map_err(|_| Error::UnrecognisedRequest)?;

        let token_mint = token
            .mint_url()
            .map_err(|_| Error::Unsupported("redeem multi-mint tokens".into()))?;
        if token_mint != self.wallet.mint_url {
            return Err(Error::Unsupported(format!(
                "token is from another mint: {token_mint}"
            )));
        }
        let unit = token.unit().unwrap_or_default();
        if unit != CurrencyUnit::Sat {
            return Err(Error::Unsupported(format!(
                "redeem a token denominated in {unit}"
            )));
        }

        let face_value = token.value().map_err(rail_err)?.to_u64();
        let id = token_payment_id(&token)?;
        let opts = ReceiveOptions {
            metadata: HashMap::from([(META_PAYMENT_ID.to_string(), id.clone())]),
            ..Default::default()
        };
        let received = self
            .wallet
            .receive(encoded, opts)
            .await
            .map_err(rail_err)?
            .to_u64();

        Ok(Payment {
            id,
            account: self.id.clone(),
            direction: Direction::Incoming,
            amount_sat: received,
            fee_sat: face_value.saturating_sub(received),
            status: PaymentStatus::Complete,
            method: Method::CashuToken,
            timestamp: now(),
            description: token.memo().clone(),
            token: None,
        })
    }

    async fn create_token(&self, amount_sat: Sats, memo: Option<String>) -> Result<Payment> {
        if amount_sat == 0 {
            return Err(Error::AmountRequired);
        }
        let opts = SendOptions {
            memo: memo.as_deref().map(SendMemo::for_token),
            // The recipient's redeem fee is paid by us, so they net `amount_sat`.
            include_fee: true,
            ..Default::default()
        };
        let prepared = match self
            .wallet
            .prepare_send(Amount::from(amount_sat), opts)
            .await
        {
            Ok(p) => p,
            Err(e) => return Err(self.spend_err(e, amount_sat).await),
        };
        let fee_sat = prepared.fee().to_u64();
        // CDK records the send under a saga-derived transaction id.
        let id = TransactionId::from_saga_id(prepared.operation_id()).to_string();
        let token = prepared.confirm(None).await.map_err(rail_err)?;

        Ok(Payment {
            id,
            account: self.id.clone(),
            direction: Direction::Outgoing,
            amount_sat,
            fee_sat,
            // Pending until the recipient redeems it (see `sync`).
            status: PaymentStatus::Pending,
            method: Method::CashuToken,
            timestamp: now(),
            description: memo,
            token: Some(token.to_string()),
        })
    }

    async fn payments(&self, limit: usize) -> Result<Vec<Payment>> {
        // CDK returns newest first.
        let txs = self
            .wallet
            .list_transactions(None)
            .await
            .map_err(rail_err)?;
        Ok(txs
            .iter()
            .take(limit)
            .map(|tx| payment_from_tx(&self.id, tx))
            .collect())
    }
}

#[cfg(test)]
mod fake_mint;
#[cfg(test)]
mod tests;

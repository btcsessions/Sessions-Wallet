//! The Spending account behind Nostr Wallet Connect (`sessions_nwc`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use breez_sdk_spark::{
    FeePolicy, GetInfoRequest, GetPaymentRequest, InputType, ListPaymentsRequest, PaymentDetails,
    PaymentStatus, PaymentType, PrepareSendPaymentRequest, ReceivePaymentMethod,
    ReceivePaymentRequest, SendPaymentMethod, SendPaymentOptions, SendPaymentRequest,
};
use sessions_core::Network;
use sessions_nwc::{
    ListTransactionsRequest, LookupInvoiceRequest, MakeInvoiceRequest, NwcError, NwcWallet,
    PayInvoiceRequest, PayInvoiceResponse, Transaction, TransactionList, TransactionState,
    TransactionType, WalletInfo,
};

use crate::{SparkAccount, LIGHTNING_COMPLETION_TIMEOUT_SECS};

/// How long `pay_invoice` waits for a payment still in flight after the
/// SDK's own wait, before telling the app it failed (NWC needs a preimage).
const SETTLE_WAIT: Duration = Duration::from_secs(40);
/// Invoices made for apps expire after this unless they ask otherwise.
const DEFAULT_EXPIRY_SECS: u32 = 3600;

pub struct SparkNwc {
    account: Arc<SparkAccount>,
    network: Network,
}

impl SparkNwc {
    pub fn new(account: Arc<SparkAccount>, network: Network) -> Self {
        SparkNwc { account, network }
    }
}

fn internal(e: impl std::fmt::Display) -> NwcError {
    NwcError::internal(e.to_string())
}

/// An idempotency key (a UUID) from the payment hash: paying the same
/// invoice twice can't pay twice.
fn idempotency_key(payment_hash: &str) -> Option<String> {
    let bytes = bitcoin::hex::FromHex::from_hex(payment_hash).ok()?;
    let bytes: Vec<u8> = bytes;
    let first: [u8; 16] = bytes.get(..16)?.try_into().ok()?;
    Some(uuid::Uuid::from_bytes(first).to_string())
}

fn transaction(p: &breez_sdk_spark::Payment) -> Transaction {
    let kind = match p.payment_type {
        PaymentType::Receive => TransactionType::Incoming,
        PaymentType::Send => TransactionType::Outgoing,
    };
    let state = match p.status {
        PaymentStatus::Completed => TransactionState::Settled,
        PaymentStatus::Pending => TransactionState::Pending,
        PaymentStatus::Failed => TransactionState::Failed,
    };
    let (invoice, description, payment_hash, preimage, expires_at) = match &p.details {
        Some(PaymentDetails::Lightning {
            invoice,
            description,
            htlc_details,
            ..
        }) => (
            Some(invoice.clone()),
            description.clone(),
            htlc_details.payment_hash.clone(),
            htlc_details.preimage.clone(),
            Some(htlc_details.expiry_time).filter(|t| *t > 0),
        ),
        // Spark transfers and on-chain payments have no payment hash; the id
        // keeps them distinct.
        _ => (None, None, p.id.clone(), None, None),
    };
    let amount = u64::try_from(p.amount).unwrap_or(u64::MAX / 1000);
    let fees = u64::try_from(p.fees).unwrap_or(0);
    Transaction {
        kind,
        state: Some(state),
        invoice,
        description,
        description_hash: None,
        preimage,
        payment_hash,
        amount_msat: amount.saturating_mul(1000),
        fees_paid_msat: fees.saturating_mul(1000),
        created_at: p.timestamp,
        expires_at: if p.status == PaymentStatus::Pending {
            expires_at
        } else {
            None
        },
        settled_at: (p.status == PaymentStatus::Completed).then_some(p.timestamp),
        metadata: None,
    }
}

impl SparkNwc {
    async fn find_by_hash(&self, hash: &str) -> Result<Option<breez_sdk_spark::Payment>, NwcError> {
        let resp = self
            .account
            .sdk()
            .list_payments(ListPaymentsRequest {
                limit: Some(500),
                sort_ascending: Some(false),
                ..Default::default()
            })
            .await
            .map_err(internal)?;
        Ok(resp.payments.into_iter().find(|p| {
            matches!(&p.details, Some(PaymentDetails::Lightning { htlc_details, .. }) if htlc_details.payment_hash.eq_ignore_ascii_case(hash))
        }))
    }

    /// A Lightning payment still in flight: wait for it to settle (or fail).
    async fn settle(
        &self,
        mut p: breez_sdk_spark::Payment,
    ) -> Result<breez_sdk_spark::Payment, NwcError> {
        let until = tokio::time::Instant::now() + SETTLE_WAIT;
        while p.status == PaymentStatus::Pending && tokio::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Ok(r) = self
                .account
                .sdk()
                .get_payment(GetPaymentRequest {
                    payment_id: p.id.clone(),
                })
                .await
            {
                p = r.payment;
            }
        }
        Ok(p)
    }
}

#[async_trait]
impl NwcWallet for SparkNwc {
    async fn get_info(&self) -> Result<WalletInfo, NwcError> {
        let info = self
            .account
            .sdk()
            .get_info(GetInfoRequest {
                ensure_synced: Some(false),
            })
            .await
            .map_err(internal)?;
        Ok(WalletInfo {
            alias: Some("Sessions Wallet".into()),
            color: Some("#ff9500".into()),
            pubkey: Some(info.identity_pubkey),
            network: match self.network {
                Network::Bitcoin => "mainnet",
                Network::Testnet => "testnet",
                Network::Signet => "signet",
                Network::Regtest => "regtest",
            }
            .into(),
            block_height: None,
            block_hash: None,
        })
    }

    async fn balance_msat(&self) -> Result<u64, NwcError> {
        let info = self
            .account
            .sdk()
            .get_info(GetInfoRequest {
                ensure_synced: Some(false),
            })
            .await
            .map_err(internal)?;
        Ok(info.balance_sats.saturating_mul(1000))
    }

    async fn pay_invoice(&self, req: PayInvoiceRequest) -> Result<PayInvoiceResponse, NwcError> {
        let sdk = self.account.sdk();
        // Whole sats only (Spark has no msats): rounding would spend more
        // than the app was budgeted.
        if !req.total_amount_msat.is_multiple_of(1000) {
            return Err(NwcError::other("This wallet pays whole sats only"));
        }
        let amount = req.amount_msat.map(|m| u128::from(m / 1000));
        let prepared = sdk
            .prepare_send_payment(PrepareSendPaymentRequest {
                payment_request: breez_sdk_spark::PaymentRequest::Input {
                    input: req.invoice.clone(),
                },
                amount,
                token_identifier: None,
                conversion_options: None,
                fee_policy: Some(FeePolicy::FeesExcluded),
            })
            .await
            .map_err(|e| NwcError::payment_failed(e.to_string()))?;
        if !matches!(
            prepared.payment_method,
            SendPaymentMethod::Bolt11Invoice { .. }
        ) {
            return Err(NwcError::other("Not a Lightning invoice"));
        }
        let fee = match &prepared.payment_method {
            SendPaymentMethod::Bolt11Invoice {
                lightning_fee_sats, ..
            } => *lightning_fee_sats,
            _ => 0,
        };
        let balance = self.balance_msat().await? / 1000;
        let needed = u64::try_from(prepared.amount)
            .unwrap_or(u64::MAX)
            .saturating_add(fee);
        if needed > balance {
            return Err(NwcError::insufficient_balance(format!(
                "Spending has {balance} sats; this needs {needed} sats with fees"
            )));
        }
        tracing::info!(account = %self.account.id, "nwc: paying an invoice");
        let payment = sdk
            .send_payment(SendPaymentRequest {
                prepare_response: prepared,
                options: Some(SendPaymentOptions::Bolt11Invoice {
                    prefer_spark: false,
                    completion_timeout_secs: Some(LIGHTNING_COMPLETION_TIMEOUT_SECS),
                }),
                idempotency_key: idempotency_key(&req.payment_hash),
            })
            .await;
        let payment = match payment {
            Ok(r) => r.payment,
            // Did it go out anyway? Then it may still settle: never report
            // that as a plain failure (the app would pay again, and the
            // budget would forget it).
            Err(e) => match self.find_by_hash(&req.payment_hash).await {
                Ok(Some(p)) if p.status != PaymentStatus::Failed => p,
                Ok(_) => return Err(NwcError::payment_failed(e.to_string())),
                Err(_) => return Err(NwcError::in_flight(format!("Outcome unknown: {e}"))),
            },
        };
        let payment = self.settle(payment).await?;
        let preimage = match &payment.details {
            Some(PaymentDetails::Lightning { htlc_details, .. }) => htlc_details.preimage.clone(),
            _ => None,
        };
        match (payment.status, preimage) {
            (PaymentStatus::Completed, Some(preimage)) => Ok(PayInvoiceResponse {
                preimage,
                fees_paid_msat: u64::try_from(payment.fees)
                    .unwrap_or(0)
                    .saturating_mul(1000),
            }),
            (PaymentStatus::Failed, _) => Err(NwcError::payment_failed("The payment failed")),
            _ => Err(NwcError::in_flight(
                "The payment is still in flight; check again later",
            )),
        }
    }

    async fn make_invoice(&self, req: MakeInvoiceRequest) -> Result<Transaction, NwcError> {
        if !req.amount_msat.is_multiple_of(1000) {
            return Err(NwcError::other("This wallet takes whole sats only"));
        }
        if req.description.is_none() && req.description_hash.is_some() {
            return Err(NwcError::new(
                sessions_nwc::ErrorCode::NotImplemented,
                "Invoices with only a description hash aren't supported",
            ));
        }
        let expiry = req
            .expiry
            .and_then(|e| u32::try_from(e).ok())
            .unwrap_or(DEFAULT_EXPIRY_SECS);
        let sdk = self.account.sdk();
        let resp = sdk
            .receive_payment(ReceivePaymentRequest {
                payment_method: ReceivePaymentMethod::Bolt11Invoice {
                    description: req.description.clone().unwrap_or_default(),
                    amount_sats: (req.amount_msat > 0).then_some(req.amount_msat / 1000),
                    expiry_secs: Some(expiry),
                    payment_hash: None,
                    receiver_identity_public_key: None,
                },
            })
            .await
            .map_err(internal)?;
        let invoice = resp.payment_request;
        let details = match sdk.parse(&invoice).await {
            Ok(InputType::Bolt11Invoice(d)) => d,
            _ => return Err(NwcError::internal("couldn't read the invoice just made")),
        };
        Ok(Transaction {
            kind: TransactionType::Incoming,
            state: Some(TransactionState::Pending),
            invoice: Some(invoice),
            description: details.description,
            description_hash: details.description_hash,
            preimage: None,
            payment_hash: details.payment_hash,
            amount_msat: req.amount_msat,
            fees_paid_msat: 0,
            created_at: details.timestamp,
            expires_at: Some(details.timestamp + details.expiry),
            settled_at: None,
            metadata: req.metadata,
        })
    }

    async fn lookup_invoice(
        &self,
        req: LookupInvoiceRequest,
    ) -> Result<Option<Transaction>, NwcError> {
        let sdk = self.account.sdk();
        // The handler always fills the hash; decode the invoice when that's all we got.
        let (hash, decoded) = match (&req.payment_hash, &req.invoice) {
            (Some(h), _) => (h.clone(), None),
            (None, Some(inv)) => match sdk.parse(inv).await {
                Ok(InputType::Bolt11Invoice(d)) => (d.payment_hash.clone(), Some(d)),
                _ => return Err(NwcError::other("That isn't a Lightning invoice")),
            },
            (None, None) => return Err(NwcError::other("Give a payment hash or an invoice")),
        };
        if let Some(p) = self.find_by_hash(&hash).await? {
            return Ok(Some(transaction(&p)));
        }
        // An invoice we made that nobody has paid yet isn't a payment.
        let decoded = match (decoded, &req.invoice) {
            (Some(d), _) => Some(d),
            (None, Some(inv)) => match sdk.parse(inv).await {
                Ok(InputType::Bolt11Invoice(d)) => Some(d),
                _ => None,
            },
            _ => None,
        };
        Ok(decoded.map(|d| {
            let expires = d.timestamp + d.expiry;
            Transaction {
                kind: TransactionType::Incoming,
                state: Some(if expires < sessions_nwc::now() {
                    TransactionState::Expired
                } else {
                    TransactionState::Pending
                }),
                invoice: req.invoice.clone(),
                description: d.description,
                description_hash: d.description_hash,
                preimage: None,
                payment_hash: d.payment_hash,
                amount_msat: d.amount_msat.unwrap_or(0),
                fees_paid_msat: 0,
                created_at: d.timestamp,
                expires_at: Some(expires),
                settled_at: None,
                metadata: None,
            }
        }))
    }

    async fn list_transactions(
        &self,
        req: ListTransactionsRequest,
    ) -> Result<TransactionList, NwcError> {
        let type_filter = req.kind.map(|k| {
            vec![match k {
                TransactionType::Incoming => PaymentType::Receive,
                TransactionType::Outgoing => PaymentType::Send,
            }]
        });
        let status_filter = if req.unpaid {
            None
        } else {
            Some(vec![PaymentStatus::Completed])
        };
        let resp = self
            .account
            .sdk()
            .list_payments(ListPaymentsRequest {
                type_filter,
                status_filter,
                asset_filter: Some(breez_sdk_spark::AssetFilter::Bitcoin),
                from_timestamp: req.from,
                to_timestamp: req.until,
                offset: req.offset.and_then(|o| u32::try_from(o).ok()),
                limit: Some(
                    req.limit
                        .and_then(|l| u32::try_from(l).ok())
                        .unwrap_or(50)
                        .min(100),
                ),
                sort_ascending: Some(false),
                ..Default::default()
            })
            .await
            .map_err(internal)?;
        Ok(TransactionList {
            transactions: resp
                .payments
                .iter()
                .filter(|p| !p.is_conversion_child())
                .map(transaction)
                .collect(),
            total_count: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_key_is_a_uuid_from_the_hash() {
        let k = idempotency_key(&"ab".repeat(32)).unwrap();
        assert_eq!(k.len(), 36);
        assert!(uuid::Uuid::parse_str(&k).is_ok());
        assert_eq!(idempotency_key(&"ab".repeat(32)), Some(k));
        assert!(idempotency_key("zz").is_none());
    }
}

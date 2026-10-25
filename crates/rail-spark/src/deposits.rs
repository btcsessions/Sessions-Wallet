//! On-chain deposits to the Spending account's bitcoin address.
//!
//! Bitcoin sent to the account's static deposit address isn't spendable
//! until it is *claimed* into Spark. The SDK claims on its own once a
//! deposit has [`CONFIRMATIONS_NEEDED`] confirmations, as long as the claim
//! fee is within `max_deposit_claim_fee` (the network's next-block rate
//! plus 1 sat/vB, see [`crate::SparkAccount::connect`]). A deposit it couldn't
//! claim stays listed with the reason; [`SparkAccount::claim_deposit`]
//! claims it with a fee the user approved.
//!
//! An *instant* (early) claim credits a deposit before it matures, for a
//! spread the provider sets. The SDK picks it on its own when a deposit
//! isn't mature yet and the spread fits the ceiling given.

use std::time::{Duration, Instant};

use breez_sdk_spark::{
    ClaimDeferredReason, ClaimDepositOutcome, ClaimDepositRequest, DepositClaimError, DepositInfo,
    Fee, FetchClaimDepositQuoteRequest, FetchClaimDepositQuoteResponse, InstantClaimStatus,
    ListUnclaimedDepositsRequest, MaxFee, RefundDepositRequest, RefundState, SdkError,
};
use serde::Serialize;
use sessions_core::{Error, Payment, Result, Sats};

use crate::{map, rail_err, SparkAccount};

/// Confirmations the Spark operators want before a deposit can be claimed
/// at its normal (on-chain fee) cost. Mirrors the SDK (mainnet; 1 on regtest).
pub const CONFIRMATIONS_NEEDED: u32 = 3;

/// Quotes are asked of the provider at most this often per deposit.
const QUOTE_TTL: Duration = Duration::from_secs(60);
/// A quote that takes longer than this is left out of the list.
const QUOTE_TIMEOUT: Duration = Duration::from_secs(10);

/// Where a deposit stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DepositStatus {
    /// Not enough confirmations yet; it's claimed on its own once it has
    /// them (or now, instantly, for `instant_fee_sat`).
    Waiting,
    /// Being claimed (or about to be, on the next sync).
    Claiming,
    /// Claiming costs more than the automatic limit: it needs the user to
    /// approve `claim_fee_sat`.
    NeedsApproval,
    /// The last claim failed (`error`). Retried on every sync.
    Failed,
    /// Being sent back on-chain (a refund).
    Refunding,
}

/// An on-chain deposit not yet in the balance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Deposit {
    pub txid: String,
    pub vout: u32,
    pub amount_sat: Sats,
    /// `None` when the chain couldn't be read just now.
    pub confirmations: Option<u32>,
    pub confirmations_needed: u32,
    pub status: DepositStatus,
    /// What claiming it costs now (network fee taken from the deposit),
    /// when known: the provider's quote, or what the last attempt asked.
    pub claim_fee_sat: Option<Sats>,
    /// What claiming it early costs (the provider's spread), when offered.
    pub instant_fee_sat: Option<Sats>,
    /// The depth an early claim is offered at (often 0 or 1).
    pub instant_confirmations: Option<u32>,
    /// Why it's stuck, in plain English.
    pub error: Option<String>,
    /// The refund transaction, while refunding.
    pub refund_txid: Option<String>,
}

/// What [`SparkAccount::claim_deposit`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ClaimStatus {
    /// Claimed: in the balance now (`payment`).
    Claimed,
    /// Claimed early; it reaches the balance in a moment.
    Submitted,
    /// Nothing claimed (see `message`; `required_fee_sat` when it cost more
    /// than allowed). The SDK keeps trying on its own.
    Deferred,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimResult {
    pub status: ClaimStatus,
    pub payment: Option<Payment>,
    pub message: Option<String>,
    /// What the claim would have cost, when that was over `max_fee_sat`.
    pub required_fee_sat: Option<Sats>,
}

/// A deposit's claim error in plain English.
pub fn claim_error_text(e: &DepositClaimError) -> String {
    match e {
        DepositClaimError::MaxDepositClaimFeeExceeded {
            required_fee_sats, ..
        } => format!(
            "Claiming it costs {required_fee_sats} sats in network fees right now, \
             more than the automatic limit. Approve the fee to claim it, or wait for fees to drop."
        ),
        DepositClaimError::MissingUtxo { .. } => "The deposit transaction can't be found on the \
             bitcoin network. It may have been replaced or dropped."
            .to_string(),
        DepositClaimError::Generic { message } => {
            let m = message.to_ascii_lowercase();
            if m.contains("chain service")
                || m.contains("timed out")
                || m.contains("connect")
                || m.contains("network")
            {
                "Couldn't reach the network to claim it. It's retried automatically.".to_string()
            } else {
                format!("Claiming failed ({message}). It's retried automatically.")
            }
        }
    }
}

/// Status, claim fee and error of a stored deposit, before any quote.
pub(crate) fn classify(
    d: &DepositInfo,
    confirmations: Option<u32>,
    needed: u32,
) -> (DepositStatus, Option<Sats>, Option<String>) {
    if d.refund_tx_id.is_some() {
        let error = match &d.refund_state {
            Some(RefundState::BroadcastPending {
                last_error: Some(e),
            }) => Some(format!("The refund hasn't reached the network yet ({e})")),
            _ => None,
        };
        return (DepositStatus::Refunding, None, error);
    }
    if matches!(
        d.instant_claim_status,
        Some(InstantClaimStatus::Submitted { .. } | InstantClaimStatus::Claimed)
    ) {
        return (DepositStatus::Claiming, None, None);
    }
    match &d.claim_error {
        Some(
            e @ DepositClaimError::MaxDepositClaimFeeExceeded {
                required_fee_sats, ..
            },
        ) => {
            return (
                DepositStatus::NeedsApproval,
                Some(*required_fee_sats),
                Some(claim_error_text(e)),
            )
        }
        Some(e) => return (DepositStatus::Failed, None, Some(claim_error_text(e))),
        None => {}
    }
    let mature = d.is_mature || confirmations.is_some_and(|c| c >= needed);
    if mature {
        (DepositStatus::Claiming, None, None)
    } else {
        (DepositStatus::Waiting, None, None)
    }
}

/// Fills in what a fresh quote says.
pub(crate) fn apply_quote(dep: &mut Deposit, q: &FetchClaimDepositQuoteResponse) {
    dep.confirmations = Some(q.confirmations);
    if let Some(i) = &q.instant {
        dep.instant_fee_sat = Some(i.fee_sats);
        dep.instant_confirmations = Some(i.confirmations_required);
    }
    // The provider's own price for the claim at maturity beats the one
    // recorded at the last attempt; an estimate doesn't.
    if dep.status == DepositStatus::NeedsApproval && !q.mature.is_estimate {
        dep.claim_fee_sat = Some(q.mature.fee_sats);
    }
}

/// The plain-English reason a claim was put off.
fn deferred_text(reason: &ClaimDeferredReason) -> (String, Option<Sats>) {
    match reason {
        ClaimDeferredReason::MaxFeeExceeded {
            required_fee_sats,
            max_fee_sats,
        } => (
            format!(
                "Claiming it now costs {required_fee_sats} sats, more than the {max_fee_sats} \
                 sats allowed."
            ),
            Some(*required_fee_sats),
        ),
        ClaimDeferredReason::NoEarlyClaimAvailable => (
            format!(
                "No early claim is offered for it yet. It's claimed automatically once it has \
                 {CONFIRMATIONS_NEEDED} confirmations."
            ),
            None,
        ),
        ClaimDeferredReason::ProviderDeclined { message } => (
            format!(
                "The early claim was declined ({message}). It's claimed automatically once it \
                 has {CONFIRMATIONS_NEEDED} confirmations."
            ),
            None,
        ),
    }
}

impl SparkAccount {
    fn confirmations_needed(&self) -> u32 {
        match self.network {
            breez_sdk_spark::Network::Regtest => 1,
            _ => CONFIRMATIONS_NEEDED,
        }
    }

    /// Confirmations of `txid` per the chain service, if it can be read.
    async fn confirmations(&self, txid: &str, tip: Option<u32>) -> Option<u32> {
        let chain = self.chain_service.as_ref()?;
        let st = chain.get_transaction_status(txid.to_string()).await.ok()?;
        if !st.confirmed {
            return Some(0);
        }
        let height = st.block_height?;
        Some(tip?.saturating_sub(height).saturating_add(1))
    }

    /// A claim quote, at most [`QUOTE_TTL`] old.
    async fn claim_quote(&self, txid: &str, vout: u32) -> Option<FetchClaimDepositQuoteResponse> {
        let key = (txid.to_string(), vout);
        if let Some((at, q)) = self
            .deposit_quotes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            if at.elapsed() < QUOTE_TTL {
                return Some(q.clone());
            }
        }
        let q = tokio::time::timeout(
            QUOTE_TIMEOUT,
            self.sdk
                .fetch_claim_deposit_quote(FetchClaimDepositQuoteRequest {
                    txid: txid.to_string(),
                    vout,
                }),
        )
        .await;
        match q {
            Ok(Ok(q)) => {
                let mut quotes = self
                    .deposit_quotes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                quotes.retain(|_, (at, _)| at.elapsed() < QUOTE_TTL);
                quotes.insert(key, (Instant::now(), q.clone()));
                Some(q)
            }
            Ok(Err(e)) => {
                tracing::info!("no claim quote for {txid}:{vout}: {e}");
                None
            }
            Err(_) => {
                tracing::info!("claim quote for {txid}:{vout} timed out");
                None
            }
        }
    }

    /// What's sitting at the deposit address right now, unconfirmed
    /// included: `("txid:vout", amount, confirmed)`. Empty when the
    /// chain isn't read through our own server. For noticing a payment the
    /// moment it's broadcast (the SDK only lists deposits it found).
    pub async fn deposit_outputs(&self) -> Result<Vec<(String, Sats, bool)>> {
        let Some(chain) = self.chain_service.as_ref() else {
            return Ok(vec![]);
        };
        let address = self.deposit_address().await?;
        let utxos = chain
            .get_address_utxos(address)
            .await
            .map_err(|e| Error::rail("spark", e))?;
        Ok(utxos
            .into_iter()
            .map(|u| {
                (
                    format!("{}:{}", u.txid, u.vout),
                    u.value,
                    u.status.confirmed,
                )
            })
            .collect())
    }

    /// On-chain deposits not yet in the balance: waiting for confirmations,
    /// being claimed, or stuck (see [`DepositStatus`]).
    pub async fn deposits(&self) -> Result<Vec<Deposit>> {
        let stored = self
            .sdk
            .list_unclaimed_deposits(ListUnclaimedDepositsRequest {})
            .await
            .map_err(rail_err)?
            .deposits;
        if stored.is_empty() {
            return Ok(vec![]);
        }
        let needed = self.confirmations_needed();
        let tip = match &self.chain_service {
            Some(c) => c.tip_height().await.ok(),
            None => None,
        };
        let mut out = Vec::with_capacity(stored.len());
        for d in &stored {
            let confirmations = self.confirmations(&d.txid, tip).await;
            let (status, claim_fee_sat, error) = classify(d, confirmations, needed);
            let mut dep = Deposit {
                txid: d.txid.clone(),
                vout: d.vout,
                amount_sat: d.amount_sats,
                confirmations,
                confirmations_needed: needed,
                status,
                claim_fee_sat,
                instant_fee_sat: None,
                instant_confirmations: None,
                error,
                refund_txid: d.refund_tx_id.clone(),
            };
            // Prices only matter where the user has a choice to make.
            if matches!(
                status,
                DepositStatus::Waiting | DepositStatus::NeedsApproval
            ) {
                if let Some(q) = self.claim_quote(&d.txid, d.vout).await {
                    apply_quote(&mut dep, &q);
                }
            }
            out.push(dep);
        }
        Ok(out)
    }

    /// Claim a deposit, paying up to `max_fee_sat` (taken from the deposit).
    /// `instant`: claim it now even before it has
    /// [`CONFIRMATIONS_NEEDED`] confirmations, for the provider's spread.
    /// Without it, an immature deposit is refused (it claims itself later,
    /// for less). The fee cap stays on the deposit for later automatic
    /// attempts.
    pub async fn claim_deposit(
        &self,
        txid: &str,
        vout: u32,
        max_fee_sat: Sats,
        instant: bool,
    ) -> Result<ClaimResult> {
        let txid = txid.trim().to_string();
        if !instant {
            let stored = self
                .sdk
                .list_unclaimed_deposits(ListUnclaimedDepositsRequest {})
                .await
                .map_err(rail_err)?
                .deposits
                .into_iter()
                .find(|d| d.txid.eq_ignore_ascii_case(&txid) && d.vout == vout);
            let tip = match &self.chain_service {
                Some(c) => c.tip_height().await.ok(),
                None => None,
            };
            let confirmations = self.confirmations(&txid, tip).await;
            let needed = self.confirmations_needed();
            let mature = stored.as_ref().is_some_and(|d| d.is_mature)
                || confirmations.is_some_and(|c| c >= needed);
            // Unreadable depth: the SDK decides (it treats that as mature).
            if !mature && confirmations.is_some() {
                return Err(Error::Invalid(format!(
                    "This deposit has {} of the {needed} confirmations it needs. Wait for them, \
                     or claim it instantly",
                    confirmations.unwrap_or(0)
                )));
            }
        }
        let resp = self
            .sdk
            .claim_deposit(ClaimDepositRequest {
                txid: txid.clone(),
                vout,
                max_fee: Some(MaxFee::Fixed {
                    amount: max_fee_sat,
                }),
            })
            .await;
        self.deposit_quotes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(txid.clone(), vout));
        match resp {
            Ok(r) => Ok(match r.outcome {
                ClaimDepositOutcome::Settled { payment } => ClaimResult {
                    status: ClaimStatus::Claimed,
                    payment: Some(map::payment(&self.id, &payment)?),
                    message: None,
                    required_fee_sat: None,
                },
                ClaimDepositOutcome::Submitted => ClaimResult {
                    status: ClaimStatus::Submitted,
                    payment: None,
                    message: Some("Claimed early. It reaches your balance in a moment.".into()),
                    required_fee_sat: None,
                },
                ClaimDepositOutcome::Deferred { reason } => {
                    let (message, required_fee_sat) = deferred_text(&reason);
                    ClaimResult {
                        status: ClaimStatus::Deferred,
                        payment: None,
                        message: Some(message),
                        required_fee_sat,
                    }
                }
            }),
            Err(SdkError::MaxDepositClaimFeeExceeded {
                required_fee_sats, ..
            }) => Ok(ClaimResult {
                status: ClaimStatus::Deferred,
                payment: None,
                message: Some(format!(
                    "Claiming it costs {required_fee_sats} sats right now, more than the \
                     {max_fee_sat} sats allowed."
                )),
                required_fee_sat: Some(required_fee_sats),
            }),
            Err(SdkError::DepositClaimInProgress { .. }) => Ok(ClaimResult {
                status: ClaimStatus::Submitted,
                payment: None,
                message: Some("It's already being claimed.".into()),
                required_fee_sat: None,
            }),
            Err(SdkError::MissingUtxo { .. }) => Err(Error::Invalid(
                "That deposit can't be found on the bitcoin network".into(),
            )),
            Err(e) => Err(rail_err(e)),
        }
    }

    /// Send a deposit back on-chain to `address` instead of claiming it (for
    /// one too small to be worth claiming), at `fee_rate_sat_vb`. Only for
    /// deposits with enough confirmations. Returns the refund's txid.
    pub async fn refund_deposit(
        &self,
        txid: &str,
        vout: u32,
        address: &str,
        fee_rate_sat_vb: u64,
    ) -> Result<String> {
        if fee_rate_sat_vb == 0 {
            return Err(Error::Invalid(
                "The fee rate must be at least 1 sat/vB".into(),
            ));
        }
        let r = self
            .sdk
            .refund_deposit(RefundDepositRequest {
                txid: txid.trim().to_string(),
                vout,
                destination_address: address
                    .trim()
                    .trim_start_matches("bitcoin:")
                    .split('?')
                    .next()
                    .unwrap_or("")
                    .to_string(),
                fee: Fee::Rate {
                    sat_per_vbyte: fee_rate_sat_vb,
                },
            })
            .await
            .map_err(rail_err)?;
        Ok(r.tx_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use breez_sdk_spark::{ClaimDepositQuote, FetchClaimDepositQuoteResponse};

    fn info() -> DepositInfo {
        DepositInfo {
            txid: "aa".repeat(32),
            vout: 1,
            amount_sats: 50_000,
            is_mature: false,
            refund_tx: None,
            refund_tx_id: None,
            refund_state: None,
            claim_error: None,
            instant_claim_status: None,
            max_claim_fee: None,
        }
    }

    #[test]
    fn classifies_deposits() {
        let d = info();
        assert_eq!(classify(&d, Some(0), 3).0, DepositStatus::Waiting);
        assert_eq!(classify(&d, Some(2), 3).0, DepositStatus::Waiting);
        assert_eq!(classify(&d, None, 3).0, DepositStatus::Waiting);
        // Deep enough on chain, or the operators say so: claimed next sync.
        assert_eq!(classify(&d, Some(3), 3).0, DepositStatus::Claiming);
        let mature = DepositInfo {
            is_mature: true,
            ..info()
        };
        assert_eq!(classify(&mature, None, 3).0, DepositStatus::Claiming);

        // Over the automatic fee limit: needs approval, with the fee.
        let over = DepositInfo {
            is_mature: true,
            claim_error: Some(DepositClaimError::MaxDepositClaimFeeExceeded {
                tx: "aa".repeat(32),
                vout: 1,
                max_fee: Some(Fee::Rate { sat_per_vbyte: 3 }),
                required_fee_sats: 1_200,
                required_fee_rate_sat_per_vbyte: 13,
            }),
            ..info()
        };
        let (s, fee, err) = classify(&over, Some(5), 3);
        assert_eq!(s, DepositStatus::NeedsApproval);
        assert_eq!(fee, Some(1_200));
        assert!(err.unwrap().contains("1200 sats"));

        // Any other claim error: failed, explained.
        let failed = DepositInfo {
            claim_error: Some(DepositClaimError::Generic {
                message: "Chain service error: keep-alive timed out".into(),
            }),
            ..info()
        };
        let (s, _, err) = classify(&failed, Some(4), 3);
        assert_eq!(s, DepositStatus::Failed);
        assert!(err.unwrap().contains("Couldn't reach the network"));
        let missing = DepositInfo {
            claim_error: Some(DepositClaimError::MissingUtxo {
                tx: "aa".repeat(32),
                vout: 1,
            }),
            ..info()
        };
        assert!(classify(&missing, None, 3)
            .2
            .unwrap()
            .contains("can't be found"));

        // An early claim in flight, even with an old error recorded.
        let early = DepositInfo {
            instant_claim_status: Some(InstantClaimStatus::Submitted {
                claim_id: "c".into(),
            }),
            ..failed.clone()
        };
        assert_eq!(classify(&early, Some(1), 3).0, DepositStatus::Claiming);
        // A declined early claim just waits for maturity.
        let declined = DepositInfo {
            instant_claim_status: Some(InstantClaimStatus::Declined {
                max_fee_sats: Some(300),
                confirmations: 1,
            }),
            ..info()
        };
        assert_eq!(classify(&declined, Some(1), 3).0, DepositStatus::Waiting);

        // Refunding wins over everything.
        let refunding = DepositInfo {
            refund_tx_id: Some("bb".repeat(32)),
            refund_state: Some(RefundState::BroadcastPending {
                last_error: Some("min relay fee not met".into()),
            }),
            ..over.clone()
        };
        let (s, fee, err) = classify(&refunding, Some(9), 3);
        assert_eq!(s, DepositStatus::Refunding);
        assert_eq!(fee, None);
        assert!(err.unwrap().contains("min relay fee"));
    }

    fn quote(conf: u32, fee: u64, estimate: bool) -> ClaimDepositQuote {
        ClaimDepositQuote {
            confirmations_required: conf,
            credit_amount_sats: 50_000 - fee,
            fee_sats: fee,
            fee_rate_sat_per_vbyte: fee.div_ceil(99),
            is_estimate: estimate,
        }
    }

    #[test]
    fn quotes_fill_in_fees_and_depth() {
        let mut dep = Deposit {
            txid: "aa".repeat(32),
            vout: 1,
            amount_sat: 50_000,
            confirmations: None,
            confirmations_needed: 3,
            status: DepositStatus::Waiting,
            claim_fee_sat: None,
            instant_fee_sat: None,
            instant_confirmations: None,
            error: None,
            refund_txid: None,
        };
        let q = FetchClaimDepositQuoteResponse {
            amount_sats: 50_000,
            confirmations: 1,
            instant: Some(quote(1, 750, false)),
            mature: quote(3, 300, true),
        };
        apply_quote(&mut dep, &q);
        assert_eq!(dep.confirmations, Some(1));
        assert_eq!(dep.instant_fee_sat, Some(750));
        assert_eq!(dep.instant_confirmations, Some(1));
        // Waiting: no claim fee yet (the mature price is an estimate anyway).
        assert_eq!(dep.claim_fee_sat, None);

        let mut stuck = Deposit {
            status: DepositStatus::NeedsApproval,
            claim_fee_sat: Some(1_200),
            ..dep.clone()
        };
        let q = FetchClaimDepositQuoteResponse {
            amount_sats: 50_000,
            confirmations: 6,
            instant: None,
            mature: quote(3, 900, false),
        };
        apply_quote(&mut stuck, &q);
        assert_eq!(stuck.claim_fee_sat, Some(900));
        assert_eq!(stuck.confirmations, Some(6));

        let json = serde_json::to_value(&stuck).unwrap();
        assert_eq!(json["status"], "needsApproval");
        assert_eq!(json["claimFeeSat"], 900);
        assert_eq!(json["amountSat"], 50_000);
        assert_eq!(json["confirmationsNeeded"], 3);
    }

    #[test]
    fn deferred_reasons_read_plainly() {
        let (m, fee) = deferred_text(&ClaimDeferredReason::MaxFeeExceeded {
            required_fee_sats: 800,
            max_fee_sats: 500,
        });
        assert!(m.contains("800 sats") && m.contains("500"));
        assert_eq!(fee, Some(800));
        let (m, fee) = deferred_text(&ClaimDeferredReason::NoEarlyClaimAvailable);
        assert!(m.contains("3 confirmations"));
        assert_eq!(fee, None);
    }
}

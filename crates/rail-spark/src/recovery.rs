//! Spending's health: when it last synced, and funds that left the balance
//! for the chain ("recoverable funds", SDK 0.28).
//!
//! Funds can leave a Spark balance on-chain (the operators or a watchtower
//! moved a leaf on-chain). Those come back with a *cooperative recovery*:
//! one transaction per leaf, co-signed by the operators, that pays its own
//! fee from the leaf and needs nothing else. That's what this module does,
//! to an address of the person's (Savings).
//!
//! Leaves that need a *unilateral exit* (pre-signed transactions fee-bumped
//! from a UTXO of ours, over days of timelocks) aren't handled here yet: the
//! quote says so, and they stay recoverable.

use std::time::{Duration, Instant};

use breez_sdk_spark::{
    ExitLeafSelection, ExitTransactionStatus, GetInfoRequest, PrepareRecoverFundsRequest,
    PrepareRecoverFundsResponse, RecoverFundsRequest, RecoveryMethod, RecoveryTxKind, SyncError,
};
use serde::Serialize;
use sessions_core::{Error, Result, Sats};

use crate::{rail_err, SparkAccount};

/// Spending's sync health and funds waiting to be recovered.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpendingHealth {
    /// Unix seconds of the last sync that refreshed the balance.
    pub last_synced_at: Option<u64>,
    /// Why the latest sync failed (the balance may be out of date).
    pub sync_error: Option<String>,
    /// Funds that left the balance and wait to be recovered on-chain.
    pub recoverable_sat: Sats,
}

/// A recovery, priced.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryQuote {
    /// Value of the leaves this recovers.
    pub value_sat: Sats,
    /// What the recovery transactions pay in fees (out of the value).
    pub fee_sat: Sats,
    /// What arrives at the address.
    pub arrives_sat: Sats,
    /// Recoverable funds this can't bring back now: too small for the fee,
    /// not found yet, or needing an on-chain exit.
    pub left_out_sat: Sats,
    /// Some funds need an on-chain exit (not supported here yet).
    pub needs_exit: bool,
}

/// A prepared recovery stays usable this long.
const QUOTE_TTL: Duration = Duration::from_secs(300);

pub(crate) struct PreparedRecovery {
    at: Instant,
    response: PrepareRecoverFundsResponse,
}

impl SparkAccount {
    /// Last sync, its error, and recoverable funds (no network).
    pub async fn health(&self) -> Result<SpendingHealth> {
        let info = self
            .sdk
            .get_info(GetInfoRequest {
                ensure_synced: Some(false),
            })
            .await
            .map_err(rail_err)?;
        Ok(SpendingHealth {
            last_synced_at: info.last_synced_at,
            sync_error: info.last_sync_error.map(|e| match e {
                SyncError::Network { .. } => "Couldn't reach the Spark network".to_string(),
                SyncError::Generic { message } => {
                    tracing::warn!("spark: sync failed: {message}");
                    "Spending couldn't sync".to_string()
                }
            }),
            recoverable_sat: info.recoverable_funds_sats,
        })
    }

    /// Price recovering the funds that left Spending to `destination` (a
    /// bitcoin address) at `fee_rate` sat/vB. Only cooperative recoveries are
    /// included; anything needing an on-chain exit is reported, not quoted.
    pub async fn prepare_recovery(&self, destination: &str, fee_rate: u64) -> Result<RecoveryQuote> {
        let response = self
            .sdk
            .prepare_recover_funds(PrepareRecoverFundsRequest {
                fee_rate_sat_per_vbyte: fee_rate.max(1),
                funding_kind: None,
                funding_strategy: None,
                destination: destination.to_string(),
                selection: ExitLeafSelection::RecoverableOnly,
            })
            .await
            .map_err(rail_err)?;
        let quote = summarize(&response);
        *self.recovery.lock().unwrap_or_else(|e| e.into_inner()) = Some(PreparedRecovery {
            at: Instant::now(),
            response,
        });
        Ok(quote)
    }

    /// Recover what [`prepare_recovery`](Self::prepare_recovery) priced:
    /// the operators co-sign, and the transactions are broadcast. Returns
    /// their txids.
    pub async fn recover(&self) -> Result<Vec<String>> {
        let prepared = self
            .recovery
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .filter(|p| p.at.elapsed() < QUOTE_TTL)
            .ok_or_else(|| Error::Invalid("That quote expired. Check the recovery again".into()))?;
        let mut response = prepared.response;
        if response.funding.is_some() {
            // Keep only what needs no funding.
            response.leaves.retain(|l| l.method == RecoveryMethod::Cooperative);
            response.funding = None;
        }
        if response.leaves.is_empty() {
            return Err(Error::Invalid("There's nothing to recover right now".into()));
        }
        let built = self
            .sdk
            .recover_funds(
                RecoverFundsRequest {
                    prepared: response,
                    funding_inputs: Vec::new(),
                },
                None,
            )
            .await
            .map_err(rail_err)?;
        let chain = self.chain_service.clone().ok_or_else(|| {
            Error::Unsupported("broadcast recoveries with this chain backend".into())
        })?;
        let mut sent = Vec::new();
        for tx in built.transactions {
            if tx.kind != RecoveryTxKind::Cooperative || !matches!(tx.status, ExitTransactionStatus::Ready) {
                continue;
            }
            match chain.broadcast_transaction(tx.tx_hex.clone()).await {
                Ok(()) => sent.push(tx.txid),
                Err(e) => {
                    let m = e.to_string();
                    // Already in the mempool or a block counts as sent.
                    if m.to_ascii_lowercase().contains("already") {
                        sent.push(tx.txid);
                    } else {
                        tracing::warn!("spark: recovery broadcast failed: {m}");
                        if sent.is_empty() {
                            return Err(Error::Network(format!("Couldn't broadcast the recovery: {m}")));
                        }
                    }
                }
            }
        }
        for f in &built.failed {
            tracing::warn!("spark: cooperative recovery of {} failed: {:?}", f.leaf_id, f.error);
        }
        if sent.is_empty() {
            return Err(Error::Invalid(
                "The Spark operators didn't co-sign the recovery. Try again later".into(),
            ));
        }
        Ok(sent)
    }
}

fn summarize(r: &PrepareRecoverFundsResponse) -> RecoveryQuote {
    let cooperative: Sats = r
        .leaves
        .iter()
        .filter(|l| l.method == RecoveryMethod::Cooperative)
        .map(|l| l.value_sats)
        .sum();
    let exit: Sats = r
        .leaves
        .iter()
        .filter(|l| l.method == RecoveryMethod::Unilateral)
        .map(|l| l.value_sats)
        .sum();
    let skipped: Sats = r.skipped.iter().map(|s| s.value_sats).sum();
    let fee = r.cooperative_fee_sats;
    RecoveryQuote {
        value_sat: cooperative,
        fee_sat: fee,
        arrives_sat: cooperative.saturating_sub(fee),
        left_out_sat: skipped + exit,
        needs_exit: exit > 0,
    }
}

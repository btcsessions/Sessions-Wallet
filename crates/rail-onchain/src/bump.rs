//! Speeding up and cancelling unconfirmed transactions.
//!
//! * **RBF speed-up**: the same payment again with a higher fee, paid out of
//!   its change (BDK's `build_fee_bump`).
//! * **RBF cancel**: the same inputs, all paid back to a fresh change address
//!   of this account. If it confirms first, the original payment never happens.
//! * **CPFP**: for payments we can't replace (usually incoming ones), a child
//!   spending our output of it pays enough that miners take both together.
//!
//! Replacements follow BIP125: a higher fee rate than the original, and an
//! absolute fee of at least the original's plus 1 sat/vB of the new size.

use std::str::FromStr;
use std::sync::Arc;

use bdk_wallet::bitcoin::{Amount, FeeRate, OutPoint, Psbt, ScriptBuf, Transaction, Txid, Weight};
use bdk_wallet::chain::ChainPosition;
use bdk_wallet::error::CreateTxError;
use bdk_wallet::{KeychainKind, Wallet};

use sessions_core::types::*;
use sessions_core::{Error, Result};

use crate::{err, Inner, OnchainAccount};

/// BIP125 incremental relay fee: 1 sat/vB.
const INCREMENTAL_KWU: u64 = 250;

/// What the wallet knows about one transaction.
struct Analysis {
    tx: Arc<Transaction>,
    /// (height, time) once confirmed.
    confirmed: Option<(u32, u64)>,
    fee: Option<Amount>,
    /// Every input spends one of our coins.
    all_inputs_ours: bool,
    /// Signals replaceability (BIP125: some input has nSequence ≤ 0xFFFFFFFD).
    rbf: bool,
    /// Another unconfirmed transaction already spends one of its outputs.
    has_children: bool,
    /// Our unspent outputs of it.
    ours: Vec<OutPoint>,
}

impl Analysis {
    fn can_replace(&self, can_spend: bool) -> bool {
        can_spend
            && self.confirmed.is_none()
            && self.all_inputs_ours
            && self.rbf
            && !self.has_children
            && self.fee.is_some()
    }

    fn can_cpfp(&self, can_spend: bool) -> bool {
        can_spend && self.confirmed.is_none() && !self.ours.is_empty()
    }
}

fn parse_txid(txid: &str) -> Result<Txid> {
    Txid::from_str(txid.trim()).map_err(|_| Error::Invalid("That isn't a transaction id".into()))
}

fn not_found() -> Error {
    Error::Invalid("This account doesn't have that transaction".into())
}

fn analyse(wallet: &Wallet, txid: Txid) -> Result<Analysis> {
    let wtx = wallet.get_tx(txid).ok_or_else(not_found)?;
    let tx = wtx.tx_node.tx.clone();
    let confirmed = match wtx.chain_position {
        ChainPosition::Confirmed { anchor, .. } => {
            Some((anchor.block_id.height, anchor.confirmation_time))
        }
        ChainPosition::Unconfirmed { .. } => None,
    };
    let all_inputs_ours = tx.input.iter().all(|i| {
        wallet
            .tx_graph()
            .get_txout(i.previous_output)
            .is_some_and(|o| wallet.is_mine(o.script_pubkey.clone()))
    });
    let has_children = wallet.transactions().any(|t| {
        t.tx_node.txid != txid
            && t.tx_node
                .tx
                .input
                .iter()
                .any(|i| i.previous_output.txid == txid)
    });
    let ours = wallet
        .list_unspent()
        .filter(|u| u.outpoint.txid == txid)
        .map(|u| u.outpoint)
        .collect();
    Ok(Analysis {
        fee: wallet.calculate_fee(&tx).ok(),
        rbf: tx.input.iter().any(|i| i.sequence.is_rbf()),
        tx,
        confirmed,
        all_inputs_ours,
        has_children,
        ours,
    })
}

fn vbytes(w: Weight) -> u64 {
    w.to_wu().div_ceil(4)
}

/// sat/vB, for display.
fn sat_vb(fee: Amount, weight: Weight) -> f32 {
    if weight.to_wu() == 0 {
        return 0.0;
    }
    (fee.to_sat() as f64 * 4.0 / weight.to_wu() as f64) as f32
}

/// Fee for `vsize` vbytes at `rate`, rounded up.
fn fee_at(rate: FeeRate, vsize: u64) -> u64 {
    (rate.to_sat_per_kwu() * 4 * vsize).div_ceil(1000)
}

/// Upper bound on the signed weight of an unsigned PSBT whose inputs are all
/// ours: each input's worst-case witness, plus the segwit marker.
fn signed_weight(wallet: &Wallet, psbt: &Psbt) -> Weight {
    let mut wu = psbt.unsigned_tx.weight().to_wu() + 2;
    for txin in &psbt.unsigned_tx.input {
        let keychain = wallet
            .tx_graph()
            .get_txout(txin.previous_output)
            .and_then(|o| wallet.derivation_of_spk(o.script_pubkey.clone()))
            .map(|(k, _)| k)
            .unwrap_or(KeychainKind::External);
        let sat = wallet
            .public_descriptor(keychain)
            .max_weight_to_satisfy()
            .map(|w| w.to_wu())
            .unwrap_or(4 * 300);
        // +1: the witness item count, in case it isn't included.
        wu += sat + 1;
    }
    Weight::from_wu(wu)
}

/// The change output to take a speed-up's extra fee from: one of our change
/// outputs, preferring one whose amount no other output shares (so a private
/// send's decoy keeps matching the payment), and then the largest.
fn pick_change(wallet: &Wallet, tx: &Transaction) -> Option<usize> {
    let internal: Vec<usize> = tx
        .output
        .iter()
        .enumerate()
        .filter(|(_, o)| {
            matches!(
                wallet.derivation_of_spk(o.script_pubkey.clone()),
                Some((KeychainKind::Internal, _))
            )
        })
        .map(|(i, _)| i)
        .collect();
    let unique: Vec<usize> = internal
        .iter()
        .copied()
        .filter(|&i| {
            !tx.output
                .iter()
                .enumerate()
                .any(|(j, o)| j != i && o.value == tx.output[i].value)
        })
        .collect();
    let pool = if unique.is_empty() { internal } else { unique };
    pool.into_iter().max_by_key(|&i| tx.output[i].value)
}

fn bump_err(e: CreateTxError) -> Error {
    match e {
        CreateTxError::CoinSelection(_) => Error::Invalid(
            "There isn't enough spare change in this transaction to pay a higher fee, and no other confirmed coins to add".into(),
        ),
        other => err(other),
    }
}

impl OnchainAccount {
    fn can_spend(&self) -> bool {
        !matches!(self.custody, Custody::WatchOnly | Custody::Mint { .. })
    }

    pub(crate) async fn details(&self, txid: &str) -> Result<TxDetails> {
        let txid = parse_txid(txid)?;
        let inner = self.inner.lock().await;
        let wallet = &inner.wallet;
        let a = analyse(wallet, txid)?;
        let tip = wallet.latest_checkpoint().height();
        let weight = a.tx.weight();
        let spend = self.can_spend();
        let rbf = a.can_replace(spend);
        let cpfp = a.can_cpfp(spend);
        Ok(TxDetails {
            txid: txid.to_string(),
            confirmations: a
                .confirmed
                .map(|(h, _)| tip.saturating_sub(h).saturating_add(1))
                .unwrap_or(0),
            block_height: a.confirmed.map(|(h, _)| h),
            block_time: a.confirmed.map(|(_, t)| t),
            fee_sat: a.fee.map(|f| f.to_sat()),
            fee_rate: a.fee.map(|f| sat_vb(f, weight)),
            vsize: vbytes(weight),
            can_speed_up: rbf || cpfp,
            can_cancel: rbf,
            speed_up_kind: if rbf {
                Some(SpeedUpKind::Rbf)
            } else if cpfp {
                Some(SpeedUpKind::Cpfp)
            } else {
                None
            },
        })
    }

    pub(crate) async fn bump(
        &self,
        txid: &str,
        action: BumpAction,
        fee: FeePriority,
    ) -> Result<SendQuote> {
        if !self.can_spend() {
            return Err(Error::Unsupported("spend from a watch-only account".into()));
        }
        let txid = parse_txid(txid)?;
        let target = self.fee_rate(fee).await?;

        let mut inner = self.inner.lock().await;
        let Inner {
            wallet,
            conn,
            labels,
            pending,
            ..
        } = &mut *inner;
        let a = analyse(wallet, txid)?;
        let frozen: Vec<OutPoint> = labels.frozen().iter().copied().collect();
        if a.confirmed.is_some() {
            return Err(Error::Invalid(
                "This transaction is already confirmed".into(),
            ));
        }
        let (psbt, notes) = match action {
            BumpAction::SpeedUp if a.can_replace(true) => {
                rbf_speed_up(wallet, txid, &a, target, frozen)?
            }
            BumpAction::SpeedUp if a.can_cpfp(true) => cpfp(wallet, &a, target)?,
            BumpAction::SpeedUp => {
                return Err(Error::Invalid(
                    "This transaction can't be sped up from this account".into(),
                ))
            }
            BumpAction::Cancel if a.can_replace(true) => rbf_cancel(wallet, txid, &a, target)?,
            BumpAction::Cancel => {
                return Err(Error::Invalid(
                    "Only unconfirmed payments sent from this account, that allow replacement, can be cancelled".into(),
                ))
            }
        };
        // A replacement (not a CPFP child) keeps the original's label.
        let replaces = action == BumpAction::Cancel || a.can_replace(true);
        if let Some(l) = labels.tx(&txid).filter(|_| replaces) {
            pending.remember(psbt.unsigned_tx.compute_txid(), l.to_string());
        }
        // Keep the change address we may have used from being handed out again.
        wallet
            .persist(conn)
            .map_err(|e| Error::Storage(e.to_string()))?;
        drop(inner);
        self.quote(psbt, 0, notes, None)
    }
}

/// Minimum replacement rate: the original's plus the incremental relay fee.
fn replacement_floor(a: &Analysis) -> FeeRate {
    let old = a.fee.unwrap_or(Amount::ZERO) / a.tx.weight();
    FeeRate::from_sat_per_kwu(old.to_sat_per_kwu() + INCREMENTAL_KWU)
}

fn rbf_speed_up(
    wallet: &mut Wallet,
    txid: Txid,
    a: &Analysis,
    target: FeeRate,
    frozen: Vec<OutPoint>,
) -> Result<(Psbt, Vec<String>)> {
    let old_fee = a.fee.unwrap_or(Amount::ZERO).to_sat();
    let floor = replacement_floor(a);
    let mut notes = vec![
        "This replaces your earlier transaction with the same payment and a higher fee."
            .to_string(),
    ];
    let rate = target.max(floor);
    if target < floor {
        notes.push(format!(
            "The speed you picked is below what a replacement must pay, so it uses {:.1} sat/vB.",
            rate.to_sat_per_kwu() as f64 / 250.0
        ));
    }
    let change = pick_change(wallet, &a.tx);
    // First at the rate; then, if BDK's size estimate came out short, at the
    // absolute fee our worst-case size needs.
    let mut fee: Option<u64> = None;
    for _ in 0..4 {
        let mut b = wallet.build_fee_bump(txid).map_err(err)?;
        // Extra inputs, if needed, never come from frozen coins.
        if !frozen.is_empty() {
            b.unspendable(frozen.clone());
        }
        if let Some(i) = change {
            // Keep every other output exactly; the fee comes out of this one.
            b.set_recipients(
                a.tx.output
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, o)| (o.script_pubkey.clone(), o.value))
                    .collect(),
            );
            b.drain_to(a.tx.output[i].script_pubkey.clone());
        }
        match fee {
            None => b.fee_rate(rate),
            Some(f) => b.fee_absolute(Amount::from_sat(f)),
        };
        let psbt = b.finish().map_err(bump_err)?;
        let new_fee = psbt.fee().map_err(err)?.to_sat();
        let vsize = vbytes(signed_weight(wallet, &psbt));
        // BIP125 rule 4: pay for the replacement's own relay too. And reach
        // the chosen rate on the worst-case signed size.
        let needed = (old_fee + vsize).max(fee_at(rate, vsize));
        if new_fee >= needed {
            return Ok((psbt, notes));
        }
        fee = Some(needed);
    }
    Err(err("couldn't build a replacement with a high enough fee"))
}

fn rbf_cancel(
    wallet: &mut Wallet,
    txid: Txid,
    a: &Analysis,
    target: FeeRate,
) -> Result<(Psbt, Vec<String>)> {
    let old_fee = a.fee.unwrap_or(Amount::ZERO);
    let floor = replacement_floor(a);
    let dest = wallet
        .reveal_next_address(KeychainKind::Internal)
        .script_pubkey();
    let build = |wallet: &mut Wallet, fee: Amount| -> Result<Psbt> {
        let mut b = wallet.build_fee_bump(txid).map_err(err)?;
        b.set_recipients(vec![]);
        b.drain_to(dest.clone());
        b.manually_selected_only();
        b.fee_absolute(fee);
        b.finish().map_err(|e| match e {
            CreateTxError::CoinSelection(_) => {
                Error::Invalid("This payment is too small to cancel at that fee".into())
            }
            other => err(other),
        })
    };
    // Shape first (inputs and one output), then the real fee.
    let shape = build(wallet, old_fee)?;
    let vsize = vbytes(signed_weight(wallet, &shape));
    let fee = fee_at(target, vsize)
        .max(fee_at(floor, vsize))
        .max(old_fee.to_sat() + vsize);
    let psbt = build(wallet, Amount::from_sat(fee))?;
    let back: u64 = psbt
        .unsigned_tx
        .output
        .iter()
        .map(|o| o.value.to_sat())
        .sum();
    Ok((
        psbt,
        vec![
            "This replaces your earlier transaction.".into(),
            format!("If it confirms first, the payment is cancelled and {back} sats come back to this account."),
            "If the original confirms first, the payment goes through and this does nothing.".into(),
        ],
    ))
}

fn cpfp(wallet: &mut Wallet, a: &Analysis, target: FeeRate) -> Result<(Psbt, Vec<String>)> {
    let parent_weight = a.tx.weight();
    let mut notes = vec![
        "This adds a small transaction of your own that pays extra, so miners confirm both together.".to_string(),
    ];
    let parent_fee = match a.fee {
        Some(f) => {
            if f / parent_weight >= target {
                return Err(Error::Invalid(format!(
                    "This transaction already pays {:.1} sat/vB, at or above the speed you picked",
                    sat_vb(f, parent_weight)
                )));
            }
            f.to_sat()
        }
        None => {
            notes.push("The sender's fee isn't known, so this pays as if they paid nothing. It may cost more than needed.".into());
            0
        }
    };
    let total_in: u64 = a
        .ours
        .iter()
        .filter_map(|op| wallet.get_utxo(*op))
        .map(|u| u.txout.value.to_sat())
        .sum();
    let dest = wallet
        .reveal_next_address(KeychainKind::Internal)
        .script_pubkey();
    let too_small = |fee: u64| {
        Error::Invalid(format!(
            "The {total_in} sats from this transaction aren't enough to pay the {fee} sat fee needed to speed it up"
        ))
    };
    let build = |wallet: &mut Wallet, fee: u64| -> Result<Psbt> {
        let mut b = wallet.build_tx();
        b.add_utxos(&a.ours).map_err(err)?;
        b.manually_selected_only();
        b.drain_to(dest.clone());
        b.fee_absolute(Amount::from_sat(fee));
        b.finish().map_err(|e| match e {
            CreateTxError::CoinSelection(_) => too_small(fee),
            other => err(other),
        })
    };
    let shape = build(wallet, 0)?;
    let child_vsize = vbytes(signed_weight(wallet, &shape));
    let package = fee_at(target, vbytes(parent_weight) + child_vsize);
    // The child also pays at least the minimum relay rate on its own.
    let fee = package.saturating_sub(parent_fee).max(child_vsize);
    if total_in < fee + dust(&dest) {
        return Err(too_small(fee));
    }
    Ok((build(wallet, fee)?, notes))
}

fn dust(spk: &ScriptBuf) -> u64 {
    spk.minimal_non_dust().to_sat()
}

/// How many inputs the two transactions both spend.
#[cfg(test)]
pub(crate) fn shared_inputs(a: &Transaction, b: &Transaction) -> usize {
    let ins: std::collections::HashSet<_> = a.input.iter().map(|i| i.previous_output).collect();
    b.input
        .iter()
        .filter(|i| ins.contains(&i.previous_output))
        .count()
}

//! "Private" sends: a single-wallet fake coinjoin in the STONEWALL pattern.
//!
//! The transaction looks like two people each paying in and each getting an
//! output of the same amount back:
//!
//! ```text
//! inputs  (group A)  ─┐            ┌─ payment       X   (recipient)
//!                     ├─ tx ───────┼─ decoy         X   (ours)
//! inputs  (group B)  ─┘            ├─ change A          (ours)
//!                                  └─ change B          (ours)
//! ```
//!
//! An observer can't tell which of the two equal outputs is the payment, or
//! which inputs "belong" with which change. Coins that share an address always
//! stay in the same group, so address reuse can't link the two sides.

use bdk_wallet::bitcoin::{FeeRate, OutPoint, ScriptBuf, Weight};
use bdk_wallet::{KeychainKind, LocalOutput, Wallet};
use sessions_core::{Error, Result};

/// Outputs below this are dropped into the fee rather than created.
const DUST: u64 = 546;

/// A planned private send, ready for the TxBuilder.
#[derive(Debug, Clone)]
pub struct PrivatePlan {
    pub inputs: Vec<OutPoint>,
    /// (script, amount) in no particular order; BDK shuffles them.
    pub outputs: Vec<(ScriptBuf, u64)>,
    pub fee: u64,
    /// Plain-language caveats for the review screen.
    pub notes: Vec<String>,
}

/// A set of coins that must stay together (same address).
#[derive(Debug, Clone)]
struct Cluster {
    outpoints: Vec<OutPoint>,
    value: u64,
    inputs: usize,
}

fn clusters(utxos: &[LocalOutput]) -> Vec<Cluster> {
    let mut by_spk: Vec<(ScriptBuf, Cluster)> = Vec::new();
    for u in utxos {
        let spk = u.txout.script_pubkey.clone();
        match by_spk.iter_mut().find(|(s, _)| *s == spk) {
            Some((_, c)) => {
                c.outpoints.push(u.outpoint);
                c.value += u.txout.value.to_sat();
                c.inputs += 1;
            }
            None => by_spk.push((
                spk,
                Cluster {
                    outpoints: vec![u.outpoint],
                    value: u.txout.value.to_sat(),
                    inputs: 1,
                },
            )),
        }
    }
    by_spk.into_iter().map(|(_, c)| c).collect()
}

/// Pick clusters reaching `target`, preferring one cluster, else the fewest
/// largest ones. Removes the chosen clusters from `pool`.
fn take(pool: &mut Vec<Cluster>, target: u64) -> Option<Vec<Cluster>> {
    pool.sort_by_key(|c| c.value);
    if let Some(i) = pool.iter().position(|c| c.value >= target) {
        return Some(vec![pool.remove(i)]);
    }
    let mut chosen = Vec::new();
    let mut sum = 0;
    while sum < target {
        let c = pool.pop()?;
        sum += c.value;
        chosen.push(c);
    }
    Some(chosen)
}

fn output_weight(spk_len: usize) -> u64 {
    // value (8) + script length varint (1) + script, all non-witness.
    (8 + 1 + spk_len as u64) * 4
}

/// Plan a private send of `amount` to `dest` at `fee_rate`.
///
/// `fresh` gives three of our own new change scripts (decoy, change A, change B).
pub fn plan(
    utxos: &[LocalOutput],
    input_weight: Weight,
    dest: &ScriptBuf,
    amount: u64,
    fee_rate: FeeRate,
    fresh: [ScriptBuf; 3],
) -> Result<PrivatePlan> {
    let total: u64 = utxos.iter().map(|u| u.txout.value.to_sat()).sum();
    let all = clusters(utxos);
    if all.len() < 2 {
        return Err(Error::Invalid(
            "Private send needs coins at two or more different addresses. Use Efficient for this one".into(),
        ));
    }

    let [decoy, change_a, change_b] = fresh;
    // Non-witness overhead + segwit marker/flag.
    let base_weight: u64 = (4 + 4 + 1 + 1) * 4 + 2;
    let per_input = input_weight.to_wu() + (32 + 4 + 4 + 1) * 4;
    let fee_for = |inputs: usize, outputs: &[&ScriptBuf]| -> u64 {
        let w = base_weight
            + per_input * inputs as u64
            + outputs.iter().map(|s| output_weight(s.len())).sum::<u64>();
        fee_rate
            .fee_wu(Weight::from_wu(w))
            .map(|a| a.to_sat())
            .unwrap_or(u64::MAX)
    };

    // Start by assuming two inputs per side, then refine with the real counts.
    let mut guess = fee_for(4, &[dest, &decoy, &change_a, &change_b]);
    for _ in 0..6 {
        let side_target = amount + guess.div_ceil(2);
        let mut pool = all.clone();
        let (Some(a), Some(b)) = (take(&mut pool, side_target), take(&mut pool, side_target))
        else {
            return Err(Error::Invalid(format!(
                "Private send needs about twice the amount, split across separate coins. You'd need around {} sats in at least two coins; you have {} sats",
                2 * side_target,
                total
            )));
        };
        let sum_a: u64 = a.iter().map(|c| c.value).sum();
        let sum_b: u64 = b.iter().map(|c| c.value).sum();
        let n_inputs: usize = a.iter().chain(&b).map(|c| c.inputs).sum();

        let fee = fee_for(n_inputs, &[dest, &decoy, &change_a, &change_b]);
        let (fee_a, fee_b) = (fee / 2, fee - fee / 2);
        if sum_a < amount + fee_a || sum_b < amount + fee_b {
            guess = fee + fee / 4;
            continue;
        }

        let mut outputs = vec![(dest.clone(), amount), (decoy.clone(), amount)];
        let mut fee_final = fee;
        for (spk, change) in [
            (change_a, sum_a - amount - fee_a),
            (change_b, sum_b - amount - fee_b),
        ] {
            if change >= DUST {
                outputs.push((spk, change));
            } else {
                // Too small to keep: it goes to the miners. The output we
                // didn't create also makes the tx a little lighter.
                fee_final += change;
            }
        }

        let mut notes = Vec::new();
        if script_kind(dest) != script_kind(&decoy) {
            notes.push(
                "The recipient uses a different address type from this wallet, so the payment output stands out a little. The cover is weaker than usual."
                    .into(),
            );
        }
        if outputs.len() < 4 {
            notes.push("One change amount was too small to keep, so the pattern is slightly less convincing.".into());
        }

        let inputs = a.into_iter().chain(b).flat_map(|c| c.outpoints).collect();
        return Ok(PrivatePlan {
            inputs,
            outputs,
            fee: fee_final,
            notes,
        });
    }
    Err(Error::Invalid(
        "Couldn't balance a private transaction with these coins. Use Efficient for this one"
            .into(),
    ))
}

#[derive(PartialEq, Eq, Debug)]
enum Kind {
    P2wpkh,
    P2wsh,
    P2tr,
    P2sh,
    P2pkh,
    Other,
}

fn script_kind(s: &ScriptBuf) -> Kind {
    if s.is_p2wpkh() {
        Kind::P2wpkh
    } else if s.is_p2wsh() {
        Kind::P2wsh
    } else if s.is_p2tr() {
        Kind::P2tr
    } else if s.is_p2sh() {
        Kind::P2sh
    } else if s.is_p2pkh() {
        Kind::P2pkh
    } else {
        Kind::Other
    }
}

/// Spendable coins for a private send: every coin, including unconfirmed
/// incoming ones (0-conf).
pub fn spendable(wallet: &Wallet) -> Vec<LocalOutput> {
    wallet.list_unspent().collect()
}

/// The coins to try first: confirmed, or our own unconfirmed change.
pub fn settled(utxos: &[LocalOutput]) -> Vec<LocalOutput> {
    utxos
        .iter()
        .filter(|u| u.chain_position.is_confirmed() || u.keychain == KeychainKind::Internal)
        .cloned()
        .collect()
}

/// Worst-case witness weight to spend one of this wallet's coins.
pub fn input_weight(wallet: &Wallet) -> Result<Weight> {
    wallet
        .public_descriptor(KeychainKind::External)
        .max_weight_to_satisfy()
        .map_err(|e| Error::rail("on-chain", e))
}

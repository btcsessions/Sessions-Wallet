//! Pure amount math for Boltz swaps. No network, fully unit-tested.
//!
//! Boltz charges `ceil(amount × percentage / 100)` plus a fixed miner fee on
//! every swap (the same rounding `boltz-client`'s fee helpers use). We always
//! solve for the smallest amount our account has to pay so the destination
//! receives *at least* what was asked, then double-check Boltz's actual quote
//! before anything can be paid.

use sessions_core::{Error, ReceiveQuote, Result, Sats};

/// Liquid's minimum relay fee rate, sat/vB.
pub const LIQUID_MIN_FEE_RATE: f64 = 0.1;
/// Upper bound (ELIP-200 discounted vbytes) for our one-input claim, including
/// the larger script-path spend used when Boltz won't co-sign. A key-path
/// claim is ~193 vB, a script-path one ~235 vB.
pub const LIQUID_CLAIM_MAX_VSIZE: u64 = 300;
/// Most Boltz may round its quote above our own inverse of its fee formula.
pub const QUOTE_TOLERANCE_SAT: Sats = 2;
/// Our one-input, one-output BTC claim (key path, Taproot destination). Used
/// for quotes only; the real claim is signed at a fee rate.
pub const BTC_CLAIM_VSIZE: u64 = 125;
/// Fee rate bounds for BTC claims, sat/vB. The cap stops a broken fee
/// estimate from burning the claim.
pub const BTC_MIN_FEE_RATE: f64 = 1.0;
pub const BTC_MAX_FEE_RATE: f64 = 500.0;
/// Smallest BTC output we'll create (P2PKH dust; every other type is lower).
pub const BTC_DUST_SAT: Sats = 546;
/// Shown to the user as the swap provider.
pub const PROVIDER: &str = "Boltz";

fn rail(msg: impl std::fmt::Display) -> Error {
    Error::rail("boltz", msg)
}

fn check_pct(pct: f64) -> Result<()> {
    if !(0.0..50.0).contains(&pct) || !pct.is_finite() {
        return Err(rail(format!("unexpected Boltz fee percentage {pct}")));
    }
    Ok(())
}

/// Boltz's percentage fee on `amount`.
pub fn percentage_fee(pct: f64, amount: Sats) -> Sats {
    ((pct / 100.0) * amount as f64).ceil() as Sats
}

/// What is left of `gross` after Boltz's percentage fee and a fixed fee.
pub fn net_of(gross: Sats, pct: f64, fixed: Sats) -> Option<Sats> {
    gross
        .checked_sub(percentage_fee(pct, gross))?
        .checked_sub(fixed)
}

/// Smallest `gross` with `net_of(gross) >= net`. `net_of` never decreases as
/// `gross` grows, so a closed-form start plus a short walk is exact.
pub fn gross_for_net(net: Sats, pct: f64, fixed: Sats) -> Result<Sats> {
    check_pct(pct)?;
    let need = net
        .checked_add(fixed)
        .ok_or_else(|| rail("amount overflow"))?;
    let mut g = ((need as f64) / (1.0 - pct / 100.0)).ceil() as Sats;
    let ok = |g: Sats| net_of(g, pct, fixed).is_some_and(|n| n >= net);
    for _ in 0..1_000 {
        if ok(g) {
            if g > 0 && ok(g - 1) {
                g -= 1;
                continue;
            }
            return Ok(g);
        }
        g = g.checked_add(1).ok_or_else(|| rail("amount overflow"))?;
    }
    Err(rail("could not solve the swap amount"))
}

/// Absolute fee we pay for our Liquid claim transaction: Boltz's own estimate,
/// but always enough for a script-path claim at Liquid's minimum fee rate, so
/// the destination receives the full amount whichever way we claim.
pub fn liquid_claim_fee(boltz_estimate: Sats) -> Sats {
    let floor = (LIQUID_CLAIM_MAX_VSIZE as f64 * LIQUID_MIN_FEE_RATE).ceil() as Sats;
    boltz_estimate.max(floor)
}

pub fn check_limits(amount: Sats, min: Sats, max: Sats, what: &str) -> Result<()> {
    if amount < min {
        return Err(rail(format!(
            "{what} needs at least {min} sats; this one is {amount} sats"
        )));
    }
    if amount > max {
        return Err(rail(format!(
            "{what} allows at most {max} sats; this one is {amount} sats"
        )));
    }
    Ok(())
}

/// Plan for paying a Liquid address over Lightning (reverse swap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReverseQuote {
    /// Invoice amount our Lightning account pays Boltz.
    pub invoice_sat: Sats,
    /// L-BTC Boltz must lock: target + our claim fee.
    pub onchain_needed_sat: Sats,
    pub claim_fee_sat: Sats,
}

pub fn quote_reverse(
    target: Sats,
    pct: f64,
    lockup_fee: Sats,
    claim_fee: Sats,
) -> Result<ReverseQuote> {
    let onchain_needed = target
        .checked_add(claim_fee)
        .ok_or_else(|| rail("amount overflow"))?;
    let invoice = gross_for_net(onchain_needed, pct, lockup_fee)?;
    Ok(ReverseQuote {
        invoice_sat: invoice,
        onchain_needed_sat: onchain_needed,
        claim_fee_sat: claim_fee,
    })
}

/// Plan for paying a Liquid address from on-chain BTC (chain swap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainQuote {
    /// L-BTC we ask Boltz to lock (`serverLockAmount`): target + our claim fee.
    pub server_lock_sat: Sats,
    /// BTC we expect to lock (`userLockAmount`), before Boltz confirms it.
    pub user_lock_sat: Sats,
    pub claim_fee_sat: Sats,
}

pub fn quote_chain(
    target: Sats,
    pct: f64,
    server_fee: Sats,
    claim_fee: Sats,
) -> Result<ChainQuote> {
    let server_lock = target
        .checked_add(claim_fee)
        .ok_or_else(|| rail("amount overflow"))?;
    let user_lock = gross_for_net(server_lock, pct, server_fee)?;
    Ok(ChainQuote {
        server_lock_sat: server_lock,
        user_lock_sat: user_lock,
        claim_fee_sat: claim_fee,
    })
}

/// Accept Boltz's `userLockAmount` only if it is what the advertised fees imply.
pub fn check_chain_quote(
    q: &ChainQuote,
    boltz_server_lock: Sats,
    boltz_user_lock: Sats,
) -> Result<()> {
    if boltz_server_lock != q.server_lock_sat {
        return Err(rail(format!(
            "Boltz offered to lock {boltz_server_lock} sats of L-BTC, expected {}",
            q.server_lock_sat
        )));
    }
    if boltz_user_lock > q.user_lock_sat + QUOTE_TOLERANCE_SAT
        || boltz_user_lock <= boltz_server_lock
    {
        return Err(rail(format!(
            "Boltz asked for {boltz_user_lock} sats, expected about {}",
            q.user_lock_sat
        )));
    }
    Ok(())
}

/// Highest fee we accept for a submarine swap (outside sender pays L-BTC, we
/// receive `invoice` over Lightning). Covers Boltz charging its percentage on
/// either the invoice or the on-chain amount.
pub fn submarine_max_fee(invoice: Sats, pct: f64, miner_fee: Sats) -> Result<Sats> {
    check_pct(pct)?;
    let gross = gross_for_net(
        invoice
            .checked_add(miner_fee)
            .ok_or_else(|| rail("amount overflow"))?,
        pct,
        0,
    )?;
    Ok(gross - invoice + QUOTE_TOLERANCE_SAT)
}

/// A fee estimate clamped to what we're willing to sign a BTC claim at.
pub fn clamp_btc_rate(rate: f64) -> f64 {
    if !rate.is_finite() {
        return BTC_MIN_FEE_RATE;
    }
    rate.clamp(BTC_MIN_FEE_RATE, BTC_MAX_FEE_RATE)
}

/// Estimated fee of our BTC claim: Boltz's own estimate or ours at `rate`,
/// whichever is higher.
pub fn btc_claim_fee(boltz_estimate: Sats, rate: f64) -> Sats {
    let ours = (BTC_CLAIM_VSIZE as f64 * clamp_btc_rate(rate)).ceil() as Sats;
    boltz_estimate.max(ours)
}

/// Fee rate to (re-)sign a BTC claim at. A re-sign never goes below the
/// previous rate; a bump (claim stuck in the mempool) goes at least
/// 1 sat/vB above it, which is enough for BIP125 replacement.
pub fn btc_claim_rate(current: f64, previous: Option<f64>, bump: bool) -> f64 {
    let floor = match (previous, bump) {
        (Some(p), true) => p + 1.0,
        (Some(p), false) => p,
        (None, _) => 0.0,
    };
    clamp_btc_rate(current.max(floor))
}

/// Receive bridges where the outside payer pays `amount` and fees come out
/// of it (LN → BTC reverse swap, L-BTC → BTC chain swap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayerPaysQuote {
    /// BTC Boltz must lock: `amount` minus its percentage and fixed fee.
    pub boltz_lock_sat: Sats,
    /// What reaches our address after our claim fee.
    pub receive_sat: Sats,
    /// `amount - receive_sat`.
    pub fee_sat: Sats,
}

pub fn quote_payer_pays(
    amount: Sats,
    pct: f64,
    boltz_fixed: Sats,
    claim_fee: Sats,
) -> Result<PayerPaysQuote> {
    check_pct(pct)?;
    let too_small = || {
        rail(format!(
            "{amount} sats doesn't cover the swap fees; send more"
        ))
    };
    let lock = net_of(amount, pct, boltz_fixed).ok_or_else(too_small)?;
    let receive = lock.checked_sub(claim_fee).ok_or_else(too_small)?;
    if receive < BTC_DUST_SAT {
        return Err(too_small());
    }
    Ok(PayerPaysQuote {
        boltz_lock_sat: lock,
        receive_sat: receive,
        fee_sat: amount - receive,
    })
}

/// Accept Boltz's BTC lockup amount only if it's what its advertised fees
/// imply for what the payer pays.
pub fn check_boltz_lock(q: &PayerPaysQuote, offered: Sats, paid: Sats) -> Result<()> {
    if offered + QUOTE_TOLERANCE_SAT < q.boltz_lock_sat || offered >= paid {
        return Err(rail(format!(
            "Boltz offered to lock {offered} sats for {paid} sats, expected about {}",
            q.boltz_lock_sat
        )));
    }
    Ok(())
}

/// Turn live pair data into a [`ReceiveQuote`]. `payer_pays`: `amount` is
/// what the payer sends and fees come out of it; otherwise `amount` is what
/// we receive and the payer pays the fees on top.
pub fn receive_quote(
    min: Sats,
    max: Sats,
    pct: f64,
    network_fee: Sats,
    amount: Option<Sats>,
    payer_pays: bool,
) -> ReceiveQuote {
    let total = amount.map(|a| percentage_fee(pct, a).saturating_add(network_fee));
    let receive = match (amount, total) {
        (Some(a), Some(t)) if payer_pays => Some(a.saturating_sub(t)),
        (Some(a), _) => Some(a),
        _ => None,
    };
    ReceiveQuote {
        provider: PROVIDER.into(),
        min_sat: min,
        max_sat: max,
        fee_percent: pct,
        network_fee_sat: network_fee,
        total_fee_sat: total,
        receive_sat: receive,
    }
}

/// `12345` → `"0.00012345"`, exact, no trailing zeros.
pub fn sats_to_btc(sats: Sats) -> String {
    let s = format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000);
    let s = s.trim_end_matches('0');
    s.trim_end_matches('.').to_string()
}

pub fn liquid_bip21(address: &str, amount: Sats, asset_id: &str) -> String {
    format!(
        "liquidnetwork:{address}?amount={}&assetid={asset_id}",
        sats_to_btc(amount)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentage_rounds_up_like_boltz() {
        assert_eq!(percentage_fee(0.25, 100_000), 250);
        assert_eq!(percentage_fee(0.25, 100_001), 251);
        assert_eq!(percentage_fee(0.1, 1), 1);
        assert_eq!(percentage_fee(0.0, 1_000), 0);
    }

    #[test]
    fn gross_is_minimal_and_sufficient() {
        for &(net, pct, fixed) in &[
            (1u64, 0.25, 0u64),
            (25_000, 0.25, 27),
            (50_020, 0.1, 19),
            (99_999, 0.5, 300),
            (1_000_000, 0.25, 1_500),
            (21_000_000, 0.0, 0),
        ] {
            let g = gross_for_net(net, pct, fixed).unwrap();
            assert!(net_of(g, pct, fixed).unwrap() >= net, "{net} {pct} {fixed}");
            assert!(
                net_of(g - 1, pct, fixed).is_none_or(|n| n < net),
                "not minimal: {net} {pct} {fixed}"
            );
        }
        assert!(gross_for_net(1, 75.0, 0).is_err());
        assert!(gross_for_net(1, f64::NAN, 0).is_err());
    }

    #[test]
    fn reverse_quote_delivers_target() {
        // 0.25% fee, 27 sat lockup, 30 sat claim: send 25k sats to Liquid.
        let q = quote_reverse(25_000, 0.25, 27, 30).unwrap();
        assert_eq!(q.onchain_needed_sat, 25_030);
        // Boltz locks invoice - 0.25% - lockup; we claim it minus our fee.
        let locked = net_of(q.invoice_sat, 0.25, 27).unwrap();
        assert!(locked >= 25_030);
        assert_eq!(locked - q.claim_fee_sat, 25_000);
        assert_eq!(q.invoice_sat, 25_120);
    }

    #[test]
    fn chain_quote_and_check() {
        let q = quote_chain(50_000, 0.1, 400, 20).unwrap();
        assert_eq!(q.server_lock_sat, 50_020);
        assert_eq!(net_of(q.user_lock_sat, 0.1, 400).unwrap(), 50_020);
        assert!(check_chain_quote(&q, 50_020, q.user_lock_sat).is_ok());
        assert!(check_chain_quote(&q, 50_020, q.user_lock_sat + QUOTE_TOLERANCE_SAT).is_ok());
        assert!(check_chain_quote(&q, 50_020, q.user_lock_sat + QUOTE_TOLERANCE_SAT + 1).is_err());
        assert!(check_chain_quote(&q, 50_019, q.user_lock_sat).is_err());
        assert!(check_chain_quote(&q, 50_020, 50_020).is_err());
    }

    #[test]
    fn submarine_fee_bound() {
        // 0.1% + 19 sats miner fee on a 100k invoice.
        let max = submarine_max_fee(100_000, 0.1, 19).unwrap();
        // Fee on the invoice amount: 100 + 19.
        assert!(119 <= max);
        // Fee on the gross amount is slightly more and still accepted.
        let gross = gross_for_net(100_019, 0.1, 0).unwrap();
        assert!(gross - 100_000 <= max);
        assert!(max <= 125);
    }

    #[test]
    fn claim_fee_floor() {
        // 300 vB × 0.1 sat/vB covers even a script-path claim.
        assert_eq!(liquid_claim_fee(0), 30);
        assert_eq!(liquid_claim_fee(19), 30);
        assert_eq!(liquid_claim_fee(45), 45);
    }

    #[test]
    fn limits() {
        assert!(check_limits(1_000, 1_000, 25_000_000, "x").is_ok());
        assert!(check_limits(999, 1_000, 25_000_000, "x")
            .unwrap_err()
            .to_string()
            .contains("at least 1000"));
        assert!(check_limits(25_000_001, 1_000, 25_000_000, "x")
            .unwrap_err()
            .to_string()
            .contains("at most"));
    }

    #[test]
    fn payer_pays_quote() {
        // 0.5% + 530 sat lockup + 140 sat claim on a 100k invoice.
        let q = quote_payer_pays(100_000, 0.5, 530, 140).unwrap();
        assert_eq!(q.boltz_lock_sat, 100_000 - 500 - 530);
        assert_eq!(q.receive_sat, q.boltz_lock_sat - 140);
        assert_eq!(q.fee_sat, 500 + 530 + 140);
        // Matches what the quote API reports for the same numbers.
        let rq = receive_quote(25_000, 25_000_000, 0.5, 530 + 140, Some(100_000), true);
        assert_eq!(rq.total_fee_sat, Some(q.fee_sat));
        assert_eq!(rq.receive_sat, Some(q.receive_sat));
        assert_eq!(rq.provider, "Boltz");
        // Too small to cover fees, or would leave dust.
        assert!(quote_payer_pays(500, 0.5, 530, 140).is_err());
        assert!(quote_payer_pays(1_200, 0.1, 530, 140).is_err());
        assert!(quote_payer_pays(1_300, 0.1, 530, 140).is_ok());
        assert!(quote_payer_pays(100_000, 80.0, 0, 0).is_err());
    }

    #[test]
    fn boltz_lock_check() {
        let q = quote_payer_pays(100_000, 0.5, 530, 140).unwrap();
        assert!(check_boltz_lock(&q, q.boltz_lock_sat, 100_000).is_ok());
        assert!(check_boltz_lock(&q, q.boltz_lock_sat - QUOTE_TOLERANCE_SAT, 100_000).is_ok());
        assert!(check_boltz_lock(&q, q.boltz_lock_sat - QUOTE_TOLERANCE_SAT - 1, 100_000).is_err());
        // More is fine, but never the whole payment or more.
        assert!(check_boltz_lock(&q, q.boltz_lock_sat + 50, 100_000).is_ok());
        assert!(check_boltz_lock(&q, 100_000, 100_000).is_err());
    }

    #[test]
    fn receiver_gets_quote() {
        // Liquid → Lightning: the invoice amount arrives; the payer adds fees.
        let q = receive_quote(1_000, 25_000_000, 0.1, 19, Some(100_000), false);
        assert_eq!(q.total_fee_sat, Some(119));
        assert_eq!(q.receive_sat, Some(100_000));
        let q = receive_quote(1_000, 25_000_000, 0.1, 19, None, false);
        assert_eq!((q.total_fee_sat, q.receive_sat), (None, None));
        // Fees larger than the amount report zero, not an underflow.
        let q = receive_quote(25_000, 25_000_000, 0.5, 900, Some(800), true);
        assert_eq!(q.receive_sat, Some(0));
    }

    #[test]
    fn btc_claim_fees_and_rates() {
        assert_eq!(btc_claim_fee(0, 2.0), 250);
        assert_eq!(btc_claim_fee(400, 2.0), 400);
        // Clamped: broken estimates never produce a silly fee.
        assert_eq!(btc_claim_fee(0, 0.2), 125);
        assert_eq!(btc_claim_fee(0, f64::NAN), 125);
        assert_eq!(btc_claim_fee(0, 1e9), 125 * 500);
        assert_eq!(btc_claim_rate(5.0, None, false), 5.0);
        assert_eq!(btc_claim_rate(5.0, Some(8.0), false), 8.0);
        assert_eq!(btc_claim_rate(5.0, Some(8.0), true), 9.0);
        assert_eq!(btc_claim_rate(20.0, Some(8.0), true), 20.0);
        assert_eq!(
            btc_claim_rate(5.0, Some(BTC_MAX_FEE_RATE), true),
            BTC_MAX_FEE_RATE
        );
    }

    #[test]
    fn btc_strings() {
        assert_eq!(sats_to_btc(12_345), "0.00012345");
        assert_eq!(sats_to_btc(100_000_000), "1");
        assert_eq!(sats_to_btc(150_000_000), "1.5");
        assert_eq!(sats_to_btc(1), "0.00000001");
        assert_eq!(sats_to_btc(0), "0");
        assert_eq!(
            liquid_bip21("lq1abc", 10_000, "6f02"),
            "liquidnetwork:lq1abc?amount=0.0001&assetid=6f02"
        );
    }
}

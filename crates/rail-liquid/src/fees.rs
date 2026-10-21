//! Pure amount math for Boltz swaps. No network, fully unit-tested.
//!
//! Boltz charges `ceil(amount × percentage / 100)` plus a fixed miner fee on
//! every swap (the same rounding `boltz-client`'s fee helpers use). We always
//! solve for the smallest amount our account has to pay so the destination
//! receives *at least* what was asked, then double-check Boltz's actual quote
//! before anything can be paid.

use sessions_core::{Error, Result, Sats};

/// Liquid's minimum relay fee rate, sat/vB.
pub const LIQUID_MIN_FEE_RATE: f64 = 0.1;
/// Upper bound (ELIP-200 discounted vbytes) for our one-input claim, including
/// the larger script-path spend used when Boltz won't co-sign. A key-path
/// claim is ~193 vB, a script-path one ~235 vB.
pub const LIQUID_CLAIM_MAX_VSIZE: u64 = 300;
/// Most Boltz may round its quote above our own inverse of its fee formula.
pub const QUOTE_TOLERANCE_SAT: Sats = 2;

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

//! Boltz status → what we do next. Pure, so the whole lifecycle is testable.
//!
//! Status strings: <https://api.docs.boltz.exchange/lifecycle.html>.

use crate::store::SwapKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do yet.
    Wait,
    /// Build, store and broadcast our claim transaction.
    Claim,
    /// Submarine: Boltz paid our invoice.
    Paid,
    /// Submarine: Boltz wants to sweep the lockup cooperatively.
    Cooperate,
    /// Chain swap: refund our BTC lockup (if there is one).
    Refund,
    /// L-BTC from an outside sender is stuck in a lockup we can't refund
    /// automatically (we hold no Liquid wallet).
    Rescue,
    /// L-BTC -> BTC chain swap: the sender locked the wrong amount; check
    /// Boltz's new quote against what was actually locked and accept it.
    Requote,
    Complete,
    /// Ended without any of our funds at stake.
    Fail,
}

/// Statuses meaning the payer's lockup transaction exists.
pub fn lockup_seen(status: &str) -> bool {
    matches!(
        status,
        "transaction.mempool"
            | "transaction.confirmed"
            | "transaction.zeroconf.rejected"
            | "invoice.pending"
            | "invoice.paid"
            | "invoice.failedToPay"
            | "transaction.lockupFailed"
            | "transaction.claim.pending"
            | "transaction.claimed"
            | "transaction.server.mempool"
            | "transaction.server.confirmed"
    )
}

pub fn plan(kind: SwapKind, status: &str, lockup_was_seen: bool) -> Action {
    match kind {
        // We paid Boltz's hold invoice; Boltz locks L-BTC for us. `Claim`
        // itself refuses to reveal the preimage until our own explorer shows
        // Boltz's lockup confirmed (Liquid blocks are 1 minute), so starting
        // at `transaction.mempool` is safe and doesn't depend on Boltz
        // reporting `transaction.confirmed`.
        // Same for a BTC reverse swap: `claim_btc` waits for Boltz's BTC
        // lockup to confirm (one block) before revealing the preimage.
        SwapKind::Reverse | SwapKind::ReverseToBtc => match status {
            "transaction.mempool" | "transaction.confirmed" | "invoice.settled" => Action::Claim,
            "swap.expired" | "invoice.expired" | "transaction.failed" | "transaction.refunded" => {
                Action::Fail
            }
            _ => Action::Wait,
        },
        // We lock BTC; Boltz locks L-BTC. Same confirmation rule as above.
        SwapKind::ChainBtcToLbtc => match status {
            "transaction.server.mempool"
            | "transaction.server.confirmed"
            | "transaction.claim.pending"
            | "transaction.claimed" => Action::Claim,
            "transaction.lockupFailed"
            | "swap.expired"
            | "transaction.failed"
            | "transaction.refunded" => Action::Refund,
            _ => Action::Wait,
        },
        // An outside sender locks L-BTC; Boltz locks BTC; we claim the BTC.
        // A failed / expired swap goes to `on_fail`, which checks whether the
        // sender's L-BTC is stuck in the lockup (then it needs a rescue).
        SwapKind::ChainLbtcToBtc => match status {
            "transaction.server.mempool"
            | "transaction.server.confirmed"
            | "transaction.claim.pending"
            | "transaction.claimed" => Action::Claim,
            "transaction.lockupFailed" => Action::Requote,
            "swap.expired" | "transaction.failed" | "transaction.refunded" => Action::Fail,
            _ => Action::Wait,
        },
        // An outside sender locks L-BTC; Boltz pays our invoice and sweeps.
        SwapKind::Submarine => match status {
            "invoice.paid" => Action::Paid,
            "transaction.claim.pending" => Action::Cooperate,
            "transaction.claimed" => Action::Complete,
            "invoice.failedToPay" | "transaction.lockupFailed" => Action::Rescue,
            "swap.expired" if lockup_was_seen => Action::Rescue,
            "swap.expired" => Action::Fail,
            _ => Action::Wait,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Phase;
    use Action::*;
    use SwapKind::*;

    #[test]
    fn reverse() {
        for s in ["swap.created", "minerfee.paid", "something.new"] {
            assert_eq!(plan(Reverse, s, false), Wait, "{s}");
        }
        assert_eq!(plan(Reverse, "transaction.mempool", true), Claim);
        assert_eq!(plan(Reverse, "transaction.confirmed", true), Claim);
        assert_eq!(plan(Reverse, "invoice.settled", true), Claim);
        for s in [
            "swap.expired",
            "invoice.expired",
            "transaction.failed",
            "transaction.refunded",
        ] {
            assert_eq!(plan(Reverse, s, true), Fail, "{s}");
        }
    }

    #[test]
    fn chain() {
        for s in [
            "swap.created",
            "transaction.mempool",
            "transaction.confirmed",
            "transaction.zeroconf.rejected",
        ] {
            assert_eq!(plan(ChainBtcToLbtc, s, false), Wait, "{s}");
        }
        for s in [
            "transaction.server.mempool",
            "transaction.server.confirmed",
            "transaction.claim.pending",
            "transaction.claimed",
        ] {
            assert_eq!(plan(ChainBtcToLbtc, s, true), Claim, "{s}");
        }
        for s in [
            "transaction.lockupFailed",
            "swap.expired",
            "transaction.failed",
            "transaction.refunded",
        ] {
            assert_eq!(plan(ChainBtcToLbtc, s, true), Refund, "{s}");
        }
    }

    #[test]
    fn reverse_to_btc_matches_reverse() {
        for s in [
            "swap.created",
            "minerfee.paid",
            "transaction.mempool",
            "transaction.confirmed",
            "invoice.settled",
            "swap.expired",
            "invoice.expired",
            "transaction.failed",
            "transaction.refunded",
        ] {
            assert_eq!(plan(ReverseToBtc, s, true), plan(Reverse, s, true), "{s}");
        }
    }

    #[test]
    fn chain_lbtc_to_btc() {
        for s in [
            "swap.created",
            "transaction.mempool",
            "transaction.confirmed",
            "transaction.zeroconf.rejected",
        ] {
            assert_eq!(plan(ChainLbtcToBtc, s, false), Wait, "{s}");
        }
        for s in [
            "transaction.server.mempool",
            "transaction.server.confirmed",
            "transaction.claim.pending",
            "transaction.claimed",
        ] {
            assert_eq!(plan(ChainLbtcToBtc, s, true), Claim, "{s}");
        }
        assert_eq!(
            plan(ChainLbtcToBtc, "transaction.lockupFailed", true),
            Requote
        );
        for s in ["swap.expired", "transaction.failed", "transaction.refunded"] {
            assert_eq!(plan(ChainLbtcToBtc, s, true), Fail, "{s}");
            assert_eq!(plan(ChainLbtcToBtc, s, false), Fail, "{s}");
        }
    }

    #[test]
    fn kinds() {
        assert!(ReverseToBtc.claims_on_bitcoin() && ChainLbtcToBtc.claims_on_bitcoin());
        assert!(!Reverse.claims_on_bitcoin() && !ChainBtcToLbtc.claims_on_bitcoin());
        assert!(Submarine.refunds_on_liquid() && ChainLbtcToBtc.refunds_on_liquid());
        assert!(!ChainBtcToLbtc.refunds_on_liquid() && !ReverseToBtc.refunds_on_liquid());
    }

    #[test]
    fn submarine() {
        for s in [
            "swap.created",
            "invoice.set",
            "transaction.mempool",
            "transaction.confirmed",
            "invoice.pending",
        ] {
            assert_eq!(plan(Submarine, s, false), Wait, "{s}");
        }
        assert_eq!(plan(Submarine, "invoice.paid", true), Paid);
        assert_eq!(
            plan(Submarine, "transaction.claim.pending", true),
            Cooperate
        );
        assert_eq!(plan(Submarine, "transaction.claimed", true), Complete);
        assert_eq!(plan(Submarine, "invoice.failedToPay", true), Rescue);
        assert_eq!(plan(Submarine, "transaction.lockupFailed", true), Rescue);
        assert_eq!(plan(Submarine, "swap.expired", false), Fail);
        assert_eq!(plan(Submarine, "swap.expired", true), Rescue);
    }

    #[test]
    fn lockup_detection() {
        assert!(!lockup_seen("swap.created"));
        assert!(!lockup_seen("invoice.set"));
        assert!(!lockup_seen("swap.expired"));
        assert!(lockup_seen("transaction.mempool"));
        assert!(lockup_seen("invoice.failedToPay"));
    }

    #[test]
    fn phases() {
        for p in [Phase::Complete, Phase::Refunded, Phase::Failed] {
            assert!(p.finished());
        }
        for p in [
            Phase::Pending,
            Phase::Paid,
            Phase::Claiming,
            Phase::Refunding,
            Phase::NeedsRescue,
        ] {
            assert!(!p.finished(), "{p:?}");
        }
        assert_eq!(Phase::NeedsRescue.as_str(), "refund_required");
        assert_eq!(Phase::Claiming.as_str(), "claiming");
    }
}

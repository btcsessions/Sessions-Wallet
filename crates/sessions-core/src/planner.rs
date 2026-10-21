//! Route finding between accounts, swap providers and outside destinations.
//!
//! The planner is pure: it only looks at what each account says it can pay
//! and receive, so it is easy to test and reason about. [`crate::Wallet`]
//! executes the plan.

use serde::{Deserialize, Serialize};

use crate::types::{AccountId, AccountInfo, Method, PaymentRequest, RailKind, Sats};
use crate::{Error, Result};

/// Lower is better: cheapest and fastest first.
pub fn method_rank(m: Method) -> u8 {
    match m {
        Method::SparkAddress => 0,
        Method::Bolt11 => 1,
        Method::Bolt12 => 2,
        Method::LightningAddress => 3,
        Method::Lnurl => 4,
        Method::CashuToken => 5,
        Method::LiquidAddress => 6,
        Method::BitcoinAddress => 7,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Leg {
    /// `from` pays `to`, which hands out a request of `method`.
    ToAccount {
        from: AccountId,
        to: AccountId,
        method: Method,
    },
    /// `from` pays an outside request directly.
    ToExternal {
        from: AccountId,
        request: PaymentRequest,
    },
    /// `from` pays a swap provider with `pay_method`; the provider delivers `request`.
    ViaSwap {
        from: AccountId,
        provider: String,
        pay_method: Method,
        request: PaymentRequest,
    },
}

impl Leg {
    pub fn from(&self) -> &AccountId {
        match self {
            Leg::ToAccount { from, .. }
            | Leg::ToExternal { from, .. }
            | Leg::ViaSwap { from, .. } => from,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub legs: Vec<Leg>,
    pub amount_sat: Sats,
}

/// What a swap provider can bridge.
#[derive(Debug, Clone)]
pub struct Bridges {
    pub provider: String,
    /// (target method, method our account pays instead)
    pub pay: Vec<(Method, Method)>,
}

fn best_shared(a: &[Method], b: &[Method]) -> Option<Method> {
    a.iter()
        .copied()
        .filter(|m| b.contains(m))
        .min_by_key(|m| method_rank(*m))
}

fn find<'a>(accounts: &'a [AccountInfo], id: &AccountId) -> Result<&'a AccountInfo> {
    accounts
        .iter()
        .find(|a| &a.id == id)
        .ok_or_else(|| Error::AccountNotFound(id.0.clone()))
}

/// Plan moving `amount` from one of the user's accounts to another.
///
/// Tries a direct transfer, then a single hop through a third account
/// (Spark preferred as the hub because it speaks Lightning and on-chain).
pub fn plan_move(
    accounts: &[AccountInfo],
    from: &AccountId,
    to: &AccountId,
    amount_sat: Sats,
) -> Result<Plan> {
    if from == to {
        return Err(Error::NoRoute);
    }
    let a = find(accounts, from)?;
    let b = find(accounts, to)?;

    if let Some(method) = best_shared(&a.pays, &b.receives) {
        return Ok(Plan {
            legs: vec![Leg::ToAccount {
                from: from.clone(),
                to: to.clone(),
                method,
            }],
            amount_sat,
        });
    }

    let mut hubs: Vec<&AccountInfo> = accounts
        .iter()
        .filter(|h| h.id != *from && h.id != *to)
        .collect();
    hubs.sort_by_key(|h| match h.rail {
        crate::RailKind::Spark => 0,
        crate::RailKind::Ecash => 1,
        crate::RailKind::Onchain => 2,
    });

    for hub in hubs {
        if let (Some(m1), Some(m2)) = (
            best_shared(&a.pays, &hub.receives),
            best_shared(&hub.pays, &b.receives),
        ) {
            return Ok(Plan {
                legs: vec![
                    Leg::ToAccount {
                        from: from.clone(),
                        to: hub.id.clone(),
                        method: m1,
                    },
                    Leg::ToAccount {
                        from: hub.id.clone(),
                        to: to.clone(),
                        method: m2,
                    },
                ],
                amount_sat,
            });
        }
    }
    Err(Error::NoRoute)
}

/// Plan paying an outside request from a given account.
pub fn plan_payment(
    accounts: &[AccountInfo],
    bridges: &[Bridges],
    from: &AccountId,
    request: &PaymentRequest,
    amount_sat: Sats,
) -> Result<Plan> {
    let a = find(accounts, from)?;
    let mut options = request.options();
    options.sort_by_key(|o| o.method().map(method_rank).unwrap_or(u8::MAX));

    // 1. Something the account can pay natively.
    for opt in &options {
        if let Some(m) = opt.method() {
            if a.pays.contains(&m) {
                return Ok(Plan {
                    legs: vec![Leg::ToExternal {
                        from: from.clone(),
                        request: opt.clone(),
                    }],
                    amount_sat,
                });
            }
        }
    }

    // 2. Through a swap provider.
    for opt in &options {
        let Some(target) = opt.method() else { continue };
        let mut candidates: Vec<(&Bridges, Method)> = bridges
            .iter()
            .flat_map(|b| {
                b.pay
                    .iter()
                    .filter(|(t, _)| *t == target)
                    .map(move |(_, p)| (b, *p))
            })
            .filter(|(_, p)| a.pays.contains(p))
            .collect();
        candidates.sort_by_key(|(_, p)| method_rank(*p));
        if let Some((b, pay_method)) = candidates.first() {
            return Ok(Plan {
                legs: vec![Leg::ViaSwap {
                    from: from.clone(),
                    provider: b.provider.clone(),
                    pay_method: *pay_method,
                    request: opt.clone(),
                }],
                amount_sat,
            });
        }
    }

    // 3. Hop through another account that can pay it.
    for hub in accounts.iter().filter(|h| h.id != *from) {
        let Ok(inner) = plan_payment(
            std::slice::from_ref(hub),
            bridges,
            &hub.id,
            request,
            amount_sat,
        ) else {
            continue;
        };
        if let Some(m) = best_shared(&a.pays, &hub.receives) {
            let mut legs = vec![Leg::ToAccount {
                from: from.clone(),
                to: hub.id.clone(),
                method: m,
            }];
            legs.extend(inner.legs);
            return Ok(Plan { legs, amount_sat });
        }
    }

    Err(Error::NoRoute)
}

/// Pick which account should pay by default: the cheapest route from an
/// account that has enough balance, preferring fewer legs.
pub fn choose_payer(
    accounts: &[AccountInfo],
    bridges: &[Bridges],
    request: &PaymentRequest,
    amount_sat: Sats,
) -> Result<Plan> {
    accounts
        .iter()
        // On-chain accounts can spend unconfirmed incoming coins too.
        .filter(|a| {
            let unconfirmed = if a.rail == RailKind::Onchain {
                a.balance.pending_in
            } else {
                0
            };
            a.balance.confirmed + unconfirmed >= amount_sat
        })
        .filter_map(|a| plan_payment(accounts, bridges, &a.id, request, amount_sat).ok())
        .min_by_key(|p| {
            let first_rank = match &p.legs[0] {
                Leg::ToExternal { request, .. } => request.method().map(method_rank).unwrap_or(9),
                Leg::ViaSwap { pay_method, .. } => method_rank(*pay_method) + 10,
                Leg::ToAccount { method, .. } => method_rank(*method) + 20,
            };
            (p.legs.len(), first_rank)
        })
        .ok_or(Error::NoRoute)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    fn acct(
        id: &str,
        rail: RailKind,
        receives: &[Method],
        pays: &[Method],
        bal: Sats,
    ) -> AccountInfo {
        AccountInfo {
            id: id.into(),
            name: id.into(),
            rail,
            custody: Custody::Hot,
            balance: Balance {
                confirmed: bal,
                ..Default::default()
            },
            receives: receives.to_vec(),
            pays: pays.to_vec(),
        }
    }

    fn setup() -> Vec<AccountInfo> {
        use Method::*;
        vec![
            acct(
                "savings",
                RailKind::Onchain,
                &[BitcoinAddress],
                &[BitcoinAddress],
                1_000_000,
            ),
            acct(
                "spending",
                RailKind::Spark,
                &[SparkAddress, Bolt11, BitcoinAddress],
                &[
                    SparkAddress,
                    Bolt11,
                    Bolt12,
                    LightningAddress,
                    Lnurl,
                    BitcoinAddress,
                ],
                50_000,
            ),
            acct("cashu", RailKind::Ecash, &[Bolt11], &[Bolt11], 2_000),
        ]
    }

    fn boltz() -> Vec<Bridges> {
        vec![Bridges {
            provider: "boltz".into(),
            pay: vec![
                (Method::LiquidAddress, Method::Bolt11),
                (Method::LiquidAddress, Method::BitcoinAddress),
            ],
        }]
    }

    #[test]
    fn onchain_to_spark_is_direct() {
        let p = plan_move(&setup(), &"savings".into(), &"spending".into(), 10_000).unwrap();
        assert_eq!(
            p.legs,
            vec![Leg::ToAccount {
                from: "savings".into(),
                to: "spending".into(),
                method: Method::BitcoinAddress
            }]
        );
    }

    #[test]
    fn spark_to_ecash_uses_lightning() {
        let p = plan_move(&setup(), &"spending".into(), &"cashu".into(), 1_000).unwrap();
        assert_eq!(
            p.legs[0],
            Leg::ToAccount {
                from: "spending".into(),
                to: "cashu".into(),
                method: Method::Bolt11
            }
        );
    }

    #[test]
    fn onchain_to_ecash_hops_through_spark() {
        let p = plan_move(&setup(), &"savings".into(), &"cashu".into(), 1_000).unwrap();
        assert_eq!(p.legs.len(), 2);
        assert_eq!(
            p.legs[0],
            Leg::ToAccount {
                from: "savings".into(),
                to: "spending".into(),
                method: Method::BitcoinAddress
            }
        );
        assert_eq!(
            p.legs[1],
            Leg::ToAccount {
                from: "spending".into(),
                to: "cashu".into(),
                method: Method::Bolt11
            }
        );
    }

    #[test]
    fn liquid_from_spark_swaps_over_lightning() {
        let req = PaymentRequest::LiquidAddress {
            address: "lq1qq…".into(),
            amount_sat: Some(20_000),
        };
        let p = plan_payment(&setup(), &boltz(), &"spending".into(), &req, 20_000).unwrap();
        assert!(matches!(
            &p.legs[0],
            Leg::ViaSwap {
                pay_method: Method::Bolt11,
                ..
            }
        ));
    }

    #[test]
    fn liquid_from_onchain_uses_chain_swap() {
        let req = PaymentRequest::LiquidAddress {
            address: "lq1qq…".into(),
            amount_sat: None,
        };
        let p = plan_payment(&setup(), &boltz(), &"savings".into(), &req, 20_000).unwrap();
        assert!(matches!(
            &p.legs[0],
            Leg::ViaSwap {
                pay_method: Method::BitcoinAddress,
                ..
            }
        ));
    }

    #[test]
    fn unified_qr_picks_lightning_when_spark_can_afford_it() {
        let req = PaymentRequest::Unified {
            options: vec![
                PaymentRequest::BitcoinAddress {
                    address: "bc1q…".into(),
                    amount_sat: Some(5_000),
                    label: None,
                    payjoin: None,
                },
                PaymentRequest::Bolt11 {
                    invoice: "lnbc…".into(),
                },
            ],
        };
        let p = choose_payer(&setup(), &boltz(), &req, 5_000).unwrap();
        assert_eq!(
            p.legs,
            vec![Leg::ToExternal {
                from: "spending".into(),
                request: PaymentRequest::Bolt11 {
                    invoice: "lnbc…".into()
                }
            }]
        );
    }

    #[test]
    fn big_payment_falls_back_to_savings() {
        let req = PaymentRequest::BitcoinAddress {
            address: "bc1q…".into(),
            amount_sat: None,
            label: None,
            payjoin: None,
        };
        let p = choose_payer(&setup(), &boltz(), &req, 500_000).unwrap();
        assert_eq!(p.legs[0].from(), &AccountId::from("savings"));
    }

    #[test]
    fn same_account_has_no_route() {
        assert!(plan_move(&setup(), &"savings".into(), &"savings".into(), 1).is_err());
    }
}

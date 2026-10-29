//! Private Lightning: hop through an ecash (Cashu) account, the "hub".
//!
//! * **Send:** Spending pays the mint (minting ecash in the hub), then the
//!   hub pays the recipient. Spending's operator sees a payment to the mint;
//!   the mint sees a payment to the recipient; neither sees both ends.
//! * **Receive:** the payer pays the mint (an invoice from the hub); once the
//!   ecash is minted, the hub pays an invoice from the account the user chose
//!   ([`Wallet::forward_private_receive`]).
//!
//! Ecash only pays BOLT11 invoices, so Lightning addresses and LNURL are
//! resolved to an invoice through the paying account first
//! ([`crate::Rail::invoice_for`]). The hub's melt fee reserve is funded on
//! top of the amount; what the mint doesn't use stays in the hub as change.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::types::{
    Direction, Method, Payment, PaymentRequest, ReceiveRequest, Sats, SendQuote, SendRequest,
};
use crate::{AccountId, Error, RailKind, Result, Wallet};

/// A private Lightning payment, priced and ready to run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivatePay {
    pub from: AccountId,
    pub hub: AccountId,
    /// The recipient's invoice (resolved from an address / LNURL if needed).
    pub invoice: String,
    pub amount_sat: Sats,
    /// What goes into the hub: the amount plus the mint's fee reserve.
    pub fund_sat: Sats,
    /// Leg 1: `from` pays the hub's invoice for `fund_sat`.
    pub leg1: SendQuote,
    /// The hub's mint quote for leg 1.
    pub hub_quote: String,
    /// Most this can cost: leg 1's fee plus the hub's reserve (unused
    /// reserve stays in the hub).
    pub max_fee_sat: Sats,
}

/// How long to wait for the mint to issue the ecash after leg 1.
const MINT_WAIT: Duration = Duration::from_secs(90);

/// Melt fees aren't known until a quote; this is the first guess when
/// forwarding a private receive (refined from the mint's answer).
fn reserve_guess(amount: Sats) -> Sats {
    (amount / 100).max(2) + 2
}

/// Extra funding for the mint's input fees (proofs spent in the melt).
fn input_fee_margin(amount: Sats) -> Sats {
    (amount / 200).max(2)
}

impl Wallet {
    async fn hub_fee(&self, hub: &AccountId, invoice: &str, amount: Sats) -> Result<Sats> {
        let r = self
            .account(hub)
            .await?
            .prepare_send(send_to(invoice, amount))
            .await;
        match r {
            Ok(q) => Ok(q.fee_sat),
            // Not funded yet: the shortfall tells us the fee reserve.
            Err(Error::InsufficientFunds { needed, .. }) => Ok(needed.saturating_sub(amount)),
            Err(e) => Err(e),
        }
    }

    /// Price a private Lightning payment of `input` from `from` through the
    /// ecash account `hub`. Nothing moves until [`Wallet::pay_private`].
    pub async fn prepare_private_pay(
        &self,
        input: &str,
        amount_sat: Option<Sats>,
        from: &AccountId,
        hub: &AccountId,
    ) -> Result<PrivatePay> {
        let request = crate::parse::parse(input)?;
        if matches!(request, PaymentRequest::Bolt12 { .. }) {
            return Err(Error::Unsupported("pay a BOLT12 offer privately yet".into()));
        }
        let amount = amount_sat
            .or(request.amount_sat())
            .ok_or(Error::AmountRequired)?;
        let payer = self.account(from).await?;
        let hub_rail = self.account(hub).await?;
        if hub_rail.kind() != RailKind::Ecash {
            return Err(Error::Invalid("Private payments go through an ecash account".into()));
        }
        let invoice = payer.invoice_for(&request, amount).await?;
        let reserve = self.hub_fee(hub, &invoice, amount).await?;
        let fund = amount
            .saturating_add(reserve)
            .saturating_add(input_fee_margin(amount));
        let target = hub_rail
            .receive(ReceiveRequest {
                method: Method::Bolt11,
                amount_sat: Some(fund),
                description: None,
            })
            .await?;
        let hub_quote = target
            .watch_id
            .clone()
            .ok_or_else(|| Error::Invalid("the mint didn't give a quote id".into()))?;
        let leg1 = payer.prepare_send(send_to(&target.request, fund)).await?;
        Ok(PrivatePay {
            from: from.clone(),
            hub: hub.clone(),
            invoice,
            amount_sat: amount,
            fund_sat: fund,
            max_fee_sat: leg1.fee_sat.saturating_add(fund - amount),
            leg1,
            hub_quote,
        })
    }

    /// Run a private payment: fund the hub, wait for the ecash, pay.
    pub async fn pay_private(&self, p: PrivatePay) -> Result<Vec<Payment>> {
        let payer = self.account(&p.from).await?;
        let hub = self.account(&p.hub).await?;
        let mut first = payer.send(p.leg1, None).await?;
        first.direction = Direction::Internal;

        let started = std::time::Instant::now();
        loop {
            match hub.check_incoming(&p.hub_quote).await {
                Ok(true) => break,
                Ok(false) => {}
                Err(e) => tracing::debug!("private pay: mint not ready ({e})"),
            }
            if started.elapsed() > MINT_WAIT {
                return Err(Error::Invalid(
                    "The mint hasn't issued the ecash yet. Your sats are safe: they'll show up in your ecash account, and you can pay from there".into(),
                ));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let q = hub.prepare_send(send_to(&p.invoice, p.amount_sat)).await?;
        let second = hub.send(q, None).await?;
        Ok(vec![first, second])
    }

    /// A private receive's ecash (mint quote `watch_id` in `hub`, for
    /// `amount_sat`) has arrived? Then pay it on to `into`. `Ok(None)` while
    /// it hasn't been paid yet.
    pub async fn forward_private_receive(
        &self,
        hub: &AccountId,
        watch_id: &str,
        into: &AccountId,
        amount_sat: Sats,
    ) -> Result<Option<Payment>> {
        let hub_rail = self.account(hub).await?;
        if !hub_rail.check_incoming(watch_id).await? {
            return Ok(None);
        }
        let mut amount = amount_sat.saturating_sub(reserve_guess(amount_sat));
        for _ in 0..3 {
            if amount == 0 {
                break;
            }
            let target = self
                .receive(into, Method::Bolt11, Some(amount), Some("Private receive".into()))
                .await?;
            match hub_rail.prepare_send(send_to(&target.request, amount)).await {
                Ok(q) => {
                    let mut p = hub_rail.send(q, None).await?;
                    p.direction = Direction::Internal;
                    return Ok(Some(p));
                }
                Err(Error::InsufficientFunds { needed, available }) => {
                    amount = amount.saturating_sub(needed.saturating_sub(available)).saturating_sub(1);
                }
                Err(e) => return Err(e),
            }
        }
        Err(Error::Invalid(
            "Couldn't move the received sats on: they're in your ecash account".into(),
        ))
    }
}

fn send_to(invoice: &str, amount: Sats) -> SendRequest {
    SendRequest {
        destination: PaymentRequest::Bolt11 {
            invoice: invoice.to_string(),
        },
        amount_sat: Some(amount),
        send_max: false,
        fee: Default::default(),
        note: None,
        style: Default::default(),
        coins: None,
    }
}

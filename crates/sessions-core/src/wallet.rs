//! The one object the app talks to.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::planner::{self, Bridges, Leg, Plan};
use crate::rail::{Rail, SwapProvider};
use crate::types::*;
use crate::{parse, Error, Result};

#[derive(Default)]
pub struct Wallet {
    accounts: RwLock<BTreeMap<AccountId, Arc<dyn Rail>>>,
    swaps: RwLock<Vec<Arc<dyn SwapProvider>>>,
}

/// Result of running a plan. Hardware and multisig accounts pause the plan
/// until the user signs on their device.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Execution {
    pub payments: Vec<Payment>,
    /// Set when the next leg needs an external signature.
    pub awaiting_signature: Option<SendQuote>,
    /// Legs still to run after the signature is provided.
    pub remaining: Vec<Leg>,
    pub amount_sat: Sats,
}

impl Wallet {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn add_account(&self, rail: Arc<dyn Rail>) {
        self.accounts.write().await.insert(rail.id().clone(), rail);
    }

    pub async fn remove_account(&self, id: &AccountId) -> Option<Arc<dyn Rail>> {
        self.accounts.write().await.remove(id)
    }

    pub async fn add_swap_provider(&self, p: Arc<dyn SwapProvider>) {
        self.swaps.write().await.push(p);
    }

    pub async fn account(&self, id: &AccountId) -> Result<Arc<dyn Rail>> {
        self.accounts.read().await.get(id).cloned().ok_or_else(|| Error::AccountNotFound(id.0.clone()))
    }

    pub async fn accounts(&self) -> Result<Vec<AccountInfo>> {
        let rails: Vec<_> = self.accounts.read().await.values().cloned().collect();
        let mut out = Vec::with_capacity(rails.len());
        for r in rails {
            out.push(r.info().await?);
        }
        Ok(out)
    }

    /// Sync every account; one failing account doesn't stop the others.
    pub async fn sync_all(&self) -> Vec<(AccountId, Result<()>)> {
        let rails: Vec<_> = self.accounts.read().await.values().cloned().collect();
        let mut out = Vec::new();
        for r in rails {
            out.push((r.id().clone(), r.sync().await));
        }
        for s in self.swaps.read().await.iter() {
            if let Err(e) = s.tick().await {
                tracing::warn!(provider = s.name(), "swap tick failed: {e}");
            }
        }
        out
    }

    pub async fn total_balance(&self) -> Result<Balance> {
        let mut total = Balance::default();
        for a in self.accounts().await? {
            total.confirmed += a.balance.confirmed;
            total.pending_in += a.balance.pending_in;
            total.pending_out += a.balance.pending_out;
        }
        Ok(total)
    }

    pub async fn activity(&self, limit: usize) -> Result<Vec<Payment>> {
        let rails: Vec<_> = self.accounts.read().await.values().cloned().collect();
        let mut all = Vec::new();
        for r in rails {
            all.extend(r.payments(limit).await?);
        }
        all.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        all.truncate(limit);
        Ok(all)
    }

    async fn bridges(&self) -> Vec<Bridges> {
        self.swaps
            .read()
            .await
            .iter()
            .map(|s| Bridges { provider: s.name().to_string(), pay: s.pay_bridges() })
            .collect()
    }

    async fn swap(&self, name: &str) -> Result<Arc<dyn SwapProvider>> {
        self.swaps
            .read()
            .await
            .iter()
            .find(|s| s.name() == name)
            .cloned()
            .ok_or_else(|| Error::Unsupported(format!("use swap provider {name}")))
    }

    /// Plan an outside payment. `from = None` lets the wallet choose the account.
    pub async fn plan_payment(&self, input: &str, amount_sat: Option<Sats>, from: Option<AccountId>) -> Result<Plan> {
        let request = parse::parse(input)?;
        let amount = amount_sat.or(request.amount_sat()).ok_or(Error::AmountRequired)?;
        let accounts = self.accounts().await?;
        let bridges = self.bridges().await;
        match from {
            Some(id) => planner::plan_payment(&accounts, &bridges, &id, &request, amount),
            None => planner::choose_payer(&accounts, &bridges, &request, amount),
        }
    }

    pub async fn plan_move(&self, from: &AccountId, to: &AccountId, amount_sat: Sats) -> Result<Plan> {
        planner::plan_move(&self.accounts().await?, from, to, amount_sat)
    }

    /// Run a plan leg by leg.
    pub async fn execute(&self, plan: Plan, fee: FeePriority) -> Result<Execution> {
        self.run_legs(plan.legs, plan.amount_sat, fee, Vec::new()).await
    }

    /// Continue a paused plan with a PSBT signed on a hardware wallet / co-signers.
    pub async fn resume(&self, paused: Execution, signed_psbt: String, fee: FeePriority) -> Result<Execution> {
        let quote = paused.awaiting_signature.ok_or(Error::Unsupported("resume a plan that isn't paused".into()))?;
        let rail = self.account(&quote.account).await?;
        let mut payments = paused.payments;
        payments.push(rail.send(quote, Some(signed_psbt)).await?);
        self.run_legs(paused.remaining, paused.amount_sat, fee, payments).await
    }

    async fn run_legs(&self, legs: Vec<Leg>, amount: Sats, fee: FeePriority, mut payments: Vec<Payment>) -> Result<Execution> {
        let mut legs = legs.into_iter();
        while let Some(leg) = legs.next() {
            let from = self.account(leg.from()).await?;
            let request = match &leg {
                Leg::ToExternal { request, .. } => request.clone(),
                Leg::ToAccount { to, method, .. } => {
                    let target = self
                        .account(to)
                        .await?
                        .receive(ReceiveRequest { method: *method, amount_sat: Some(amount), description: Some("Sessions Wallet transfer".into()) })
                        .await?;
                    parse::parse(&target.request)?
                }
                Leg::ViaSwap { provider, pay_method, request, .. } => {
                    let refund = if *pay_method == Method::BitcoinAddress {
                        Some(from.receive(ReceiveRequest { method: Method::BitcoinAddress, amount_sat: None, description: None }).await?.request)
                    } else {
                        None
                    };
                    let bridged = self.swap(provider).await?.bridge_payment(request.clone(), amount, *pay_method, refund).await?;
                    bridged.pay
                }
            };

            let quote = from
                .prepare_send(SendRequest { destination: request, amount_sat: Some(amount), send_max: false, fee, note: None })
                .await?;

            if quote.needs_signature.is_some() {
                return Ok(Execution { payments, awaiting_signature: Some(quote), remaining: legs.collect(), amount_sat: amount });
            }
            let mut p = from.send(quote, None).await?;
            if matches!(leg, Leg::ToAccount { .. }) {
                p.direction = Direction::Internal;
            }
            payments.push(p);
        }
        Ok(Execution { payments, awaiting_signature: None, remaining: vec![], amount_sat: amount })
    }

    /// Receive from outside, optionally through a swap (e.g. "give me a Liquid address").
    pub async fn receive(&self, into: &AccountId, method: Method, amount_sat: Option<Sats>, description: Option<String>) -> Result<ReceiveTarget> {
        let rail = self.account(into).await?;
        let info = rail.info().await?;
        if info.receives.contains(&method) {
            return rail.receive(ReceiveRequest { method, amount_sat, description }).await;
        }
        for s in self.swaps.read().await.iter() {
            for (outside, ours) in s.receive_bridges() {
                if outside == method && info.receives.contains(&ours) {
                    let inner = rail.receive(ReceiveRequest { method: ours, amount_sat, description: description.clone() }).await?;
                    return s.bridge_receive(inner, outside).await;
                }
            }
        }
        Err(Error::Unsupported(format!("receive via {method:?}")))
    }

    /// Redeem a bearer token (Cashu). Tries the chosen account, or every ecash
    /// account until one accepts it (each account only accepts its own mint).
    pub async fn redeem(&self, input: &str, into: Option<AccountId>) -> Result<Payment> {
        let request = parse::parse(input)?;
        if let Some(id) = into {
            return self.account(&id).await?.redeem(request).await;
        }
        let rails: Vec<_> = self.accounts.read().await.values().cloned().collect();
        let mut last = Error::NoRoute;
        for r in rails.into_iter().filter(|r| r.kind() == RailKind::Ecash) {
            match r.redeem(request.clone()).await {
                Ok(p) => return Ok(p),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Create a bearer token from an ecash account.
    pub async fn create_token(&self, from: &AccountId, amount_sat: Sats, memo: Option<String>) -> Result<Payment> {
        self.account(from).await?.create_token(amount_sat, memo).await
    }
}

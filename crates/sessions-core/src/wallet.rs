//! The one object the app talks to.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::planner::{self, Bridges, Leg, Plan};
use crate::rail::{Rail, SwapProvider, SwapStatus};
use crate::types::*;
use crate::{parse, Error, Result};

/// Per-account limit for one sync round.
pub const SYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Default)]
pub struct Wallet {
    accounts: RwLock<BTreeMap<AccountId, Arc<dyn Rail>>>,
    swaps: RwLock<Vec<Arc<dyn SwapProvider>>>,
}

/// The first leg of a plan, priced and ready to confirm.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prepared {
    pub quote: SendQuote,
    pub leg: Leg,
    pub remaining: Vec<Leg>,
    /// Extra hops after this one (each may add a small fee).
    pub hops_after: usize,
    pub amount_sat: Sats,
    /// Swap service fee already included in `quote.amount_sat`.
    pub swap_fee_sat: Sats,
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
    /// Sync every account at once, each with its own time limit, so one
    /// slow server can't hold up the rest. Swaps are driven separately by
    /// [`Wallet::tick_swaps`].
    pub async fn sync_all(&self) -> Vec<(AccountId, Result<()>)> {
        self.sync_all_within(SYNC_TIMEOUT).await
    }

    pub async fn sync_all_within(&self, limit: std::time::Duration) -> Vec<(AccountId, Result<()>)> {
        let rails: Vec<_> = self.accounts.read().await.values().cloned().collect();
        let mut set = tokio::task::JoinSet::new();
        for r in rails {
            set.spawn(async move {
                let id = r.id().clone();
                let res = match tokio::time::timeout(limit, r.sync()).await {
                    Ok(res) => res,
                    Err(_) => Err(Error::Rail { rail: id.0.clone(), message: format!("sync took longer than {} s", limit.as_secs()) }),
                };
                (id, res)
            });
        }
        let mut out = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok(pair) = joined {
                out.push(pair);
            }
        }
        out
    }

    /// Drive pending swaps (claims, refunds). Cheap; run every few seconds
    /// while unlocked so hold invoices settle promptly.
    pub async fn tick_swaps(&self) -> Vec<SwapStatus> {
        let mut out = Vec::new();
        for s in self.swaps.read().await.iter() {
            match s.tick().await {
                Ok(v) => out.extend(v),
                Err(e) => tracing::warn!(provider = s.name(), "swap tick failed: {e}"),
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
        all.sort_by_key(|p| std::cmp::Reverse(p.timestamp));
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

    /// Price the first leg of a plan so the user sees the real fee before
    /// confirming. Nothing moves until [`Wallet::confirm`].
    pub async fn prepare(&self, plan: Plan, fee: FeePriority) -> Result<Prepared> {
        let mut legs = plan.legs.into_iter();
        let leg = legs.next().ok_or(Error::NoRoute)?;
        let (quote, swap_fee_sat) = self.prepare_leg(&leg, plan.amount_sat, fee).await?;
        let remaining: Vec<Leg> = legs.collect();
        Ok(Prepared { quote, leg, hops_after: remaining.len(), remaining, amount_sat: plan.amount_sat, swap_fee_sat })
    }

    /// Send a prepared plan. Pauses (returns `awaiting_signature`) when a
    /// hardware wallet or cosigner has to sign.
    pub async fn confirm(&self, prepared: Prepared, fee: FeePriority) -> Result<Execution> {
        if prepared.quote.needs_signature.is_some() {
            // The paused leg is resumed from its quote; only later legs remain.
            return Ok(Execution {
                payments: vec![],
                awaiting_signature: Some(prepared.quote),
                remaining: prepared.remaining,
                amount_sat: prepared.amount_sat,
            });
        }
        let from = self.account(prepared.leg.from()).await?;
        let mut p = from.send(prepared.quote, None).await?;
        if matches!(prepared.leg, Leg::ToAccount { .. }) {
            p.direction = Direction::Internal;
        }
        self.run_legs(prepared.remaining, prepared.amount_sat, fee, vec![p]).await
    }

    /// Prepare and confirm in one go (no fee preview). Used by automations.
    pub async fn execute(&self, plan: Plan, fee: FeePriority) -> Result<Execution> {
        let prepared = self.prepare(plan, fee).await?;
        self.confirm(prepared, fee).await
    }

    /// Continue a paused plan with a PSBT signed on a hardware wallet / co-signers.
    pub async fn resume(&self, paused: Execution, signed_psbt: String, fee: FeePriority) -> Result<Execution> {
        let quote = paused.awaiting_signature.ok_or(Error::Unsupported("resume a plan that isn't paused".into()))?;
        let rail = self.account(&quote.account).await?;
        let mut payments = paused.payments;
        payments.push(rail.send(quote, Some(signed_psbt)).await?);
        self.run_legs(paused.remaining, paused.amount_sat, fee, payments).await
    }

    /// Returns the quote for one leg plus any swap fee on top of it.
    async fn prepare_leg(&self, leg: &Leg, amount: Sats, fee: FeePriority) -> Result<(SendQuote, Sats)> {
        let from = self.account(leg.from()).await?;
        let (request, pay_amount, swap_fee) = match leg {
            Leg::ToExternal { request, .. } => (request.clone(), amount, 0),
            Leg::ToAccount { to, method, .. } => {
                let target = self
                    .account(to)
                    .await?
                    .receive(ReceiveRequest { method: *method, amount_sat: Some(amount), description: Some("Sessions Wallet transfer".into()) })
                    .await?;
                (parse::parse(&target.request)?, amount, 0)
            }
            Leg::ViaSwap { provider, pay_method, request, .. } => {
                let refund = if *pay_method == Method::BitcoinAddress {
                    Some(from.receive(ReceiveRequest { method: Method::BitcoinAddress, amount_sat: None, description: None }).await?.request)
                } else {
                    None
                };
                let bridged = self.swap(provider).await?.bridge_payment(request.clone(), amount, *pay_method, refund).await?;
                // The account pays the swap's amount (target + swap fees), not the target amount.
                (bridged.pay, bridged.amount_sat, bridged.swap_fee_sat)
            }
        };
        let quote = from
            .prepare_send(SendRequest { destination: request, amount_sat: Some(pay_amount), send_max: false, fee, note: None })
            .await?;
        Ok((quote, swap_fee))
    }

    async fn run_legs(&self, legs: Vec<Leg>, amount: Sats, fee: FeePriority, mut payments: Vec<Payment>) -> Result<Execution> {
        let mut legs = legs.into_iter();
        while let Some(leg) = legs.next() {
            let (quote, _) = self.prepare_leg(&leg, amount, fee).await?;
            if quote.needs_signature.is_some() {
                return Ok(Execution { payments, awaiting_signature: Some(quote), remaining: legs.collect(), amount_sat: amount });
            }
            let mut p = self.account(leg.from()).await?.send(quote, None).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rail::BridgedPayment;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// In-memory account that "pays" by moving numbers between mocks.
    struct MockRail {
        id: AccountId,
        kind: RailKind,
        custody: Custody,
        balance: Mutex<Sats>,
        receives: Vec<Method>,
        pays: Vec<Method>,
        fee: Sats,
        /// request string -> receiving account balance cell
        sent: Mutex<Vec<(String, Sats)>>,
    }

    impl MockRail {
        fn new(id: &str, kind: RailKind, bal: Sats, receives: &[Method], pays: &[Method], fee: Sats) -> Arc<Self> {
            Arc::new(Self {
                id: id.into(),
                kind,
                custody: Custody::Hot,
                balance: Mutex::new(bal),
                receives: receives.to_vec(),
                pays: pays.to_vec(),
                fee,
                sent: Mutex::new(vec![]),
            })
        }
    }

    #[async_trait]
    impl Rail for MockRail {
        fn id(&self) -> &AccountId {
            &self.id
        }
        fn kind(&self) -> RailKind {
            self.kind
        }
        async fn info(&self) -> Result<AccountInfo> {
            Ok(AccountInfo {
                id: self.id.clone(),
                name: self.id.0.clone(),
                rail: self.kind,
                custody: self.custody.clone(),
                balance: Balance { confirmed: *self.balance.lock().unwrap(), ..Default::default() },
                receives: self.receives.clone(),
                pays: self.pays.clone(),
            })
        }
        async fn sync(&self) -> Result<()> {
            Ok(())
        }
        async fn receive(&self, req: ReceiveRequest) -> Result<ReceiveTarget> {
            let request = match req.method {
                Method::BitcoinAddress => "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq".to_string(),
                Method::Bolt11 => format!("lnbc1{}xxxxxxxxxxxxxxxxxxxxxxxx", self.id),
                Method::SparkAddress => format!("sp1{}xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", self.id),
                m => return Err(Error::Unsupported(format!("{m:?}"))),
            };
            Ok(ReceiveTarget { method: req.method, request, amount_sat: req.amount_sat, fee_sat: None, expires_at: None, watch_id: None })
        }
        async fn prepare_send(&self, req: SendRequest) -> Result<SendQuote> {
            let amount = req.amount_sat.ok_or(Error::AmountRequired)?;
            let have = *self.balance.lock().unwrap();
            if have < amount + self.fee {
                return Err(Error::InsufficientFunds { needed: amount + self.fee, available: have });
            }
            let method = req.destination.method().unwrap();
            if !self.pays.contains(&method) {
                return Err(Error::Unsupported(format!("pay {method:?}")));
            }
            Ok(SendQuote {
                account: self.id.clone(),
                method,
                amount_sat: amount,
                fee_sat: self.fee,
                handle: serde_json::to_string(&req.destination).unwrap(),
                needs_signature: None,
            })
        }
        async fn send(&self, q: SendQuote, _signed: Option<String>) -> Result<Payment> {
            *self.balance.lock().unwrap() -= q.amount_sat + q.fee_sat;
            self.sent.lock().unwrap().push((q.handle.clone(), q.amount_sat));
            Ok(Payment {
                id: format!("p{}", q.amount_sat),
                account: self.id.clone(),
                direction: Direction::Outgoing,
                amount_sat: q.amount_sat,
                fee_sat: q.fee_sat,
                status: PaymentStatus::Complete,
                method: q.method,
                timestamp: 0,
                description: None,
                token: None,
            })
        }
        async fn payments(&self, _limit: usize) -> Result<Vec<Payment>> {
            Ok(vec![])
        }
    }

    struct MockBoltz;

    #[async_trait]
    impl SwapProvider for MockBoltz {
        fn name(&self) -> &str {
            "boltz"
        }
        fn pay_bridges(&self) -> Vec<(Method, Method)> {
            vec![(Method::LiquidAddress, Method::Bolt11)]
        }
        fn receive_bridges(&self) -> Vec<(Method, Method)> {
            vec![(Method::LiquidAddress, Method::Bolt11)]
        }
        async fn bridge_payment(&self, _t: PaymentRequest, amount: Sats, _m: Method, _r: Option<String>) -> Result<BridgedPayment> {
            Ok(BridgedPayment {
                swap_id: "s1".into(),
                pay: PaymentRequest::Bolt11 { invoice: "lnbc1boltzxxxxxxxxxxxxxxxxxxxx".into() },
                amount_sat: amount + 150,
                swap_fee_sat: 150,
            })
        }
        async fn bridge_receive(&self, ours: ReceiveTarget, _outside: Method) -> Result<ReceiveTarget> {
            Ok(ReceiveTarget { method: Method::LiquidAddress, request: format!("lq1lockup-for-{}", ours.request), ..ours })
        }
        async fn tick(&self) -> Result<Vec<SwapStatus>> {
            Ok(vec![])
        }
    }

    async fn wallet() -> (Wallet, Arc<MockRail>, Arc<MockRail>) {
        use Method::*;
        let w = Wallet::new();
        let savings = MockRail::new("savings", RailKind::Onchain, 1_000_000, &[BitcoinAddress], &[BitcoinAddress], 300);
        let spending = MockRail::new("spending", RailKind::Spark, 50_000, &[SparkAddress, Bolt11, BitcoinAddress], &[SparkAddress, Bolt11, BitcoinAddress], 2);
        w.add_account(savings.clone()).await;
        w.add_account(spending.clone()).await;
        w.add_swap_provider(Arc::new(MockBoltz)).await;
        (w, savings, spending)
    }

    #[tokio::test]
    async fn prepare_shows_fee_without_moving_funds() {
        let (w, savings, spending) = wallet().await;
        let plan = w.plan_move(&"savings".into(), &"spending".into(), 100_000).await.unwrap();
        let prepared = w.prepare(plan, FeePriority::Normal).await.unwrap();
        assert_eq!(prepared.quote.fee_sat, 300);
        assert_eq!(*savings.balance.lock().unwrap(), 1_000_000);

        let ex = w.confirm(prepared, FeePriority::Normal).await.unwrap();
        assert_eq!(ex.payments[0].direction, Direction::Internal);
        assert_eq!(*savings.balance.lock().unwrap(), 1_000_000 - 100_300);
        assert!(spending.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn liquid_payment_pays_swap_amount_including_fee() {
        let (w, _, spending) = wallet().await;
        let plan = w.plan_payment("lq1qqexampleliquidaddress", Some(20_000), Some("spending".into())).await.unwrap();
        let prepared = w.prepare(plan, FeePriority::Normal).await.unwrap();
        assert_eq!(prepared.swap_fee_sat, 150);
        assert_eq!(prepared.quote.amount_sat, 20_150);
        w.confirm(prepared, FeePriority::Normal).await.unwrap();
        assert_eq!(*spending.balance.lock().unwrap(), 50_000 - 20_150 - 2);
    }

    #[tokio::test]
    async fn slow_account_does_not_block_sync() {
        struct Slow;
        #[async_trait]
        impl Rail for Slow {
            fn id(&self) -> &AccountId {
                static ID: std::sync::OnceLock<AccountId> = std::sync::OnceLock::new();
                ID.get_or_init(|| "slow".into())
            }
            fn kind(&self) -> RailKind {
                RailKind::Onchain
            }
            async fn info(&self) -> Result<AccountInfo> {
                unreachable!()
            }
            async fn sync(&self) -> Result<()> {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                Ok(())
            }
            async fn receive(&self, _: ReceiveRequest) -> Result<ReceiveTarget> {
                unreachable!()
            }
            async fn prepare_send(&self, _: SendRequest) -> Result<SendQuote> {
                unreachable!()
            }
            async fn send(&self, _: SendQuote, _: Option<String>) -> Result<Payment> {
                unreachable!()
            }
            async fn payments(&self, _: usize) -> Result<Vec<Payment>> {
                Ok(vec![])
            }
        }
        let (w, _, _) = wallet().await;
        w.add_account(Arc::new(Slow)).await;
        let started = std::time::Instant::now();
        let results = w.sync_all_within(std::time::Duration::from_millis(200)).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(results.len(), 3);
        assert!(results.iter().any(|(id, r)| id.0 == "slow" && r.is_err()));
        assert!(results.iter().filter(|(_, r)| r.is_ok()).count() == 2);
    }

    #[tokio::test]
    async fn receive_liquid_is_bridged_to_lightning() {
        let (w, _, _) = wallet().await;
        let t = w.receive(&"spending".into(), Method::LiquidAddress, Some(5_000), None).await.unwrap();
        assert_eq!(t.method, Method::LiquidAddress);
        assert!(t.request.starts_with("lq1lockup-for-lnbc1spending"));
    }

    #[tokio::test]
    async fn auto_payer_skips_accounts_that_cannot_afford_it() {
        let (w, _, _) = wallet().await;
        let plan = w.plan_payment("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq", Some(200_000), None).await.unwrap();
        assert_eq!(plan.legs[0].from(), &AccountId::from("savings"));
    }

    #[tokio::test]
    async fn insufficient_funds_surfaces_at_prepare() {
        let (w, _, _) = wallet().await;
        let plan = w.plan_move(&"spending".into(), &"savings".into(), 60_000).await.unwrap();
        assert!(matches!(w.prepare(plan, FeePriority::Normal).await, Err(Error::InsufficientFunds { .. })));
    }
}

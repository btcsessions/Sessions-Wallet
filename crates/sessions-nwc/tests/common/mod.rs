#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::{Secp256k1, SecretKey};
use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
use sessions_nwc::*;
use tokio::sync::Notify;

pub const FEE_MSAT: u64 = 1_000;

/// A real signed BOLT11 invoice; returns (invoice, payment hash hex).
pub fn invoice(amount_msat: Option<u64>, seed: u32) -> (String, String) {
    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(&[0x11; 32]).unwrap();
    let hash = sha256::Hash::hash(&seed.to_be_bytes());
    let b = InvoiceBuilder::new(Currency::Bitcoin)
        .description("test".into())
        .payment_hash(hash)
        .payment_secret(PaymentSecret([42; 32]))
        .duration_since_epoch(std::time::Duration::from_secs(sessions_nwc::now()))
        .min_final_cltv_expiry_delta(144);
    let b = match amount_msat {
        Some(a) => b.amount_milli_satoshis(a),
        None => b,
    };
    let inv = b
        .build_signed(|m| secp.sign_ecdsa_recoverable(m, &sk))
        .unwrap();
    (inv.to_string(), hash.to_string())
}

#[derive(Default)]
pub struct Mock {
    pub paid: Mutex<Vec<PayInvoiceRequest>>,
    pub txs: Mutex<HashMap<String, Transaction>>,
    pub fail_next: AtomicBool,
    /// The next payment is sent but doesn't settle in time.
    pub stuck_next: AtomicBool,
    /// When set, payments wait for `gate.notify_one()`.
    pub gate: Option<Arc<Notify>>,
    pub pay_started: AtomicUsize,
    pub last_list: Mutex<Option<ListTransactionsRequest>>,
}

fn tx(kind: TransactionType, hash: &str, amount: u64) -> Transaction {
    Transaction {
        kind,
        state: Some(TransactionState::Settled),
        invoice: None,
        description: None,
        description_hash: None,
        preimage: None,
        payment_hash: hash.to_string(),
        amount_msat: amount,
        fees_paid_msat: 0,
        created_at: 1,
        expires_at: None,
        settled_at: None,
        metadata: None,
    }
}

#[async_trait]
impl NwcWallet for Mock {
    async fn get_info(&self) -> Result<WalletInfo, NwcError> {
        Ok(WalletInfo {
            alias: Some("Sessions".into()),
            network: "mainnet".into(),
            ..Default::default()
        })
    }

    async fn balance_msat(&self) -> Result<u64, NwcError> {
        Ok(21_000_000)
    }

    async fn pay_invoice(&self, req: PayInvoiceRequest) -> Result<PayInvoiceResponse, NwcError> {
        self.pay_started.fetch_add(1, Ordering::SeqCst);
        if let Some(g) = &self.gate {
            g.notified().await;
        }
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(NwcError::payment_failed("no route"));
        }
        if self.stuck_next.swap(false, Ordering::SeqCst) {
            return Err(NwcError::in_flight("still in flight"));
        }
        let preimage = format!("{:0>64}", req.payment_hash.len());
        let mut t = tx(
            TransactionType::Outgoing,
            &req.payment_hash,
            req.total_amount_msat,
        );
        t.preimage = Some(preimage.clone());
        t.fees_paid_msat = FEE_MSAT;
        self.txs.lock().unwrap().insert(req.payment_hash.clone(), t);
        self.paid.lock().unwrap().push(req);
        Ok(PayInvoiceResponse {
            preimage,
            fees_paid_msat: FEE_MSAT,
        })
    }

    async fn make_invoice(&self, req: MakeInvoiceRequest) -> Result<Transaction, NwcError> {
        let n = self.txs.lock().unwrap().len() as u32 + 1000;
        let (inv, hash) = invoice(Some(req.amount_msat), n);
        let mut t = tx(TransactionType::Incoming, &hash, req.amount_msat);
        t.state = Some(TransactionState::Pending);
        t.invoice = Some(inv);
        t.description = req.description;
        self.txs.lock().unwrap().insert(hash, t.clone());
        Ok(t)
    }

    async fn lookup_invoice(
        &self,
        req: LookupInvoiceRequest,
    ) -> Result<Option<Transaction>, NwcError> {
        let hash = req.payment_hash.expect("service fills the hash");
        Ok(self.txs.lock().unwrap().get(&hash).cloned())
    }

    async fn list_transactions(
        &self,
        req: ListTransactionsRequest,
    ) -> Result<TransactionList, NwcError> {
        *self.last_list.lock().unwrap() = Some(req);
        let transactions: Vec<_> = self.txs.lock().unwrap().values().cloned().collect();
        Ok(TransactionList {
            total_count: Some(transactions.len() as u64),
            transactions,
        })
    }
}

/// Collects what the service asks the app to persist.
#[derive(Default)]
pub struct Saved(pub Mutex<Vec<Connection>>);

impl ConnectionStore for Saved {
    fn save(&self, c: &Connection) {
        self.0.lock().unwrap().push(c.clone());
    }
}

impl Saved {
    pub fn last(&self) -> Connection {
        self.0.lock().unwrap().last().unwrap().clone()
    }
}

pub fn new_conn(perms: &[Permission], budget: Budget, now: u64) -> NewConnection {
    Connection::create(
        ConnectionOptions {
            name: "test".into(),
            relays: vec!["wss://relay.example".into()],
            permissions: perms.iter().copied().collect(),
            budget,
            expires_at: None,
            lud16: None,
        },
        now,
    )
    .unwrap()
}

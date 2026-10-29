use std::path::PathBuf;
use std::sync::Arc;

use cdk::wallet::types::{Transaction, TransactionDirection, TransactionStatus};
use cdk::wallet::MintConnector;
use sessions_core::{FeePriority, Rail};

use super::*;
use crate::fake_mint::{fake_invoice, FakeMint, FEE_PAID, FEE_RESERVE};

const MINT: &str = "https://fake.mint";
const OTHER_MINT: &str = "https://other.mint";
const PHRASE_A: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const PHRASE_B: &str =
    "legal winner thank year wave sausage worth useful legal winner thank yellow";

// ---------- mapping ----------

fn tx(
    direction: TransactionDirection,
    method: Option<PaymentMethod>,
    status: TransactionStatus,
) -> Transaction {
    Transaction {
        mint_url: MintUrl::from_str(MINT).unwrap(),
        direction,
        amount: Amount::from(21u64),
        fee: Amount::from(2u64),
        unit: CurrencyUnit::Sat,
        ys: vec![],
        timestamp: 1_700_000_000,
        memo: Some("hi".into()),
        metadata: HashMap::new(),
        quote_id: None,
        payment_request: None,
        payment_proof: None,
        payment_method: method,
        saga_id: None,
        status,
    }
}

#[test]
fn maps_methods_and_statuses() {
    assert_eq!(method_from_cdk(None), Method::CashuToken);
    assert_eq!(
        method_from_cdk(Some(&PaymentMethod::BOLT11)),
        Method::Bolt11
    );
    assert_eq!(
        method_from_cdk(Some(&PaymentMethod::BOLT12)),
        Method::Bolt12
    );
    assert_eq!(
        status_from_cdk(TransactionStatus::Pending),
        PaymentStatus::Pending
    );
    assert_eq!(
        status_from_cdk(TransactionStatus::Completed),
        PaymentStatus::Complete
    );
    assert_eq!(
        status_from_cdk(TransactionStatus::Failed),
        PaymentStatus::Failed
    );
}

#[test]
fn maps_transaction_to_payment() {
    let acct = AccountId::from("e1");
    let mut t = tx(
        TransactionDirection::Outgoing,
        Some(PaymentMethod::BOLT11),
        TransactionStatus::Completed,
    );
    t.quote_id = Some("q1".into());
    let p = payment_from_tx(&acct, &t);
    assert_eq!(p.id, "q1");
    assert_eq!(p.direction, Direction::Outgoing);
    assert_eq!((p.amount_sat, p.fee_sat), (21, 2));
    assert_eq!(p.status, PaymentStatus::Complete);
    assert_eq!(p.method, Method::Bolt11);
    assert_eq!(p.timestamp, 1_700_000_000);
    assert_eq!(p.description.as_deref(), Some("hi"));
    assert!(p.token.is_none());

    // Our own id wins over the quote id; with neither we fall back to CDK's tx id.
    t.metadata.insert(META_PAYMENT_ID.into(), "mine".into());
    assert_eq!(payment_id(&t), "mine");
    let t = tx(
        TransactionDirection::Incoming,
        None,
        TransactionStatus::Pending,
    );
    let p = payment_from_tx(&acct, &t);
    assert_eq!(p.id, t.id().to_string());
    assert_eq!(p.direction, Direction::Incoming);
    assert_eq!(p.method, Method::CashuToken);
    assert_eq!(p.status, PaymentStatus::Pending);
}

#[test]
fn bolt11_destination_selection() {
    let inv = fake_invoice(10, 1);
    assert_eq!(
        bolt11_of(&PaymentRequest::Bolt11 {
            invoice: inv.clone()
        })
        .unwrap(),
        inv
    );
    let unified = PaymentRequest::Unified {
        options: vec![
            PaymentRequest::BitcoinAddress {
                address: "bc1q...".into(),
                amount_sat: None,
                label: None,
                payjoin: None,
            },
            PaymentRequest::Bolt11 {
                invoice: inv.clone(),
            },
        ],
    };
    assert_eq!(bolt11_of(&unified).unwrap(), inv);
    let addr = PaymentRequest::BitcoinAddress {
        address: "bc1q...".into(),
        amount_sat: None,
        label: None,
        payjoin: None,
    };
    assert!(matches!(bolt11_of(&addr), Err(Error::Unsupported(_))));
}

// ---------- end to end against an in-process fake mint ----------

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p =
            std::env::temp_dir().join(format!("rail-ecash-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn open(
    dir: &TempDir,
    id: &str,
    mint_url: &str,
    phrase: &str,
    mint: &Arc<FakeMint>,
) -> EcashAccount {
    let client: Arc<dyn MintConnector + Send + Sync> = mint.clone();
    EcashAccount::open_inner(
        EcashConfig {
            id: AccountId::from(id),
            name: format!("Ecash {id}"),
            mint_url: mint_url.into(),
            mnemonic: phrase.into(),
            db_path: dir.0.join(format!("{id}.redb")),
        },
        Some(client),
    )
    .await
    .unwrap()
}

fn bolt11_send(invoice: String) -> SendRequest {
    SendRequest {
        destination: PaymentRequest::Bolt11 { invoice },
        amount_sat: None,
        send_max: false,
        fee: FeePriority::default(),
        note: None,
        style: Default::default(),
        coins: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mint_token_redeem_melt_end_to_end() {
    let dir = TempDir::new("e2e");
    let mint = Arc::new(FakeMint::new(1));
    let alice = open(&dir, "alice", MINT, PHRASE_A, &mint).await;
    let bob = open(&dir, "bob", MINT, PHRASE_B, &mint).await;

    // --- info basics
    let info = alice.info().await.unwrap();
    assert_eq!(alice.kind(), RailKind::Ecash);
    assert_eq!(info.custody, Custody::Mint { url: MINT.into() });
    assert_eq!(info.balance, Balance::default());
    assert_eq!(
        (info.receives, info.pays),
        (vec![Method::Bolt11], vec![Method::Bolt11])
    );

    // --- receive needs an amount, and only over BOLT11
    let req = |method, amount_sat| ReceiveRequest {
        method,
        amount_sat,
        description: None,
    };
    assert!(matches!(
        alice.receive(req(Method::Bolt11, None)).await,
        Err(Error::AmountRequired)
    ));
    assert!(matches!(
        alice.receive(req(Method::CashuToken, Some(5))).await,
        Err(Error::Unsupported(_))
    ));

    // --- Lightning in: quote, unpaid sync is a no-op, pay, sync mints
    let target = alice
        .receive(ReceiveRequest {
            method: Method::Bolt11,
            amount_sat: Some(100),
            description: Some("top up".into()),
        })
        .await
        .unwrap();
    assert!(target.request.starts_with("lnbc"));
    assert_eq!(target.amount_sat, Some(100));
    let quote_id = target.watch_id.clone().unwrap();

    alice.sync().await.unwrap();
    assert_eq!(alice.info().await.unwrap().balance.confirmed, 0);

    mint.pay_mint_quote(&quote_id);
    alice.sync().await.unwrap();
    assert_eq!(alice.info().await.unwrap().balance.confirmed, 100);

    let history = alice.payments(10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, quote_id);
    assert_eq!(
        (history[0].direction, history[0].method),
        (Direction::Incoming, Method::Bolt11)
    );
    assert_eq!(
        (history[0].amount_sat, history[0].status),
        (100, PaymentStatus::Complete)
    );

    // --- token out
    let sent = alice
        .create_token(30, Some("for bob".into()))
        .await
        .unwrap();
    let token = sent.token.clone().unwrap();
    assert!(token.starts_with("cashu"));
    assert_eq!(
        (sent.direction, sent.method, sent.amount_sat),
        (Direction::Outgoing, Method::CashuToken, 30)
    );
    assert_eq!(sent.status, PaymentStatus::Pending);
    let info = alice.info().await.unwrap();
    assert_eq!(info.balance.confirmed, 70);
    assert_eq!(
        info.balance.pending_out, 30,
        "unclaimed token counts as pending out"
    );
    let hist = alice.payments(10).await.unwrap();
    let out = hist
        .iter()
        .find(|p| p.id == sent.id)
        .expect("token send in history under the same id");
    assert_eq!(out.status, PaymentStatus::Pending);

    // --- a token from another mint is refused without touching it
    let carol = open(&dir, "carol", OTHER_MINT, PHRASE_B, &mint).await;
    match carol
        .redeem(PaymentRequest::CashuToken {
            token: token.clone(),
        })
        .await
    {
        Err(Error::Unsupported(msg)) => {
            assert_eq!(msg, format!("token is from another mint: {MINT}"))
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }

    // --- bob redeems it
    let got = bob
        .redeem(PaymentRequest::CashuToken {
            token: token.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        (got.direction, got.method, got.amount_sat, got.fee_sat),
        (Direction::Incoming, Method::CashuToken, 30, 0)
    );
    assert_eq!(got.description.as_deref(), Some("for bob"));
    assert_eq!(bob.info().await.unwrap().balance.confirmed, 30);
    let bob_hist = bob.payments(10).await.unwrap();
    assert_eq!(bob_hist.len(), 1);
    assert_eq!(bob_hist[0].id, got.id);

    // double redeem fails
    assert!(bob
        .redeem(PaymentRequest::CashuToken {
            token: token.clone()
        })
        .await
        .is_err());
    assert_eq!(bob.info().await.unwrap().balance.confirmed, 30);

    // --- alice's sync notices the token was claimed
    alice.sync().await.unwrap();
    let info = alice.info().await.unwrap();
    assert_eq!((info.balance.confirmed, info.balance.pending_out), (70, 0));
    let out = alice
        .payments(10)
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.id == sent.id)
        .unwrap();
    assert_eq!(out.status, PaymentStatus::Complete);

    // --- Lightning out
    let addr = SendRequest {
        destination: PaymentRequest::BitcoinAddress {
            address: "bc1qxyz".into(),
            amount_sat: Some(5),
            label: None,
            payjoin: None,
        },
        ..bolt11_send(String::new())
    };
    assert!(matches!(
        alice.prepare_send(addr).await,
        Err(Error::Unsupported(_))
    ));

    let too_big = alice.prepare_send(bolt11_send(fake_invoice(500, 2))).await;
    assert!(
        matches!(too_big, Err(Error::InsufficientFunds { needed, available: 70 }) if needed == 500 + FEE_RESERVE),
        "{too_big:?}"
    );

    let invoice = fake_invoice(20, 3);
    let quote = alice
        .prepare_send(bolt11_send(invoice.clone()))
        .await
        .unwrap();
    assert_eq!(
        (quote.amount_sat, quote.fee_sat, quote.method),
        (20, FEE_RESERVE, Method::Bolt11)
    );
    let paid = alice.send(quote.clone(), None).await.unwrap();
    assert_eq!(paid.id, quote.handle);
    assert_eq!(
        (paid.amount_sat, paid.fee_sat, paid.status),
        (20, FEE_PAID, PaymentStatus::Complete)
    );
    assert_eq!(mint.paid_invoices.lock().unwrap().as_slice(), &[invoice]);
    assert_eq!(
        alice.info().await.unwrap().balance.confirmed,
        70 - 20 - FEE_PAID,
        "unused fee reserve came back as change"
    );

    let melt = alice
        .payments(10)
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.id == quote.handle)
        .unwrap();
    assert_eq!(
        (melt.direction, melt.method, melt.status),
        (Direction::Outgoing, Method::Bolt11, PaymentStatus::Complete)
    );

    // --- spending more than we have as a token
    let r = alice.create_token(1_000, None).await;
    assert!(
        matches!(
            r,
            Err(Error::InsufficientFunds {
                needed: 1_000,
                available: 49
            })
        ),
        "{r:?}"
    );

    // --- state survives a restart (redb file)
    drop(alice);
    let alice = open(&dir, "alice", MINT, PHRASE_A, &mint).await;
    assert_eq!(alice.info().await.unwrap().balance.confirmed, 49);
    assert_eq!(alice.payments(10).await.unwrap().len(), 3);
    assert_eq!(alice.payments(1).await.unwrap().len(), 1);
}

#[tokio::test]
async fn rejects_bad_config() {
    let dir = TempDir::new("cfg");
    let cfg = |mint_url: &str, mnemonic: &str| EcashConfig {
        id: AccountId::from("x"),
        name: "x".into(),
        mint_url: mint_url.into(),
        mnemonic: mnemonic.into(),
        db_path: dir.0.join("x.redb"),
    };
    assert!(matches!(
        EcashAccount::open(cfg(MINT, "not a phrase")).await,
        Err(Error::InvalidMnemonic(_))
    ));
    assert!(EcashAccount::open(cfg("", PHRASE_A)).await.is_err());
    // Opening without network works (nothing is fetched until sync/receive/send).
    let acct = EcashAccount::open(cfg("https://Mint.Example.com/", PHRASE_A))
        .await
        .unwrap();
    assert_eq!(acct.mint_url(), "https://mint.example.com");
}

// ---------- private Lightning through an ecash hub ----------

/// Stands in for Spending: pays invoices by paying the fake mint's quotes,
/// hands out fake invoices, resolves "addresses" to fake invoices.
struct FakeSpending {
    id: AccountId,
    mint: Arc<FakeMint>,
    tag: std::sync::atomic::AtomicU64,
}

#[async_trait::async_trait]
impl Rail for FakeSpending {
    fn id(&self) -> &AccountId {
        &self.id
    }
    fn kind(&self) -> RailKind {
        RailKind::Spark
    }
    async fn info(&self) -> Result<AccountInfo> {
        Ok(AccountInfo {
            id: self.id.clone(),
            name: "Spending".into(),
            rail: RailKind::Spark,
            custody: Custody::Hot,
            balance: Balance::default(),
            receives: vec![Method::Bolt11],
            pays: vec![Method::Bolt11],
            group: None,
            account_index: None,
            scanning: false,
            scan_error: None,
            scan_status: None,
        })
    }
    async fn sync(&self) -> Result<()> {
        Ok(())
    }
    async fn receive(&self, req: ReceiveRequest) -> Result<ReceiveTarget> {
        let n = self.tag.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ReceiveTarget {
            method: Method::Bolt11,
            request: fake_invoice(req.amount_sat.unwrap(), 1000 + n),
            amount_sat: req.amount_sat,
            fee_sat: None,
            expires_at: None,
            watch_id: None,
        })
    }
    async fn invoice_for(&self, _r: &PaymentRequest, amount: Sats) -> Result<String> {
        Ok(fake_invoice(amount, 77))
    }
    async fn prepare_send(&self, req: SendRequest) -> Result<SendQuote> {
        let PaymentRequest::Bolt11 { invoice } = req.destination else { panic!("bolt11") };
        Ok(SendQuote {
            account: self.id.clone(),
            method: Method::Bolt11,
            amount_sat: req.amount_sat.unwrap(),
            fee_sat: 1,
            handle: invoice,
            needs_signature: None,
            private: false,
            payjoin: false,
            notes: vec![],
        })
    }
    async fn send(&self, quote: SendQuote, _: Option<String>) -> Result<Payment> {
        assert!(self.mint.pay_invoice(&quote.handle), "paid an invoice that isn't the mint's");
        Ok(Payment {
            id: "p1".into(),
            account: self.id.clone(),
            direction: Direction::Outgoing,
            amount_sat: quote.amount_sat,
            fee_sat: 1,
            status: PaymentStatus::Complete,
            method: Method::Bolt11,
            timestamp: 0,
            description: None,
            token: None,
        })
    }
    async fn payments(&self, _: usize) -> Result<Vec<Payment>> {
        Ok(vec![])
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_send_and_receive_hop_through_ecash() {
    let dir = TempDir::new("private");
    let mint = Arc::new(FakeMint::new(3));
    let hub = Arc::new(open(&dir, "hub", MINT, PHRASE_A, &mint).await);
    let spending = Arc::new(FakeSpending {
        id: AccountId::from("spending"),
        mint: mint.clone(),
        tag: Default::default(),
    });
    let w = sessions_core::Wallet::new();
    w.add_account(hub.clone()).await;
    w.add_account(spending.clone()).await;

    // --- send 1,000 sats to "alice@example.com", privately
    let p = w
        .prepare_private_pay("alice@example.com", Some(1_000), &"spending".into(), &"hub".into())
        .await
        .unwrap();
    assert_eq!(p.amount_sat, 1_000);
    assert!(p.fund_sat >= 1_000 + FEE_RESERVE, "{}", p.fund_sat);
    // The recipient's invoice (same payment hash; the timestamp may differ).
    let hash = |i: &str| *i.parse::<cdk::lightning_invoice::Bolt11Invoice>().unwrap().payment_hash();
    assert_eq!(hash(&p.invoice), hash(&fake_invoice(1_000, 77)));
    let paid = w.pay_private(p.clone()).await.unwrap();
    assert_eq!(paid.len(), 2);
    assert_eq!(paid[0].direction, Direction::Internal);
    // The mint paid the recipient's invoice, not ours.
    assert!(mint.paid_invoices.lock().unwrap().contains(&p.invoice));
    // Unused reserve and margin stay in the hub as change.
    let left = hub.info().await.unwrap().balance.confirmed;
    assert_eq!(left, p.fund_sat - 1_000 - FEE_PAID);

    // --- receive 5,000 privately into Spending
    let target = hub
        .receive(ReceiveRequest { method: Method::Bolt11, amount_sat: Some(5_000), description: None })
        .await
        .unwrap();
    let quote = target.watch_id.unwrap();
    // Not paid yet: nothing to forward.
    assert!(w
        .forward_private_receive(&"hub".into(), &quote, &"spending".into(), 5_000)
        .await
        .unwrap()
        .is_none());
    mint.pay_mint_quote(&quote);
    let fwd = w
        .forward_private_receive(&"hub".into(), &quote, &"spending".into(), 5_000)
        .await
        .unwrap()
        .expect("forwarded");
    assert_eq!(fwd.direction, Direction::Internal);
    assert!(fwd.amount_sat >= 5_000 - 60 && fwd.amount_sat < 5_000, "{}", fwd.amount_sat);
    // Most of it moved on; only fee change is left behind.
    let after = hub.info().await.unwrap().balance.confirmed;
    assert!(after < left + 60, "{after}");
}

#[tokio::test]
async fn a_damaged_database_is_set_aside_and_rebuilt() {
    let dir = TempDir::new("damaged");
    let mint = Arc::new(FakeMint::new(9));
    drop(open(&dir, "e", MINT, PHRASE_A, &mint).await);
    let path = dir.0.join("e.redb");
    // Scribble over everything after the header.
    let mut bytes = std::fs::read(&path).unwrap();
    for (i, b) in bytes.iter_mut().enumerate().skip(512) {
        *b = (i * 7 % 251) as u8;
    }
    std::fs::write(&path, &bytes).unwrap();
    let a = open(&dir, "e", MINT, PHRASE_A, &mint).await;
    assert_eq!(a.info().await.unwrap().balance.confirmed, 0);
    let aside = std::fs::read_dir(&dir.0)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().contains("damaged-"));
    assert!(aside, "the damaged file is kept");
}

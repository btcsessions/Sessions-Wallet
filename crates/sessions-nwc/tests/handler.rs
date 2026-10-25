mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use common::*;
use serde_json::{json, Value};
use sessions_nwc::client::Client;
use sessions_nwc::*;

const T0: u64 = 1_790_000_000;
const ALL: &[Permission] = &Permission::ALL;

struct Setup {
    svc: Arc<WalletService<Mock>>,
    wallet: Arc<Mock>,
    saved: Arc<Saved>,
    client: Client,
    conn: Connection,
}

fn setup_with(perms: &[Permission], budget: Budget, mock: Mock) -> Setup {
    let n = new_conn(perms, budget, T0);
    let wallet = Arc::new(mock);
    let saved = Arc::new(Saved::default());
    let svc = Arc::new(WalletService::new(wallet.clone(), saved.clone()));
    assert!(svc.set_connections(vec![n.connection.clone()]).is_empty());
    Setup {
        svc,
        wallet,
        saved,
        client: Client::from_uri(&n.uri).unwrap(),
        conn: n.connection,
    }
}

fn setup(perms: &[Permission], budget: Budget) -> Setup {
    setup_with(perms, budget, Mock::default())
}

impl Setup {
    async fn call_at(
        &self,
        method: &str,
        params: Value,
        enc: Option<Encryption>,
        now: u64,
    ) -> ResponseBody {
        let ev = self
            .client
            .request(method, params, enc, now, vec![])
            .unwrap();
        let out = self.svc.handle_event_at(&ev, now).await;
        let resp = out.response().expect("a response").clone();
        assert_eq!(resp.kind, KIND_RESPONSE);
        assert_eq!(resp.pubkey, self.conn.wallet_pubkey);
        assert_eq!(resp.tag("p").unwrap()[0], self.client.pubkey_hex());
        assert_eq!(resp.tag("e").unwrap()[0], ev.id);
        assert!(resp.verify());
        // Answered with the request's scheme.
        let nip04 = resp.content.contains("?iv=");
        assert_eq!(nip04, enc.is_none() || enc == Some(Encryption::Nip04));
        self.client.response(&resp).unwrap()
    }

    async fn call(&self, method: &str, params: Value) -> ResponseBody {
        self.call_at(method, params, Some(Encryption::Nip44V2), T0 + 10)
            .await
    }
}

fn ok(r: &ResponseBody) -> &Value {
    assert!(r.error.is_none(), "unexpected error {:?}", r.error);
    r.result.as_ref().unwrap()
}

fn err(r: &ResponseBody) -> ErrorCode {
    assert!(r.result.is_none());
    r.error.as_ref().expect("an error").code
}

#[tokio::test]
async fn get_info_over_nip44_nip04_and_legacy() {
    let s = setup(ALL, Budget::default());
    for enc in [Some(Encryption::Nip44V2), Some(Encryption::Nip04), None] {
        let r = s.call_at("get_info", json!({}), enc, T0 + 5).await;
        assert_eq!(r.result_type, "get_info");
        let v = ok(&r);
        assert_eq!(v["alias"], "Sessions");
        assert_eq!(v["network"], "mainnet");
        assert_eq!(
            v["methods"],
            json!([
                "pay_invoice",
                "get_balance",
                "get_budget",
                "get_info",
                "make_invoice",
                "lookup_invoice",
                "list_transactions"
            ])
        );
        assert_eq!(
            v["notifications"],
            json!(["payment_received", "payment_sent"])
        );
        assert_eq!(v["extensions"], json!(["02", "05", "13"]));
    }
    // Error and result can't both be set; success has error: null.
    let raw = s.call("get_balance", json!({})).await;
    assert_eq!(ok(&raw)["balance"], 21_000_000);
    // last_used_at was persisted.
    assert_eq!(s.saved.last().last_used_at, Some(T0 + 10));
}

#[tokio::test]
async fn nip44_without_tag_is_understood() {
    // Some clients send NIP-44 without the `encryption` tag.
    let s = setup(ALL, Budget::default());
    let mut ev = s
        .client
        .request(
            "get_balance",
            json!({}),
            Some(Encryption::Nip44V2),
            T0,
            vec![],
        )
        .unwrap();
    ev.tags.retain(|t| t[0] != "encryption");
    let ev =
        sessions_backup::nostr::Event::sign(&client_key(&s), T0, KIND_REQUEST, ev.tags, ev.content);
    let out = s.svc.handle_event_at(&ev, T0).await;
    let resp = out.response().unwrap();
    assert!(!resp.content.contains("?iv="));
    assert_eq!(ok(&s.client.response(resp).unwrap())["balance"], 21_000_000);
}

#[tokio::test]
async fn unsupported_encryption() {
    let s = setup(ALL, Budget::default());
    let mut ev = s
        .client
        .request("get_info", json!({}), Some(Encryption::Nip44V2), T0, vec![])
        .unwrap();
    // A client asking for a scheme we don't know.
    for t in ev.tags.iter_mut() {
        if t[0] == "encryption" {
            t[1] = "nip44_v3".into();
        }
    }
    let k = client_key(&s);
    let ev = sessions_backup::nostr::Event::sign(&k, T0, KIND_REQUEST, ev.tags, ev.content);
    let out = s.svc.handle_event_at(&ev, T0).await;
    let r = s.client.response(out.response().unwrap()).unwrap();
    assert_eq!(err(&r), ErrorCode::UnsupportedEncryption);
}

fn client_key(s: &Setup) -> sessions_backup::keys::NostrKey {
    let bytes: Vec<u8> = bitcoin::hex::FromHex::from_hex(s.client.uri.secret.expose()).unwrap();
    sessions_backup::keys::NostrKey::from_secret_bytes(&bytes.try_into().unwrap()).unwrap()
}

#[tokio::test]
async fn ignores_strangers_forgeries_expired_and_duplicates() {
    let s = setup(ALL, Budget::default());
    let other = setup(ALL, Budget::default());

    // Another connection's client writing to our wallet key.
    let mut ev = other
        .client
        .request("get_info", json!({}), Some(Encryption::Nip44V2), T0, vec![])
        .unwrap();
    ev.tags[0][1] = s.conn.wallet_pubkey.clone();
    let k = client_key(&other);
    let ev = sessions_backup::nostr::Event::sign(&k, T0, KIND_REQUEST, ev.tags, ev.content);
    assert!(matches!(
        s.svc.handle_event_at(&ev, T0).await,
        Outcome::Ignored(Ignored::WrongAuthor)
    ));

    // Not one of our wallet keys.
    let ev = other
        .client
        .request("get_info", json!({}), None, T0, vec![])
        .unwrap();
    assert!(matches!(
        s.svc.handle_event_at(&ev, T0).await,
        Outcome::Ignored(Ignored::UnknownWallet)
    ));

    // Tampered content.
    let mut ev = s
        .client
        .request("get_info", json!({}), None, T0, vec![])
        .unwrap();
    ev.content.push('x');
    assert!(matches!(
        s.svc.handle_event_at(&ev, T0).await,
        Outcome::Ignored(Ignored::BadSignature)
    ));

    // Expired request; one that hasn't expired yet goes through.
    let exp = vec![vec!["expiration".to_string(), (T0 + 30).to_string()]];
    let ev = s
        .client
        .request("get_info", json!({}), None, T0, exp.clone())
        .unwrap();
    assert!(matches!(
        s.svc.handle_event_at(&ev, T0 + 31).await,
        Outcome::Ignored(Ignored::Expired)
    ));
    let ev = s
        .client
        .request("get_balance", json!({}), None, T0 + 1, exp)
        .unwrap();
    assert!(s
        .svc
        .handle_event_at(&ev, T0 + 29)
        .await
        .response()
        .is_some());

    // The same event again (from a second relay, or after a reconnect).
    assert!(matches!(
        s.svc.handle_event_at(&ev, T0 + 29).await,
        Outcome::Ignored(Ignored::Duplicate)
    ));

    // Wrong kind.
    let mut ev = ev.clone();
    ev.kind = KIND_RESPONSE;
    assert!(matches!(
        s.svc.handle_event_at(&ev, T0).await,
        Outcome::Ignored(Ignored::WrongKind)
    ));
}

#[tokio::test]
async fn permissions_and_unsupported_methods() {
    let s = setup(&[Permission::Receive], Budget::default());
    let (inv, _) = invoice(Some(10_000), 1);
    assert_eq!(
        err(&s.call("pay_invoice", json!({ "invoice": inv })).await),
        ErrorCode::Restricted
    );
    assert_eq!(
        err(&s.call("get_balance", json!({})).await),
        ErrorCode::Restricted
    );
    assert_eq!(
        err(&s.call("get_budget", json!({})).await),
        ErrorCode::Restricted
    );
    assert_eq!(
        err(&s.call("list_transactions", json!({})).await),
        ErrorCode::Restricted
    );
    for m in [
        "pay_keysend",
        "multi_pay_invoice",
        "multi_pay_keysend",
        "sign_message",
        "bogus",
    ] {
        let r = s.call(m, json!({})).await;
        assert_eq!(r.result_type, m);
        assert_eq!(err(&r), ErrorCode::NotImplemented);
    }
    let v = ok(&s.call("get_info", json!({})).await).clone();
    assert_eq!(
        v["methods"],
        json!(["get_info", "make_invoice", "lookup_invoice"])
    );
    assert!(v.get("notifications").is_none());
    assert!(s.wallet.paid.lock().unwrap().is_empty());

    // Garbage JSON / missing params.
    assert_eq!(
        err(&s
            .call("make_invoice", json!({ "description": "no amount" }))
            .await),
        ErrorCode::Other
    );
}

#[tokio::test]
async fn disabled_and_expired_connections() {
    let s = setup(ALL, Budget::default());
    let mut c = s.conn.clone();
    c.enabled = false;
    s.svc.set_connections(vec![c.clone()]);
    assert_eq!(
        err(&s.call("get_info", json!({})).await),
        ErrorCode::Unauthorized
    );
    c.enabled = true;
    c.expires_at = Some(T0 + 5);
    s.svc.set_connections(vec![c]);
    assert_eq!(
        err(&s.call("get_info", json!({})).await),
        ErrorCode::Unauthorized
    );
    // Neither is listened for or advertised.
    assert!(s.svc.subscriptions(T0 + 10).is_empty());
    assert!(s.svc.info_events(T0 + 10).is_empty());
    assert_eq!(s.svc.subscriptions(T0).len(), 1);
}

#[tokio::test]
async fn budget_caps_and_rollover() {
    let s = setup(
        ALL,
        Budget {
            max_per_payment_sat: Some(4_000),
            limit_sat: Some(5_000),
            period: BudgetPeriod::Daily,
        },
    );
    let pay = |seed: u32, sat: u64| {
        let (inv, _) = invoice(Some(sat * 1000), seed);
        json!({ "invoice": inv })
    };
    // Over the per-payment cap.
    assert_eq!(
        err(&s.call("pay_invoice", pay(1, 4_001)).await),
        ErrorCode::QuotaExceeded
    );
    // 3,000 sats + 1 sat fee.
    let r = s.call("pay_invoice", pay(2, 3_000)).await;
    assert_eq!(ok(&r)["fees_paid"], FEE_MSAT);
    assert!(ok(&r)["preimage"].is_string());
    assert_eq!(s.saved.last().usage.spent_msat, 3_001_000);
    // 3,001 + 2,000 > 5,000.
    assert_eq!(
        err(&s.call("pay_invoice", pay(3, 2_000)).await),
        ErrorCode::QuotaExceeded
    );
    // Exactly to the limit is fine.
    ok(&s.call("pay_invoice", pay(4, 1_999)).await);
    let b = ok(&s.call("get_budget", json!({})).await).clone();
    assert_eq!(b["used_budget"], 5_001_000);
    assert_eq!(b["total_budget"], 5_000_000);
    assert_eq!(b["renewal_period"], "daily");
    assert_eq!(b["renews_at"], T0 + 86_400);
    assert_eq!(
        err(&s.call("pay_invoice", pay(5, 1)).await),
        ErrorCode::QuotaExceeded
    );

    // Next day: a fresh budget.
    let next = T0 + 86_400 + 1;
    let r = s
        .call_at(
            "pay_invoice",
            pay(6, 3_000),
            Some(Encryption::Nip44V2),
            next,
        )
        .await;
    ok(&r);
    let saved = s.saved.last();
    assert_eq!(saved.usage.spent_msat, 3_001_000);
    assert_eq!(saved.usage.period_start, T0 + 86_400);
    assert_eq!(s.wallet.paid.lock().unwrap().len(), 3);

    // The app's reset.
    assert_eq!(s.svc.reset_usage(&s.conn.id).unwrap().usage.spent_msat, 0);
    assert_eq!(s.saved.last().usage.spent_msat, 0);
}

#[tokio::test]
async fn no_budget_means_empty_get_budget() {
    let s = setup(ALL, Budget::default());
    assert_eq!(ok(&s.call("get_budget", json!({})).await), &json!({}));
    let t = setup(
        ALL,
        Budget {
            limit_sat: Some(0),
            period: BudgetPeriod::Total,
            ..Default::default()
        },
    );
    let b = ok(&t.call("get_budget", json!({})).await).clone();
    assert_eq!(
        b,
        json!({ "used_budget": 0, "total_budget": 0, "renewal_period": "never" })
    );
    let (inv, _) = invoice(Some(1_000), 9);
    assert_eq!(
        err(&t.call("pay_invoice", json!({ "invoice": inv })).await),
        ErrorCode::QuotaExceeded
    );
}

#[tokio::test]
async fn failed_payment_releases_the_reservation() {
    let s = setup(
        ALL,
        Budget {
            limit_sat: Some(10_000),
            ..Default::default()
        },
    );
    s.wallet.fail_next.store(true, Ordering::SeqCst);
    let (inv, _) = invoice(Some(8_000_000), 1);
    assert_eq!(
        err(&s.call("pay_invoice", json!({ "invoice": inv })).await),
        ErrorCode::PaymentFailed
    );
    assert_eq!(ok(&s.call("get_budget", json!({})).await)["used_budget"], 0);
    // And the same invoice can be retried.
    ok(&s.call("pay_invoice", json!({ "invoice": inv })).await);
    assert_eq!(
        ok(&s.call("get_budget", json!({})).await)["used_budget"],
        8_001_000
    );
}

#[tokio::test]
async fn in_flight_payments_count_against_the_budget() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let s = Arc::new(setup_with(
        ALL,
        Budget {
            limit_sat: Some(10_000),
            ..Default::default()
        },
        Mock {
            gate: Some(gate.clone()),
            ..Default::default()
        },
    ));
    let (a, _) = invoice(Some(6_000_000), 1);
    let (b, _) = invoice(Some(6_000_000), 2);
    let first = {
        let s = s.clone();
        tokio::spawn(async move { s.call("pay_invoice", json!({ "invoice": a })).await })
    };
    while s.wallet.pay_started.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    // While the first is in flight, its amount is reserved.
    assert_eq!(
        ok(&s.call("get_budget", json!({})).await)["used_budget"],
        6_000_000
    );
    assert_eq!(
        err(&s.call("pay_invoice", json!({ "invoice": b })).await),
        ErrorCode::QuotaExceeded
    );
    gate.notify_one();
    ok(&first.await.unwrap());
    assert_eq!(
        ok(&s.call("get_budget", json!({})).await)["used_budget"],
        6_001_000
    );
    assert_eq!(s.wallet.pay_started.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn amountless_invoices_and_mismatches() {
    let s = setup(ALL, Budget::default());
    let (inv, hash) = invoice(None, 1);
    assert_eq!(
        err(&s.call("pay_invoice", json!({ "invoice": inv })).await),
        ErrorCode::Other
    );
    ok(&s
        .call("pay_invoice", json!({ "invoice": inv, "amount": 5_000 }))
        .await);
    let paid = s.wallet.paid.lock().unwrap().last().unwrap().clone();
    assert_eq!(paid.amount_msat, Some(5_000));
    assert_eq!(paid.total_amount_msat, 5_000);
    assert_eq!(paid.payment_hash, hash);

    let (inv, _) = invoice(Some(7_000), 2);
    assert_eq!(
        err(&s
            .call("pay_invoice", json!({ "invoice": inv, "amount": 9_000 }))
            .await),
        ErrorCode::Other
    );
    // Same amount restated is fine; the wallet gets no override.
    ok(&s
        .call("pay_invoice", json!({ "invoice": inv, "amount": 7_000 }))
        .await);
    assert_eq!(
        s.wallet.paid.lock().unwrap().last().unwrap().amount_msat,
        None
    );

    assert_eq!(
        err(&s
            .call("pay_invoice", json!({ "invoice": "lnbc1garbage" }))
            .await),
        ErrorCode::Other
    );
}

#[tokio::test]
async fn replayed_payment_is_not_paid_twice() {
    let s = setup(
        ALL,
        Budget {
            limit_sat: Some(100),
            ..Default::default()
        },
    );
    let (inv, _) = invoice(Some(50_000), 1);
    let first = ok(&s.call("pay_invoice", json!({ "invoice": inv })).await).clone();
    // A new request (new event id) for the same invoice.
    let again = s
        .call_at(
            "pay_invoice",
            json!({ "invoice": inv }),
            Some(Encryption::Nip04),
            T0 + 20,
        )
        .await;
    assert_eq!(ok(&again)["preimage"], first["preimage"]);
    assert_eq!(s.wallet.paid.lock().unwrap().len(), 1);
    assert_eq!(s.saved.last().usage.spent_msat, 51_000);
}

#[tokio::test]
async fn lookup_visibility() {
    let recv = setup(&[Permission::Receive], Budget::default());
    let made = ok(&recv
        .call(
            "make_invoice",
            json!({ "amount": 21_000, "description": "zap" }),
        )
        .await)
    .clone();
    assert_eq!(made["type"], "incoming");
    assert_eq!(made["amount"], 21_000);
    assert_eq!(made["description"], "zap");
    let hash = made["payment_hash"].as_str().unwrap();
    let inv = made["invoice"].as_str().unwrap();
    // Own invoice, by hash or by invoice.
    let by_hash = ok(&recv
        .call("lookup_invoice", json!({ "payment_hash": hash }))
        .await)
    .clone();
    assert_eq!(by_hash["payment_hash"], hash);
    ok(&recv.call("lookup_invoice", json!({ "invoice": inv })).await);
    // Someone else's: indistinguishable from missing.
    recv.wallet.txs.lock().unwrap().insert(
        "ab".repeat(32),
        serde_json::from_value(json!({
            "type": "outgoing", "payment_hash": "ab".repeat(32), "amount": 1, "created_at": 1
        }))
        .unwrap(),
    );
    assert_eq!(
        err(&recv
            .call("lookup_invoice", json!({ "payment_hash": "ab".repeat(32) }))
            .await),
        ErrorCode::NotFound
    );
    assert_eq!(
        err(&recv.call("lookup_invoice", json!({})).await),
        ErrorCode::Other
    );
    // The connection remembered it (persisted for after a restart).
    assert!(recv
        .saved
        .last()
        .recent
        .iter()
        .any(|r| r.payment_hash == hash));

    // With History: anything, and NOT_FOUND for truly missing.
    let hist = setup(&[Permission::History], Budget::default());
    hist.wallet.txs.lock().unwrap().insert(
        "cd".repeat(32),
        serde_json::from_value(json!({
            "type": "incoming", "payment_hash": "cd".repeat(32), "amount": 5, "created_at": 1
        }))
        .unwrap(),
    );
    ok(&hist
        .call("lookup_invoice", json!({ "payment_hash": "cd".repeat(32) }))
        .await);
    assert_eq!(
        err(&hist
            .call("lookup_invoice", json!({ "payment_hash": "ef".repeat(32) }))
            .await),
        ErrorCode::NotFound
    );
}

#[tokio::test]
async fn list_transactions_is_capped() {
    let s = setup(&[Permission::History], Budget::default());
    let r = s
        .call(
            "list_transactions",
            json!({ "limit": 5000, "type": "incoming", "unpaid": true, "from": 5 }),
        )
        .await;
    assert!(ok(&r)["transactions"].is_array());
    let req = s.wallet.last_list.lock().unwrap().clone().unwrap();
    assert_eq!(req.limit, Some(handler::MAX_LIST_LIMIT));
    assert_eq!(req.kind, Some(TransactionType::Incoming));
    assert!(req.unpaid);
    assert_eq!(req.from, Some(5));
}

#[tokio::test]
async fn notifications_both_kinds_only_where_allowed() {
    let wallet = Arc::new(Mock::default());
    let saved = Arc::new(Saved::default());
    let svc = WalletService::new(wallet, saved);
    let yes = new_conn(
        &[Permission::Receive, Permission::Notifications],
        Budget::default(),
        T0,
    );
    let no = new_conn(&[Permission::Receive], Budget::default(), T0);
    svc.set_connections(vec![yes.connection.clone(), no.connection.clone()]);
    let tx: Transaction = serde_json::from_value(json!({
        "type": "incoming", "state": "settled", "payment_hash": "aa".repeat(32),
        "amount": 1000, "fees_paid": 0, "created_at": T0, "settled_at": T0 + 1
    }))
    .unwrap();
    let evs = svc.notification_events(NotificationType::PaymentReceived, &tx, T0 + 2);
    assert_eq!(evs.len(), 2);
    let client = Client::from_uri(&yes.uri).unwrap();
    let mut kinds: Vec<u16> = evs.iter().map(|(_, e)| e.kind).collect();
    kinds.sort();
    assert_eq!(
        kinds,
        vec![KIND_NOTIFICATION_NIP04, KIND_NOTIFICATION_NIP44]
    );
    for (relays, ev) in &evs {
        assert_eq!(relays, &yes.connection.relays);
        assert_eq!(ev.tag("p").unwrap()[0], yes.connection.client_pubkey);
        assert_eq!(
            ev.content.contains("?iv="),
            ev.kind == KIND_NOTIFICATION_NIP04
        );
        let v: Value = serde_json::from_str(&client.decrypt(ev).unwrap()).unwrap();
        assert_eq!(v["notification_type"], "payment_received");
        assert_eq!(v["notification"]["payment_hash"], "aa".repeat(32));
        assert_eq!(v["notification"]["amount"], 1000);
    }
}

#[tokio::test]
async fn info_events_per_connection() {
    let wallet = Arc::new(Mock::default());
    let svc = WalletService::new(wallet, Arc::new(Saved::default()));
    let a = new_conn(
        &[Permission::Pay, Permission::Notifications],
        Budget::default(),
        T0,
    );
    let b = new_conn(&[Permission::Receive], Budget::default(), T0);
    svc.set_connections(vec![a.connection.clone(), b.connection.clone()]);
    let infos = svc.info_events(T0);
    assert_eq!(infos.len(), 2);
    for (_, ev) in infos {
        assert_eq!(ev.kind, KIND_INFO);
        assert!(ev.verify());
        assert_eq!(ev.tag("encryption").unwrap()[0], "nip44_v2 nip04");
        if ev.pubkey == a.connection.wallet_pubkey {
            assert_eq!(ev.content, "pay_invoice get_budget get_info lookup_invoice");
            assert_eq!(
                ev.tag("notifications").unwrap()[0],
                "payment_received payment_sent"
            );
            assert_eq!(ev.tag("extensions").unwrap()[0], "02 13");
        } else {
            assert_eq!(ev.pubkey, b.connection.wallet_pubkey);
            assert_eq!(ev.content, "get_info make_invoice lookup_invoice");
            assert!(ev.tag("notifications").is_none());
        }
    }
    let subs = svc.subscriptions(T0);
    let mut pks = subs["wss://relay.example"].clone();
    pks.sort();
    let mut want = vec![a.connection.wallet_pubkey, b.connection.wallet_pubkey];
    want.sort();
    assert_eq!(pks, want);
}

#[tokio::test]
async fn set_connections_keeps_live_usage() {
    let s = setup(
        ALL,
        Budget {
            limit_sat: Some(100),
            ..Default::default()
        },
    );
    let (inv, _) = invoice(Some(10_000), 1);
    ok(&s.call("pay_invoice", json!({ "invoice": inv })).await);
    // The app edits the name from its stale copy (usage 0).
    let mut edited = s.conn.clone();
    edited.name = "Renamed".into();
    edited.budget.limit_sat = Some(200);
    assert!(s.svc.set_connections(vec![edited]).is_empty());
    let now = s.svc.connection(&s.conn.id).unwrap();
    assert_eq!(now.name, "Renamed");
    assert_eq!(now.budget.limit_sat, Some(200));
    assert_eq!(now.usage.spent_msat, 11_000);
    assert_eq!(now.recent.len(), 1);

    // Invalid connections are skipped, not fatal.
    let mut bad = s.conn.clone();
    bad.id = "bad".into();
    bad.wallet_pubkey = bad.client_pubkey.clone();
    let rejected = s.svc.set_connections(vec![now.clone(), bad]);
    assert_eq!(rejected, vec!["bad".to_string()]);
    assert_eq!(s.svc.connections().len(), 1);

    // Removing it: requests are then ignored.
    s.svc.set_connections(vec![]);
    let ev = s
        .client
        .request("get_info", json!({}), None, T0, vec![])
        .unwrap();
    assert!(matches!(
        s.svc.handle_event_at(&ev, T0).await,
        Outcome::Ignored(Ignored::UnknownWallet)
    ));
}

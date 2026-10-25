//! The whole service against a tiny in-process relay: subscribe, info events,
//! request → response, notifications, connection changes, reconnects, stop.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sessions_backup::nostr::Event;
use sessions_nwc::client::Client;
use sessions_nwc::*;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

// ---------- a minimal NIP-01 relay ----------

#[derive(Default)]
struct RelayState {
    stored: Mutex<Vec<Event>>,
    /// Live subscriptions: (connection id, sub id, filters).
    subs: Mutex<Vec<(u64, String, Vec<Value>)>>,
    open: Mutex<u64>,
}

struct Relay {
    url: String,
    state: Arc<RelayState>,
    kick: broadcast::Sender<()>,
}

fn matches(f: &Value, e: &Event) -> bool {
    let in_list = |key: &str, val: &str| {
        f.get(key)
            .and_then(Value::as_array)
            .is_none_or(|a| a.iter().any(|x| x.as_str() == Some(val)))
    };
    f.get("kinds")
        .and_then(Value::as_array)
        .is_none_or(|a| a.iter().any(|k| k.as_u64() == Some(e.kind as u64)))
        && in_list("authors", &e.pubkey)
        && f.get("#p").is_none_or(|_| {
            e.tags
                .iter()
                .filter(|t| t[0] == "p")
                .any(|t| in_list("#p", &t[1]))
        })
        && f.get("since")
            .and_then(Value::as_u64)
            .is_none_or(|s| e.created_at >= s)
}

async fn start_relay() -> Relay {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let state = Arc::new(RelayState::default());
    let (events, _) = broadcast::channel::<Event>(1024);
    let (kick, _) = broadcast::channel::<()>(4);
    let st = state.clone();
    let k = kick.clone();
    tokio::spawn(async move {
        let mut next_id = 0u64;
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            next_id += 1;
            let id = next_id;
            let st = st.clone();
            let events = events.clone();
            let mut kick = k.subscribe();
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                    return;
                };
                *st.open.lock().unwrap() += 1;
                let mut rx = events.subscribe();
                loop {
                    tokio::select! {
                        _ = kick.recv() => break,
                        ev = rx.recv() => {
                            let Ok(ev) = ev else { continue };
                            let subs: Vec<_> = st.subs.lock().unwrap().iter()
                                .filter(|(c, _, fs)| *c == id && fs.iter().any(|f| matches(f, &ev)))
                                .map(|(_, s, _)| s.clone()).collect();
                            for s in subs {
                                let _ = ws.send(Message::text(json!(["EVENT", s, ev]).to_string())).await;
                            }
                        }
                        msg = ws.next() => {
                            let Some(Ok(msg)) = msg else { break };
                            let Message::Text(t) = msg else { continue };
                            let v: Value = serde_json::from_str(t.as_str()).unwrap();
                            match v[0].as_str().unwrap() {
                                "EVENT" => {
                                    let ev: Event = serde_json::from_value(v[1].clone()).unwrap();
                                    let ok = ev.verify();
                                    let _ = ws.send(Message::text(json!(["OK", ev.id, ok, ""]).to_string())).await;
                                    if !ok { continue; }
                                    if !(20000..30000).contains(&ev.kind) {
                                        let mut s = st.stored.lock().unwrap();
                                        if (10000..20000).contains(&ev.kind) {
                                            s.retain(|o| !(o.kind == ev.kind && o.pubkey == ev.pubkey));
                                        }
                                        s.push(ev.clone());
                                    }
                                    let _ = events.send(ev);
                                }
                                "REQ" => {
                                    let sub = v[1].as_str().unwrap().to_string();
                                    let filters: Vec<Value> = v.as_array().unwrap()[2..].to_vec();
                                    let stored: Vec<Event> = st.stored.lock().unwrap().iter()
                                        .filter(|e| filters.iter().any(|f| matches(f, e))).cloned().collect();
                                    {
                                        let mut subs = st.subs.lock().unwrap();
                                        subs.retain(|(c, s, _)| !(*c == id && *s == sub));
                                        subs.push((id, sub.clone(), filters));
                                    }
                                    for e in stored {
                                        let _ = ws.send(Message::text(json!(["EVENT", sub, e]).to_string())).await;
                                    }
                                    let _ = ws.send(Message::text(json!(["EOSE", sub]).to_string())).await;
                                }
                                "CLOSE" => {
                                    let sub = v[1].as_str().unwrap();
                                    st.subs.lock().unwrap().retain(|(c, s, _)| !(*c == id && s == sub));
                                }
                                _ => {}
                            }
                        }
                    }
                }
                st.subs.lock().unwrap().retain(|(c, _, _)| *c != id);
                *st.open.lock().unwrap() -= 1;
            });
        }
    });
    Relay { url, state, kick }
}

impl Relay {
    /// Whether some connection subscribed to requests for `wallet_pk`.
    fn listening_for(&self, wallet_pk: &str) -> bool {
        let probe = Event {
            id: String::new(),
            pubkey: String::new(),
            created_at: now(),
            kind: KIND_REQUEST,
            tags: vec![vec!["p".into(), wallet_pk.into()]],
            content: String::new(),
            sig: String::new(),
        };
        self.state
            .subs
            .lock()
            .unwrap()
            .iter()
            .any(|(_, _, fs)| fs.iter().any(|f| matches(f, &probe)))
    }

    fn info_of(&self, wallet_pk: &str) -> Option<Event> {
        self.state
            .stored
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.kind == KIND_INFO && e.pubkey == wallet_pk)
            .cloned()
    }
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

// ---------- a client over the relay ----------

struct RelayClient {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    client: Client,
}

impl RelayClient {
    async fn connect(url: &str, uri: &str) -> Self {
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let client = Client::from_uri(uri).unwrap();
        let req =
            json!(["REQ", "c", { "kinds": [23195, 23196, 23197], "#p": [client.pubkey_hex()] }]);
        ws.send(Message::text(req.to_string())).await.unwrap();
        let mut c = RelayClient { ws, client };
        // Wait for EOSE so the subscription is live.
        loop {
            let v = c.next_json().await;
            if v[0] == "EOSE" {
                break;
            }
        }
        c
    }

    async fn next_json(&mut self) -> Value {
        loop {
            let msg = timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("relay message in time")
                .unwrap()
                .unwrap();
            if let Message::Text(t) = msg {
                return serde_json::from_str(t.as_str()).unwrap();
            }
        }
    }

    async fn next_event(&mut self) -> Event {
        loop {
            let v = self.next_json().await;
            if v[0] == "EVENT" {
                return serde_json::from_value(v[2].clone()).unwrap();
            }
        }
    }

    async fn call(&mut self, method: &str, enc: Option<Encryption>) -> ResponseBody {
        let ev = self
            .client
            .request(method, json!({}), enc, now(), vec![])
            .unwrap();
        self.ws
            .send(Message::text(json!(["EVENT", ev]).to_string()))
            .await
            .unwrap();
        loop {
            let e = self.next_event().await;
            if e.kind == KIND_RESPONSE && e.tag("e").map(|t| t[0].clone()) == Some(ev.id.clone()) {
                return self.client.response(&e).unwrap();
            }
        }
    }
}

fn test_config() -> ServiceConfig {
    ServiceConfig {
        ping_interval: Duration::from_millis(300),
        idle_timeout: Duration::from_secs(5),
        min_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(200),
        ..ServiceConfig::default()
    }
}

fn conn_on(relay: &str, perms: &[Permission]) -> NewConnection {
    Connection::create(
        ConnectionOptions {
            name: "loopback".into(),
            relays: vec![relay.to_string()],
            permissions: perms.iter().copied().collect(),
            ..Default::default()
        },
        now(),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_to_end_over_a_relay() {
    let relay = start_relay().await;
    let a = conn_on(&relay.url, &Permission::ALL);
    let saved = Arc::new(Saved::default());
    let handle = start(
        Arc::new(Mock::default()),
        vec![a.connection.clone()],
        saved.clone(),
        test_config(),
    );

    // Subscribed for our wallet key, info event published.
    eventually("subscription", || {
        relay.listening_for(&a.connection.wallet_pubkey)
    })
    .await;
    eventually("info event", || {
        relay.info_of(&a.connection.wallet_pubkey).is_some()
    })
    .await;
    let info = relay.info_of(&a.connection.wallet_pubkey).unwrap();
    assert_eq!(info.tag("encryption").unwrap()[0], "nip44_v2 nip04");
    assert!(info.content.contains("pay_invoice"));
    assert_eq!(
        handle.relay_status(),
        vec![(relay.url.clone(), RelayStatus::Connected)]
    );

    // Request → response, both schemes.
    let mut c = RelayClient::connect(&relay.url, &a.uri).await;
    let r = c.call("get_balance", Some(Encryption::Nip44V2)).await;
    assert_eq!(r.result.unwrap()["balance"], 21_000_000);
    let r = c.call("get_info", None).await;
    assert_eq!(r.result.unwrap()["alias"], "Sessions");
    eventually("usage persisted", || {
        saved
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.last_used_at.is_some())
    })
    .await;

    // Notifications: NIP-04 and NIP-44 copies.
    let tx: Transaction = serde_json::from_value(json!({
        "type": "incoming", "state": "settled", "payment_hash": "aa".repeat(32),
        "amount": 1000, "created_at": now()
    }))
    .unwrap();
    handle.notify_payment_received(&tx);
    let mut kinds = vec![c.next_event().await, c.next_event().await]
        .into_iter()
        .map(|e| {
            let v: Value = serde_json::from_str(&c.client.decrypt(&e).unwrap()).unwrap();
            assert_eq!(v["notification_type"], "payment_received");
            e.kind
        })
        .collect::<Vec<_>>();
    kinds.sort();
    assert_eq!(
        kinds,
        vec![KIND_NOTIFICATION_NIP04, KIND_NOTIFICATION_NIP44]
    );

    // A second connection added at runtime is picked up.
    let b = conn_on(&relay.url, &[Permission::Receive]);
    let rejected = handle.set_connections(vec![a.connection.clone(), b.connection.clone()]);
    assert!(rejected.is_empty());
    eventually("second subscription", || {
        relay.listening_for(&b.connection.wallet_pubkey)
    })
    .await;
    eventually("second info", || {
        relay.info_of(&b.connection.wallet_pubkey).is_some()
    })
    .await;
    let mut cb = RelayClient::connect(&relay.url, &b.uri).await;
    let r = cb.call("get_balance", Some(Encryption::Nip44V2)).await;
    assert_eq!(r.error.unwrap().code, ErrorCode::Restricted);

    // The relay drops everyone: the service reconnects and resubscribes.
    relay.kick.send(()).unwrap();
    eventually("drop", || !relay.listening_for(&a.connection.wallet_pubkey)).await;
    eventually("resubscribe", || {
        relay.listening_for(&a.connection.wallet_pubkey)
            && relay.listening_for(&b.connection.wallet_pubkey)
    })
    .await;
    let mut c = RelayClient::connect(&relay.url, &a.uri).await;
    let r = c.call("get_balance", Some(Encryption::Nip04)).await;
    assert_eq!(r.result.unwrap()["balance"], 21_000_000);

    // Removing a connection unsubscribes its key.
    handle.set_connections(vec![b.connection.clone()]);
    eventually("unsubscribe a", || {
        !relay.listening_for(&a.connection.wallet_pubkey)
    })
    .await;
    assert!(relay.listening_for(&b.connection.wallet_pubkey));

    // Dropping the handle closes the service's socket. Only the test client
    // `c` stays connected (`cb` was dropped by the kick).
    drop(handle);
    eventually("service disconnected", || {
        !relay.listening_for(&b.connection.wallet_pubkey)
    })
    .await;
    eventually("only test clients left", || {
        *relay.state.open.lock().unwrap() == 1
    })
    .await;
}

//! Against real relays (network): `cargo test -p sessions-nwc --test live -- --ignored`.
//! Relays to try: `NWC_LIVE_RELAYS="wss://a wss://b"` (default: the default relay).
//! Behind a TLS-intercepting proxy, point `SESSIONS_EXTRA_CA` at its PEM bundle.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sessions_backup::nostr::Event;
use sessions_nwc::client::Client;
use sessions_nwc::*;
use tokio::time::{timeout, Instant};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::Connector;

fn tls() -> Arc<rustls::ClientConfig> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::CertificateDer;
    let mut roots = sessions_backup::relay::web_roots();
    if let Ok(path) = std::env::var("SESSIONS_EXTRA_CA") {
        let certs: Vec<CertificateDer> = CertificateDer::pem_file_iter(&path)
            .expect("CA bundle")
            .filter_map(Result::ok)
            .collect();
        roots.add_parsable_certificates(certs);
    }
    sessions_backup::relay::tls_with_roots(roots)
}

async fn round_trip(relay: &str) -> Result<Duration, String> {
    let n = Connection::create(
        ConnectionOptions {
            name: "live".into(),
            relays: vec![relay.to_string()],
            permissions: Permission::ALL.into_iter().collect(),
            ..Default::default()
        },
        now(),
    )
    .map_err(|e| e.to_string())?;
    let handle = start(
        Arc::new(Mock::default()),
        vec![n.connection.clone()],
        Arc::new(Saved::default()),
        ServiceConfig {
            tls: tls(),
            ..ServiceConfig::default()
        },
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while handle.relay_status().first().map(|s| &s.1) != Some(&RelayStatus::Connected) {
        if Instant::now() > deadline {
            return Err(format!(
                "service never connected: {:?}",
                handle.relay_status()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let tls = tls();
    let (mut ws, _) = tokio_tungstenite::connect_async_tls_with_config(
        relay,
        None,
        true,
        Some(Connector::Rustls(tls)),
    )
    .await
    .map_err(|e| e.to_string())?;
    let client = Client::from_uri(&n.uri).map_err(|e| e.to_string())?;
    let sub = json!(["REQ", "c",
        { "kinds": [23195], "#p": [client.pubkey_hex()] },
        { "kinds": [13194], "authors": [n.connection.wallet_pubkey] }]);
    ws.send(Message::text(sub.to_string()))
        .await
        .map_err(|e| e.to_string())?;
    // Give the service's REQ a moment, then ask.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let ev = client
        .request(
            "get_info",
            json!({}),
            Some(Encryption::Nip44V2),
            now(),
            vec![],
        )
        .map_err(|e| e.to_string())?;
    let sent = Instant::now();
    ws.send(Message::text(json!(["EVENT", ev]).to_string()))
        .await
        .map_err(|e| e.to_string())?;
    let mut saw_info = false;
    loop {
        let msg = timeout(Duration::from_secs(15), ws.next())
            .await
            .map_err(|_| format!("no response (info event seen: {saw_info})"))?
            .ok_or("closed")?
            .map_err(|e| e.to_string())?;
        let Message::Text(t) = msg else { continue };
        let v: Value = serde_json::from_str(t.as_str()).map_err(|e| e.to_string())?;
        match v[0].as_str() {
            Some("OK") if v[2] == false => return Err(format!("request refused: {}", v[3])),
            Some("EVENT") => {
                let e: Event = serde_json::from_value(v[2].clone()).map_err(|e| e.to_string())?;
                if e.kind == KIND_INFO {
                    saw_info = true;
                } else if e.kind == KIND_RESPONSE {
                    let r = client.response(&e).map_err(|e| e.to_string())?;
                    if r.error.is_some() {
                        return Err(format!("{:?}", r.error));
                    }
                    return Ok(sent.elapsed());
                }
            }
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "network"]
async fn live_relays() {
    let relays = std::env::var("NWC_LIVE_RELAYS").unwrap_or_else(|_| DEFAULT_RELAY.to_string());
    let mut failures = 0;
    for r in relays.split_whitespace() {
        match round_trip(r).await {
            Ok(d) => println!("{r}: ok, response in {} ms", d.as_millis()),
            Err(e) => {
                failures += 1;
                println!("{r}: FAILED: {e}");
            }
        }
    }
    assert_eq!(failures, 0);
}

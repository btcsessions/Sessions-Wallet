//! Join codes against real relays. Run with `cargo test -p sessions-group -- --ignored`.

use sessions_backup::keys::NostrKey;
use sessions_backup::relay::Relays;
use sessions_group::join::{self, Beacon, JoinCode};

fn relays_client() -> Relays {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::CertificateDer;
    use sessions_backup::relay::{tls_with_roots, web_roots};
    let mut roots = web_roots();
    if let Ok(path) = std::env::var("SESSIONS_EXTRA_CA") {
        let certs: Vec<CertificateDer> = CertificateDer::pem_file_iter(&path)
            .expect("CA bundle")
            .filter_map(Result::ok)
            .collect();
        roots.add_parsable_certificates(certs);
    }
    Relays::with_tls(tls_with_roots(roots))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test]
#[ignore]
async fn beacon_and_request_over_relays() {
    let relays: Vec<String> = ["wss://relay.primal.net", "wss://relay.damus.io", "wss://nos.lol"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let r = relays_client();
    let code = JoinCode::generate();
    let host = NostrKey::generate();
    let joiner = NostrKey::generate();
    let t = now();
    let beacon = Beacon {
        host: host.public_hex(),
        relays: relays.clone(),
        name: "Test vault".into(),
        expires: t + 3600,
    };
    let pubs = r.publish(&relays, &[join::beacon_event(&code, &beacon, t).unwrap()]).await;
    println!("beacon: {:?}", pubs.iter().map(|(u, x)| (u, x.is_ok())).collect::<Vec<_>>());
    assert!(pubs.iter().any(|(_, x)| x.is_ok()));

    let found: Vec<_> = r
        .query(&relays, serde_json::json!({"kinds":[join::KIND_BEACON],"#d":[code.beacon_tag()]}))
        .await
        .into_iter()
        .flat_map(|(_, x)| x.unwrap_or_default())
        .collect();
    assert_eq!(join::open_beacon(&code, &found, now()), Some(beacon));

    let req = join::request_event(&code, &host.public_hex(), &joiner, "Mom", now()).unwrap();
    let pubs = r.publish(&relays, &[req]).await;
    println!("request: {:?}", pubs.iter().map(|(u, x)| (u, x.as_ref().err().map(|e| e.to_string()))).collect::<Vec<_>>());
    let got: Vec<_> = r
        .query(&relays, serde_json::json!({"kinds":[join::KIND_JOIN_REQUEST],"#p":[host.public_hex()],"since": t - 60}))
        .await
        .into_iter()
        .flat_map(|(_, x)| x.unwrap_or_default())
        .collect();
    let j = got.iter().find_map(|e| join::open_request(&code, &host, e, now()).ok()).expect("request found");
    assert_eq!(j.pubkey, joiner.public_hex());
}

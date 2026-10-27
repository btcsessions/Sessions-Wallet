//! Device linking against real relays. Run with `cargo test -p sessions-group -- --ignored`.

use sessions_backup::relay::Relays;
use sessions_group::link::{self, Offer, Received, Sender, KIND_LINK};

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
async fn hello_and_seed_over_relays() {
    let relays: Vec<String> = ["wss://relay.primal.net", "wss://relay.damus.io", "wss://nos.lol"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let r = relays_client();
    let offer = Offer::new(now());
    let sender = Sender::new(link::parse(&offer.qr()).unwrap());
    let fetch = || async {
        let mut evs: Vec<_> = r
            .query(
                &relays,
                serde_json::json!({"kinds":[KIND_LINK],"#p":[offer.key.public_hex()],"since": offer.created - 60}),
            )
            .await
            .into_iter()
            .flat_map(|(_, x)| x.unwrap_or_default())
            .collect();
        evs.sort_by_key(|e| e.created_at);
        evs.dedup_by(|a, b| a.id == b.id);
        evs
    };

    let pubs = r.publish(&relays, &[sender.hello("Test phone", now()).unwrap()]).await;
    println!("hello: {:?}", pubs.iter().map(|(u, x)| (u, x.is_ok())).collect::<Vec<_>>());
    assert!(pubs.iter().any(|(_, x)| x.is_ok()));
    let got = fetch().await;
    let hello = got.iter().find_map(|e| match link::open(&offer, e, now()) {
        Ok(Received::Hello { code, .. }) => Some(code),
        _ => None,
    });
    assert_eq!(hello, Some(sender.code()));

    let phrase = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    let pubs = r.publish(&relays, &[sender.seed(phrase, now() + 1).unwrap()]).await;
    assert!(pubs.iter().any(|(_, x)| x.is_ok()));
    let got = fetch().await;
    let seed = got.iter().find_map(|e| match link::open(&offer, e, now() + 2) {
        Ok(Received::Seed { phrase, .. }) => Some(phrase.to_string()),
        _ => None,
    });
    assert_eq!(seed.as_deref(), Some(phrase));
}

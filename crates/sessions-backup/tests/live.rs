//! Publishes a throwaway backup to the default public relays and reads it
//! back. Needs the network: `cargo test -p sessions-backup --test live -- --ignored --nocapture`.
//! Behind a TLS-intercepting proxy, point `SESSIONS_EXTRA_CA` at its PEM bundle.

use std::time::{SystemTime, UNIX_EPOCH};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;
use sessions_backup::keys::{PasskeyKeys, StateKeys};
use sessions_backup::relay::{tls_with_roots, web_roots, Relays};
use sessions_backup::state::{self, Snapshot};
use sessions_backup::{default_relays, envelope, KIND_APP_DATA};
use sessions_core::keystore::Seed;
use sessions_core::Network;

fn relays() -> Relays {
    let mut roots = web_roots();
    if let Ok(path) = std::env::var("SESSIONS_EXTRA_CA") {
        let certs: Vec<CertificateDer> = CertificateDer::pem_file_iter(&path)
            .expect("CA bundle")
            .filter_map(Result::ok)
            .collect();
        let (added, _) = roots.add_parsable_certificates(certs);
        eprintln!("trusting {added} extra roots from {path}");
    }
    Relays::with_tls(tls_with_roots(roots))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test]
#[ignore = "talks to public Nostr relays"]
async fn live_publish_and_fetch() {
    // SESSIONS_RELAYS="wss://a,wss://b" tries other relays.
    let urls: Vec<String> = match std::env::var("SESSIONS_RELAYS") {
        Ok(list) => list.split(',').map(|s| s.trim().to_string()).collect(),
        Err(_) => default_relays(),
    };
    let client = relays();

    // A throwaway wallet.
    let seed = Seed::generate(12).unwrap();
    let keys = StateKeys::from_master(&seed.master_xpriv(Network::Bitcoin).unwrap()).unwrap();
    let snap = Snapshot {
        version: now(),
        created_at: now(),
        data: serde_json::json!({ "test": "sessions-wallet live backup test", "labels": { "a": vec!["x"; 50] } }),
    };
    let events = state::encode(&keys, &snap, now()).unwrap().unwrap();
    let t = std::time::Instant::now();
    let published = client.publish(&urls, &events).await;
    eprintln!("publish took {:?}", t.elapsed());
    for (url, r) in &published {
        eprintln!("publish {url}: {r:?}");
    }
    let accepted = published.iter().filter(|(_, r)| r.is_ok()).count();
    assert!(accepted >= 2, "only {accepted} relays accepted the backup");

    let t = std::time::Instant::now();
    let fetched = client
        .fetch(&urls, &keys.nostr.public_hex(), KIND_APP_DATA)
        .await;
    eprintln!("fetch took {:?}", t.elapsed());
    let mut all = Vec::new();
    for (url, r) in fetched {
        match r {
            Ok(evs) => {
                eprintln!("fetch {url}: {} events", evs.len());
                all.extend(evs);
            }
            Err(e) => eprintln!("fetch {url}: {e}"),
        }
    }
    assert_eq!(state::decode(&keys, &all), Some(snap.clone()));

    // Full-size chunks (~43 KB of base64 each): a wallet with many labels.
    let noise: Vec<String> = (0..1500u32)
        .map(|_| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(rand_bytes())
        })
        .collect();
    let big = Snapshot {
        version: snap.version + 1,
        created_at: now(),
        data: serde_json::json!({ "labels": noise }),
    };
    let events = state::encode(&keys, &big, now() + 1).unwrap().unwrap();
    eprintln!(
        "big backup: {} chunks, largest event content {} bytes",
        events.len(),
        events.iter().map(|e| e.content.len()).max().unwrap()
    );
    let published = client.publish(&urls, &events).await;
    for (url, r) in &published {
        eprintln!("publish big {url}: {r:?}");
    }
    let all: Vec<_> = client
        .fetch(&urls, &keys.nostr.public_hex(), KIND_APP_DATA)
        .await
        .into_iter()
        .filter_map(|(_, r)| r.ok())
        .flatten()
        .collect();
    assert_eq!(state::decode(&keys, &all), Some(big));

    // A seed envelope under a throwaway passkey.
    let pk = PasskeyKeys::from_prf(&rand_bytes()).unwrap();
    let env = envelope::seal(&pk, &seed.phrase(), now()).unwrap();
    let published = client.publish(&urls, std::slice::from_ref(&env)).await;
    assert!(published.iter().filter(|(_, r)| r.is_ok()).count() >= 2);
    let found: Vec<_> = client
        .fetch(&urls, &pk.nostr.public_hex(), KIND_APP_DATA)
        .await
        .into_iter()
        .filter_map(|(_, r)| r.ok())
        .flatten()
        .collect();
    assert_eq!(
        envelope::open(&pk, &found).unwrap().as_str(),
        seed.phrase().as_str()
    );

    // A key with nothing published: relays answer (EOSE) with nothing.
    let empty = PasskeyKeys::from_prf(&rand_bytes()).unwrap();
    let answers = client
        .fetch(&urls, &empty.nostr.public_hex(), KIND_APP_DATA)
        .await;
    let answered = answers
        .iter()
        .filter(|(_, r)| matches!(r, Ok(v) if v.is_empty()))
        .count();
    eprintln!("{answered} relays answered 'nothing' for a fresh key");
    assert!(answered >= 2);
}

fn rand_bytes() -> [u8; 32] {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b
}

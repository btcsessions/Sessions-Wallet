//! The seed envelope: a recovery phrase backed up with a passkey (Advanced
//! "both", or passkey backup turned on for a phrase wallet). The phrase is
//! sealed with the passkey's envelope key and published under the passkey's
//! Nostr key, so the passkey alone finds and opens it.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sessions_core::{Error, Result};
use zeroize::Zeroizing;

use crate::crypto;
use crate::keys::PasskeyKeys;
use crate::nostr::Event;
use crate::{ENVELOPE_D, KIND_APP_DATA};

#[derive(Serialize, Deserialize)]
struct Inner {
    v: u32,
    phrase: String,
}

impl Drop for Inner {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.phrase);
    }
}

pub fn seal(keys: &PasskeyKeys, phrase: &str, created_at: u64) -> Result<Event> {
    let inner = Inner {
        v: 1,
        phrase: phrase.to_string(),
    };
    let json =
        Zeroizing::new(serde_json::to_vec(&inner).map_err(|e| Error::Crypto(e.to_string()))?);
    let sealed = crypto::seal(&keys.envelope_key, ENVELOPE_D.as_bytes(), &json)?;
    Ok(Event::sign(
        &keys.nostr,
        created_at,
        KIND_APP_DATA,
        vec![vec!["d".into(), ENVELOPE_D.into()]],
        B64.encode(sealed),
    ))
}

/// The phrase in the newest envelope that opens with these keys.
pub fn open(keys: &PasskeyKeys, events: &[Event]) -> Option<Zeroizing<String>> {
    let me = keys.nostr.public_hex();
    let mut mine: Vec<&Event> = events
        .iter()
        .filter(|e| e.pubkey == me && e.kind == KIND_APP_DATA && e.d() == Some(ENVELOPE_D))
        .collect();
    mine.sort_by_key(|e| std::cmp::Reverse(e.created_at));
    mine.into_iter().find_map(|e| {
        let sealed = B64.decode(&e.content).ok()?;
        let json = crypto::open(&keys.envelope_key, ENVELOPE_D.as_bytes(), &sealed).ok()?;
        let inner: Inner = serde_json::from_slice(&json).ok()?;
        Some(Zeroizing::new(inner.phrase.clone()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHRASE: &str =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";

    #[test]
    fn round_trip_and_wrong_passkey() {
        let k = PasskeyKeys::from_prf(&[9u8; 32]).unwrap();
        let e = seal(&k, PHRASE, 1_700_000_000).unwrap();
        assert!(e.verify());
        assert_eq!(e.d(), Some(ENVELOPE_D));
        assert!(!e.content.contains("legal"));
        assert_eq!(open(&k, std::slice::from_ref(&e)).unwrap().as_str(), PHRASE);

        // Another passkey: neither its key nor its identity match.
        let other = PasskeyKeys::from_prf(&[10u8; 32]).unwrap();
        assert!(open(&other, std::slice::from_ref(&e)).is_none());
        // Re-signed by the other passkey's identity, still not decryptable.
        let resigned = Event::sign(
            &other.nostr,
            e.created_at,
            e.kind,
            e.tags.clone(),
            e.content.clone(),
        );
        assert!(open(&other, &[resigned]).is_none());
        assert!(open(&k, &[]).is_none());
    }

    #[test]
    fn newest_envelope_wins() {
        let k = PasskeyKeys::from_prf(&[9u8; 32]).unwrap();
        let old = seal(&k, "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about", 1).unwrap();
        let new = seal(&k, PHRASE, 2).unwrap();
        assert_eq!(open(&k, &[old, new]).unwrap().as_str(), PHRASE);
    }
}

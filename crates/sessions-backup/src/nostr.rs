//! Just enough Nostr (NIP-01) for the backup: build, sign and verify events.

use bitcoin::hashes::{sha256, Hash};
use bitcoin::hex::{DisplayHex, FromHex};
use bitcoin::secp256k1::{schnorr, Message, XOnlyPublicKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::keys::{secp, NostrKey};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u16,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
}

/// NIP-01 event id: SHA-256 of `[0, pubkey, created_at, kind, tags, content]`.
fn event_id(
    pubkey: &str,
    created_at: u64,
    kind: u16,
    tags: &[Vec<String>],
    content: &str,
) -> [u8; 32] {
    let canonical = serde_json::json!([0, pubkey, created_at, kind, tags, content]);
    sha256::Hash::hash(canonical.to_string().as_bytes()).to_byte_array()
}

impl Event {
    pub fn sign(
        key: &NostrKey,
        created_at: u64,
        kind: u16,
        tags: Vec<Vec<String>>,
        content: String,
    ) -> Event {
        let pubkey = key.public_hex();
        let id = event_id(&pubkey, created_at, kind, &tags, &content);
        let mut aux = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut aux);
        let sig = secp().sign_schnorr_with_aux_rand(&Message::from_digest(id), key.keypair(), &aux);
        Event {
            id: id.to_lower_hex_string(),
            pubkey,
            created_at,
            kind,
            tags,
            content,
            sig: sig.serialize().to_lower_hex_string(),
        }
    }

    /// The id matches the content and the signature is the author's.
    pub fn verify(&self) -> bool {
        let id = event_id(
            &self.pubkey,
            self.created_at,
            self.kind,
            &self.tags,
            &self.content,
        );
        if id.to_lower_hex_string() != self.id {
            return false;
        }
        let (Ok(pk), Ok(sig)) = (
            <[u8; 32]>::from_hex(&self.pubkey),
            <[u8; 64]>::from_hex(&self.sig),
        ) else {
            return false;
        };
        let (Ok(pk), Ok(sig)) = (
            XOnlyPublicKey::from_slice(&pk),
            schnorr::Signature::from_slice(&sig),
        ) else {
            return false;
        };
        secp()
            .verify_schnorr(&sig, &Message::from_digest(id), &pk)
            .is_ok()
    }

    /// Values of the first tag called `name` (after the name).
    pub fn tag(&self, name: &str) -> Option<&[String]> {
        self.tags
            .iter()
            .find(|t| t.first().map(String::as_str) == Some(name))
            .map(|t| &t[1..])
    }

    /// The `d` tag of a parameterized replaceable event.
    pub fn d(&self) -> Option<&str> {
        self.tag("d").and_then(|v| v.first()).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::PasskeyKeys;

    #[test]
    fn sign_and_verify() {
        let k = PasskeyKeys::from_prf(&[3u8; 32]).unwrap();
        let tags = vec![vec!["d".to_string(), "x/y".to_string()]];
        let e = Event::sign(
            &k.nostr,
            1_700_000_000,
            30078,
            tags,
            "hi \"there\"\n".into(),
        );
        assert!(e.verify());
        assert_eq!(e.d(), Some("x/y"));
        assert_eq!(e.pubkey, k.nostr.public_hex());

        // Survives JSON (as it comes back from a relay).
        let back: Event = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert!(back.verify());

        let mut bad = e.clone();
        bad.content.push('!');
        assert!(!bad.verify());
        let mut bad = e.clone();
        bad.created_at += 1;
        assert!(!bad.verify());
        let mut bad = e.clone();
        // Someone else's key, same id: the signature doesn't match.
        bad.pubkey = PasskeyKeys::from_prf(&[4u8; 32])
            .unwrap()
            .nostr
            .public_hex();
        assert!(!bad.verify());
        let mut bad = e;
        bad.sig = "00".repeat(64);
        assert!(!bad.verify());
    }

    /// NIP-01 serialisation: compact JSON, no spaces, strings escaped.
    #[test]
    fn id_matches_nip01_serialisation() {
        let tags = vec![vec!["d".to_string(), "a".to_string()]];
        let s = serde_json::json!([0, "ab", 5, 30078, tags, "x\ny"]).to_string();
        assert_eq!(s, r#"[0,"ab",5,30078,[["d","a"]],"x\ny"]"#);
    }
}

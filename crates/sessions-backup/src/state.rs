//! The wallet-state backup: a JSON snapshot, packed, sealed with the state key
//! and published as NIP-78 app-data events signed by the state Nostr key.
//!
//! Size: the sealed snapshot is split into chunks of at most [`CHUNK_BYTES`]
//! (≈ 43 KB of base64 per event, under every common relay's limit). Chunk 0
//! has `d` = [`STATE_D`], chunk *i* `STATE_D/i`. Every chunk carries
//! `["chunk", i, n, set]`, where `set` names this particular upload (a hash of
//! the sealed bytes), so leftovers of an older, longer upload are never mixed
//! in. A typical wallet is one chunk; [`MAX_CHUNKS`] chunks hold ≈ 512 KB
//! sealed, i.e. tens of thousands of labels. Past that, the caller backs up
//! without labels (see `TooLarge`).

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::hex::DisplayHex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sessions_core::{Error, Result};

use crate::crypto;
use crate::keys::StateKeys;
use crate::nostr::Event;
use crate::{KIND_APP_DATA, STATE_D};

pub const CHUNK_BYTES: usize = 32 * 1024;
pub const MAX_CHUNKS: usize = 16;

/// What's inside the encryption.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// Monotonic across devices: restore picks the highest.
    pub version: u64,
    /// Unix seconds when this snapshot was made.
    pub created_at: u64,
    /// The app's state (config, labels…), opaque here.
    pub data: Value,
}

/// SHA-256 of the data's JSON: "has anything changed since the last upload?"
pub fn content_hash(data: &Value) -> String {
    sha256::Hash::hash(data.to_string().as_bytes())
        .to_byte_array()
        .to_lower_hex_string()
}

/// The snapshot doesn't fit in [`MAX_CHUNKS`] events.
#[derive(Debug)]
pub struct TooLarge(pub usize);

/// Seal and split `snap` into signed events, `created_at` on each.
pub fn encode(
    keys: &StateKeys,
    snap: &Snapshot,
    created_at: u64,
) -> Result<std::result::Result<Vec<Event>, TooLarge>> {
    let json = serde_json::to_vec(snap).map_err(|e| Error::Storage(e.to_string()))?;
    let sealed = crypto::seal(&keys.state_key, STATE_D.as_bytes(), &crypto::pack(&json)?)?;
    let parts: Vec<&[u8]> = sealed.chunks(CHUNK_BYTES).collect();
    if parts.len() > MAX_CHUNKS {
        return Ok(Err(TooLarge(sealed.len())));
    }
    let set = set_id(&sealed);
    let n = parts.len();
    Ok(Ok(parts
        .into_iter()
        .enumerate()
        .map(|(i, part)| {
            let d = if i == 0 {
                STATE_D.to_string()
            } else {
                format!("{STATE_D}/{i}")
            };
            let tags = vec![
                vec!["d".into(), d],
                vec!["chunk".into(), i.to_string(), n.to_string(), set.clone()],
            ];
            Event::sign(
                &keys.nostr,
                created_at,
                KIND_APP_DATA,
                tags,
                B64.encode(part),
            )
        })
        .collect()))
}

fn set_id(sealed: &[u8]) -> String {
    sha256::Hash::hash(sealed).to_byte_array()[..8].to_lower_hex_string()
}

/// The newest snapshot (highest version, then newest) that can be fully
/// reassembled and decrypted from these events, gathered from any relays.
pub fn decode(keys: &StateKeys, events: &[Event]) -> Option<Snapshot> {
    use std::collections::BTreeMap;
    let me = keys.nostr.public_hex();
    // set → n → parts by index
    let mut sets: BTreeMap<(String, usize), BTreeMap<usize, &str>> = BTreeMap::new();
    for e in events {
        if e.pubkey != me
            || e.kind != KIND_APP_DATA
            || !e.d().is_some_and(|d| d.starts_with(STATE_D))
        {
            continue;
        }
        let Some([i, n, set]) = e
            .tag("chunk")
            .and_then(|c| <&[String; 3]>::try_from(c).ok())
        else {
            continue;
        };
        let (Ok(i), Ok(n)) = (i.parse::<usize>(), n.parse::<usize>()) else {
            continue;
        };
        if n == 0 || n > MAX_CHUNKS || i >= n {
            continue;
        }
        sets.entry((set.clone(), n))
            .or_default()
            .insert(i, e.content.as_str());
    }
    sets.into_iter()
        .filter(|((_, n), parts)| parts.len() == *n)
        .filter_map(|((set, _), parts)| {
            let mut sealed = Vec::new();
            for p in parts.values() {
                sealed.extend(B64.decode(p).ok()?);
            }
            if set_id(&sealed) != set {
                return None;
            }
            let packed = crypto::open(&keys.state_key, STATE_D.as_bytes(), &sealed).ok()?;
            let json = crypto::unpack(&packed).ok()?;
            serde_json::from_slice::<Snapshot>(&json).ok()
        })
        .max_by_key(|s| (s.version, s.created_at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessions_core::keystore::Seed;
    use sessions_core::Network;

    fn keys(phrase: &str) -> StateKeys {
        let seed = Seed::from_phrase(phrase).unwrap();
        StateKeys::from_master(&seed.master_xpriv(Network::Bitcoin).unwrap()).unwrap()
    }
    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const LEGAL: &str =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";

    fn snap(version: u64, data: Value) -> Snapshot {
        Snapshot {
            version,
            created_at: 1_700_000_000 + version,
            data,
        }
    }

    #[test]
    fn round_trip_one_chunk() {
        let k = keys(ABANDON);
        let s = snap(
            3,
            serde_json::json!({ "config": { "accounts": ["a", "b"] } }),
        );
        let events = encode(&k, &s, 1_700_000_100).unwrap().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].d(), Some(STATE_D));
        assert_eq!(events[0].kind, 30078);
        assert!(events[0].verify());
        // Nothing readable on the relay.
        assert!(!events[0].content.contains("accounts"));
        assert_eq!(decode(&k, &events), Some(s));
        // Another seed can't read it (and ignores events that aren't its own).
        assert_eq!(decode(&keys(LEGAL), &events), None);
    }

    #[test]
    fn highest_version_wins_across_relays() {
        let k = keys(ABANDON);
        let v1 = encode(&k, &snap(1, serde_json::json!({"n": 1})), 100)
            .unwrap()
            .unwrap();
        let v5 = encode(&k, &snap(5, serde_json::json!({"n": 5})), 90)
            .unwrap()
            .unwrap();
        let v3 = encode(&k, &snap(3, serde_json::json!({"n": 3})), 300)
            .unwrap()
            .unwrap();
        // Relay A has v1, relay B v5, relay C v3 (created_at doesn't decide).
        let all: Vec<Event> = [v1, v5, v3].concat();
        assert_eq!(decode(&k, &all).unwrap().version, 5);
        assert_eq!(decode(&k, &[]), None);
    }

    #[test]
    fn chunks_and_incomplete_sets() {
        let k = keys(ABANDON);
        // Incompressible data, ~3 chunks.
        let noise: Vec<String> = (0..2400u32)
            .map(|i| sha256::Hash::hash(&i.to_be_bytes()).to_string())
            .collect();
        let big = snap(7, serde_json::json!({ "labels": noise }));
        let events = encode(&k, &big, 1).unwrap().unwrap();
        assert!(events.len() >= 3, "{} chunks", events.len());
        assert_eq!(events[1].d(), Some(format!("{STATE_D}/1").as_str()));
        assert!(events
            .iter()
            .all(|e| e.content.len() <= CHUNK_BYTES * 4 / 3 + 4));
        assert_eq!(decode(&k, &events).unwrap(), big);

        // An older, smaller backup plus leftovers of the big one minus a chunk:
        // the incomplete big set is ignored, the small one is used.
        let small = snap(6, serde_json::json!({"n": 6}));
        let mut mixed = encode(&k, &small, 2).unwrap().unwrap();
        mixed.extend(events[1..].iter().cloned());
        assert_eq!(decode(&k, &mixed).unwrap(), small);

        // A chunk with a forged set id is rejected.
        let mut forged = events.clone();
        forged[0].content = B64.encode(b"junk");
        assert_eq!(decode(&k, &forged), None);
    }

    #[test]
    fn too_large() {
        let k = keys(ABANDON);
        let noise: Vec<String> = (0..20_000u32)
            .map(|i| sha256::Hash::hash(&i.to_be_bytes()).to_string())
            .collect();
        let r = encode(&k, &snap(1, serde_json::json!(noise)), 1).unwrap();
        assert!(r.is_err());
    }

    #[test]
    fn hash_tracks_content() {
        let a = serde_json::json!({"x": 1});
        assert_eq!(content_hash(&a), content_hash(&serde_json::json!({"x": 1})));
        assert_ne!(content_hash(&a), content_hash(&serde_json::json!({"x": 2})));
    }
}

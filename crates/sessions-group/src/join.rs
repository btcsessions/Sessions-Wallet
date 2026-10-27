//! Join codes: how someone asks to join a vault group.
//!
//! The creator's app makes a short code (`K7QM-2XPA-9TRD`), shown as a QR in
//! person or texted as a code or link. The code is the only secret:
//!
//! 1. The creator publishes a *beacon*: an app-data event (kind 30078) under a
//!    throwaway key, found by a tag derived from the code and encrypted with
//!    a key derived from the code. It says which wallet key to ask and where.
//!    Relays learn nothing without the code; it expires after a day.
//! 2. The joiner's app finds the beacon, then sends a *join request* to the
//!    creator: their wallet key and name, signed by that key, carrying a proof
//!    that they hold the code, NIP-44 encrypted to the creator under another
//!    throwaway key.
//! 3. The creator sees each request and approves or ignores it. Approving
//!    invites the joiner's wallet key into the Marmot group.
//!
//! A code never lets anyone in on its own. The creator approves every person.

use base64::Engine as _;
use bitcoin::secp256k1::{SecretKey, XOnlyPublicKey};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sessions_backup::keys::NostrKey;
use sessions_backup::nostr::Event;
use sessions_core::{Error, Result};
use sessions_nwc::nip44::{self, ConversationKey};
use sha2::Sha256;
use zeroize::Zeroizing;

/// NIP-78 app data (addressable): the beacon.
pub const KIND_BEACON: u16 = 30078;
/// The join request (outer, under a throwaway key).
pub const KIND_JOIN_REQUEST: u16 = 4610;
/// The join request (inner, signed by the joiner's wallet key).
pub const KIND_JOIN_INNER: u16 = 4611;

/// How long a code works.
pub const CODE_LIFETIME_SECS: u64 = 24 * 60 * 60;
/// A request older than this is ignored.
const REQUEST_MAX_AGE_SECS: u64 = CODE_LIFETIME_SECS + 60 * 60;

/// Crockford base32 (no I, L, O, U).
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_CHARS: usize = 12;
const LINK_PREFIX: &str = "sessionswallet://join/";

/// A join code: 12 characters, 60 bits.
#[derive(Clone, PartialEq, Eq)]
pub struct JoinCode(Zeroizing<String>);

impl std::fmt::Debug for JoinCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JoinCode(…)")
    }
}

struct Derived {
    tag: String,
    key: Zeroizing<[u8; 32]>,
    proof: String,
}

impl JoinCode {
    pub fn generate() -> Self {
        let mut bytes = [0u8; CODE_CHARS];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
        JoinCode(Zeroizing::new(
            bytes
                .iter()
                .map(|b| ALPHABET[(*b & 31) as usize] as char)
                .collect(),
        ))
    }

    /// From what was typed, scanned or tapped: the code with or without
    /// dashes and spaces, any case, or the join link. Look-alikes are mapped
    /// (O→0, I/L→1).
    pub fn parse(input: &str) -> Result<Self> {
        let s = input.trim();
        let s = s
            .strip_prefix(LINK_PREFIX)
            .or_else(|| s.strip_prefix("sessionswallet:join/"))
            .unwrap_or(s);
        let code: String = s
            .chars()
            .filter(|c| !matches!(c, '-' | ' ' | '_'))
            .map(|c| match c.to_ascii_uppercase() {
                'O' => '0',
                'I' | 'L' => '1',
                c => c,
            })
            .collect();
        if code.len() != CODE_CHARS || !code.bytes().all(|b| ALPHABET.contains(&b)) {
            return Err(Error::Invalid(
                "That isn't a join code (12 letters and numbers)".into(),
            ));
        }
        Ok(JoinCode(Zeroizing::new(code)))
    }

    /// `K7QM-2XPA-9TRD`.
    pub fn display(&self) -> String {
        let c = self.0.as_str();
        format!("{}-{}-{}", &c[..4], &c[4..8], &c[8..])
    }

    /// `sessionswallet://join/K7QM2XPA9TRD`, for links and QR codes.
    pub fn link(&self) -> String {
        format!("{LINK_PREFIX}{}", self.0.as_str())
    }

    fn derive(&self) -> Derived {
        let hk = Hkdf::<Sha256>::new(Some(b"sessions-wallet/join/v1"), self.0.as_bytes());
        let mut okm = Zeroizing::new([0u8; 96]);
        hk.expand(b"beacon-tag|beacon-key|proof", &mut okm[..])
            .expect("96 bytes is a valid HKDF length");
        let mut key = Zeroizing::new([0u8; 32]);
        key.copy_from_slice(&okm[32..64]);
        Derived {
            tag: hex(&okm[..32]),
            key,
            proof: hex(&okm[64..]),
        }
    }

    /// The beacon's `d` tag, to look it up.
    pub fn beacon_tag(&self) -> String {
        self.derive().tag
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn tag<'a>(e: &'a Event, name: &str) -> Option<&'a str> {
    e.tags
        .iter()
        .find(|t| t.first().map(String::as_str) == Some(name))
        .and_then(|t| t.get(1))
        .map(String::as_str)
}

/// What the beacon tells a joiner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Beacon {
    /// The creator's wallet public key (hex).
    pub host: String,
    /// Where to send the join request.
    pub relays: Vec<String>,
    /// The vault's name, shown to the joiner.
    pub name: String,
    pub expires: u64,
}

const BEACON_AAD: &[u8] = b"sessions-wallet/join-beacon/v1";

pub fn beacon_event(code: &JoinCode, beacon: &Beacon, now: u64) -> Result<Event> {
    let d = code.derive();
    let plain = serde_json::to_vec(beacon).map_err(|e| Error::Invalid(e.to_string()))?;
    let sealed = sessions_backup::crypto::seal(&d.key, BEACON_AAD, &plain)?;
    Ok(Event::sign(
        &NostrKey::generate(),
        now,
        KIND_BEACON,
        vec![
            vec!["d".into(), d.tag],
            vec!["expiration".into(), beacon.expires.to_string()],
        ],
        base64::engine::general_purpose::STANDARD.encode(sealed),
    ))
}

/// The newest unexpired beacon for `code` among `events`.
pub fn open_beacon(code: &JoinCode, events: &[Event], now: u64) -> Option<Beacon> {
    let d = code.derive();
    events
        .iter()
        .filter(|e| e.kind == KIND_BEACON && tag(e, "d") == Some(d.tag.as_str()) && e.verify())
        .filter_map(|e| {
            let sealed = base64::engine::general_purpose::STANDARD
                .decode(&e.content)
                .ok()?;
            let plain = sessions_backup::crypto::open(&d.key, BEACON_AAD, &sealed).ok()?;
            let b: Beacon = serde_json::from_slice(&plain).ok()?;
            (b.expires > now && b.host.len() == 64).then_some((e.created_at, b))
        })
        .max_by_key(|(at, _)| *at)
        .map(|(_, b)| b)
}

#[derive(Serialize, Deserialize)]
struct Inner {
    name: String,
}

fn secret(key: &NostrKey) -> Result<SecretKey> {
    SecretKey::from_slice(&key.secret_bytes()[..]).map_err(|e| Error::Crypto(e.to_string()))
}

fn xonly(hex: &str) -> Result<XOnlyPublicKey> {
    sessions_nwc::xonly_from_hex(hex).map_err(|e| Error::Invalid(e.to_string()))
}

/// The joiner's request to `host`, signed by their wallet key.
pub fn request_event(
    code: &JoinCode,
    host: &str,
    joiner: &NostrKey,
    name: &str,
    now: u64,
) -> Result<Event> {
    let d = code.derive();
    let inner = Event::sign(
        joiner,
        now,
        KIND_JOIN_INNER,
        vec![
            vec!["p".into(), host.into()],
            vec!["proof".into(), d.proof],
        ],
        serde_json::to_string(&Inner {
            name: name.trim().chars().take(40).collect(),
        })
        .expect("serializable"),
    );
    let throwaway = NostrKey::generate();
    let conv = ConversationKey::new(&secret(&throwaway)?, &xonly(host)?);
    let content = nip44::encrypt(&conv, &serde_json::to_string(&inner).expect("serializable"))
        .map_err(|e| Error::Crypto(e.to_string()))?;
    Ok(Event::sign(
        &throwaway,
        now,
        KIND_JOIN_REQUEST,
        vec![vec!["p".into(), host.into()]],
        content,
    ))
}

/// Someone asking to join.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Joiner {
    /// Their wallet public key (hex): what gets invited.
    pub pubkey: String,
    pub name: String,
    pub at: u64,
}

/// A join request for `host`, checked against `code`. `Err` for anything
/// not addressed to us, not carrying the code's proof, or not signed.
pub fn open_request(code: &JoinCode, host: &NostrKey, event: &Event, now: u64) -> Result<Joiner> {
    let bad = || Error::Invalid("not a join request for this code".into());
    let host_hex = host.public_hex();
    if event.kind != KIND_JOIN_REQUEST || tag(event, "p") != Some(host_hex.as_str()) || !event.verify() {
        return Err(bad());
    }
    let conv = ConversationKey::new(&secret(host)?, &xonly(&event.pubkey)?);
    let plain = nip44::decrypt(&conv, &event.content).map_err(|_| bad())?;
    let inner: Event = serde_json::from_str(&plain).map_err(|_| bad())?;
    if inner.kind != KIND_JOIN_INNER
        || tag(&inner, "p") != Some(host_hex.as_str())
        || tag(&inner, "proof") != Some(code.derive().proof.as_str())
        || !inner.verify()
        || inner.created_at + REQUEST_MAX_AGE_SECS < now
        || inner.created_at > now + 10 * 60
        || inner.pubkey == host_hex
    {
        return Err(bad());
    }
    let body: Inner = serde_json::from_str(&inner.content).map_err(|_| bad())?;
    Ok(Joiner {
        pubkey: inner.pubkey,
        name: body.name,
        at: inner.created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_formats() {
        let c = JoinCode::generate();
        let shown = c.display();
        assert_eq!(shown.len(), 14);
        assert_eq!(JoinCode::parse(&shown).unwrap(), c);
        assert_eq!(JoinCode::parse(&shown.to_lowercase()).unwrap(), c);
        assert_eq!(JoinCode::parse(&c.link()).unwrap(), c);
        assert_eq!(
            JoinCode::parse("k7qm 2xpa 9trd").unwrap(),
            JoinCode::parse("K7QM-2XPA-9TRD").unwrap()
        );
        assert_eq!(
            JoinCode::parse("O0IL-0000-0000").unwrap(),
            JoinCode::parse("0011-0000-0000").unwrap()
        );
        assert!(JoinCode::parse("K7QM-2XPA").is_err());
        assert!(JoinCode::parse("K7QM-2XPA-9TRU").is_err());
        assert_ne!(JoinCode::generate(), c);
    }

    #[test]
    fn beacon_and_request() {
        let now = 1_800_000_000;
        let code = JoinCode::generate();
        let host = NostrKey::generate();
        let joiner = NostrKey::generate();
        let beacon = Beacon {
            host: host.public_hex(),
            relays: vec!["wss://relay.example".into()],
            name: "Family vault".into(),
            expires: now + CODE_LIFETIME_SECS,
        };
        let ev = beacon_event(&code, &beacon, now).unwrap();
        assert!(!ev.content.contains("Family"));
        assert_ne!(ev.pubkey, host.public_hex());
        assert_eq!(open_beacon(&code, &[ev.clone()], now), Some(beacon.clone()));
        // Wrong code, or expired: nothing.
        assert_eq!(open_beacon(&JoinCode::generate(), &[ev.clone()], now), None);
        assert_eq!(open_beacon(&code, &[ev], now + CODE_LIFETIME_SECS + 1), None);

        let req = request_event(&code, &host.public_hex(), &joiner, "Mom", now).unwrap();
        assert_ne!(req.pubkey, joiner.public_hex());
        let j = open_request(&code, &host, &req, now + 5).unwrap();
        assert_eq!(j.pubkey, joiner.public_hex());
        assert_eq!(j.name, "Mom");
        // Without the code, or for someone else, or too old: refused.
        assert!(open_request(&JoinCode::generate(), &host, &req, now).is_err());
        assert!(open_request(&code, &NostrKey::generate(), &req, now).is_err());
        assert!(open_request(&code, &host, &req, now + 3 * CODE_LIFETIME_SECS).is_err());
        let mut tampered = req.clone();
        tampered.content.push('A');
        assert!(open_request(&code, &host, &tampered, now).is_err());
    }
}

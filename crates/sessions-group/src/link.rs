//! Linking a new device to a wallet (`sessions-group::link`).
//!
//! The new device shows a QR code carrying a throwaway public key `E` and a
//! random secret `S`. The wallet device scans it and, over Nostr relays:
//!
//! 1. sends a **hello** from its own throwaway key `P` to `E`, with a MAC
//!    under `S` (so only someone who saw the QR can start a link);
//! 2. both devices show the same 6-digit **check code**, from `S`, `E`, `P`;
//! 3. once the person confirms the codes match (and enters the passcode on
//!    the wallet device), it sends the **seed** to `E`, NIP-44 encrypted and
//!    MAC'd under `S`.
//!
//! The new device then restores the wallet from the seed and pulls the
//! encrypted setup backup like any restore. The recovery phrase is never
//! shown as a readable QR, and only the holder of `E`'s secret (the screen
//! that showed the QR) can read it. Events expire after [`LINK_TTL_SECS`].

use bitcoin::hashes::{hmac, sha256, Hash, HashEngine};
use bitcoin::secp256k1::{SecretKey, XOnlyPublicKey};
use serde::{Deserialize, Serialize};
use sessions_backup::keys::NostrKey;
use sessions_backup::nostr::Event;
use sessions_core::{Error, Result};
use sessions_nwc::nip44::{self, ConversationKey};
use zeroize::Zeroizing;

/// Link messages (hello, seed), from a throwaway key, `p`-tagged to the new device.
pub const KIND_LINK: u16 = 4620;
/// How long an offer and its messages live.
pub const LINK_TTL_SECS: u64 = 15 * 60;
/// The QR / link prefix.
pub const PREFIX: &str = "sessionswallet://link/";

/// The new device's side: shown as a QR.
pub struct Offer {
    pub key: NostrKey,
    pub secret: Zeroizing<[u8; 16]>,
    pub created: u64,
}

impl Offer {
    pub fn new(now: u64) -> Self {
        let mut s = Zeroizing::new([0u8; 16]);
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut s[..]);
        Offer {
            key: NostrKey::generate(),
            secret: s,
            created: now,
        }
    }

    /// What the QR encodes.
    pub fn qr(&self) -> String {
        format!("{PREFIX}{}/{}", self.key.public_hex(), hex(&self.secret[..]))
    }

    pub fn expired(&self, now: u64) -> bool {
        now > self.created + LINK_TTL_SECS
    }
}

/// What the wallet device read from the QR.
#[derive(Clone)]
pub struct Scanned {
    pub target: String,
    pub secret: Zeroizing<[u8; 16]>,
}

impl std::fmt::Debug for Scanned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scanned").field("target", &self.target).finish_non_exhaustive()
    }
}

pub fn parse(qr: &str) -> Result<Scanned> {
    let bad = || Error::Invalid("That isn't a Sessions Wallet link code".into());
    let rest = qr.trim().strip_prefix(PREFIX).ok_or_else(bad)?;
    let (pk, s) = rest.split_once('/').ok_or_else(bad)?;
    if pk.len() != 64 || xonly(pk).is_err() {
        return Err(bad());
    }
    let bytes = unhex(s).ok_or_else(bad)?;
    let secret: [u8; 16] = bytes.try_into().map_err(|_| bad())?;
    Ok(Scanned {
        target: pk.to_lowercase(),
        secret: Zeroizing::new(secret),
    })
}

/// The 6-digit code both screens show.
pub fn check_code(secret: &[u8; 16], target: &str, sender: &str) -> String {
    let m = mac(secret, &["code", target, sender]);
    let n = u32::from_be_bytes([m[0], m[1], m[2], m[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
enum Body {
    Hello { mac: String, name: String },
    Seed { mac: String, phrase: String },
}

/// What the new device got.
pub enum Received {
    /// A wallet device wants to link: show the code.
    Hello { sender: String, name: String, code: String },
    /// The seed.
    Seed { sender: String, phrase: Zeroizing<String> },
}

/// The wallet device's two messages come from this throwaway key.
pub struct Sender {
    pub key: NostrKey,
    pub to: Scanned,
}

impl Sender {
    pub fn new(to: Scanned) -> Self {
        Sender {
            key: NostrKey::generate(),
            to,
        }
    }

    pub fn code(&self) -> String {
        check_code(&self.to.secret, &self.to.target, &self.key.public_hex())
    }

    pub fn hello(&self, name: &str, now: u64) -> Result<Event> {
        let mac = hex(&mac(&self.to.secret, &["hello", &self.to.target, &self.key.public_hex()]));
        self.event(
            &Body::Hello {
                mac,
                name: name.trim().chars().take(40).collect(),
            },
            now,
        )
    }

    pub fn seed(&self, phrase: &str, now: u64) -> Result<Event> {
        let mac = hex(&mac(&self.to.secret, &["seed", &self.to.target, &self.key.public_hex()]));
        self.event(
            &Body::Seed {
                mac,
                phrase: phrase.to_string(),
            },
            now,
        )
    }

    fn event(&self, body: &Body, now: u64) -> Result<Event> {
        let conv = ConversationKey::new(&secret(&self.key)?, &xonly(&self.to.target)?);
        let plain = Zeroizing::new(serde_json::to_string(body).expect("serializable"));
        let content = nip44::encrypt(&conv, &plain).map_err(|e| Error::Crypto(e.to_string()))?;
        Ok(Event::sign(
            &self.key,
            now,
            KIND_LINK,
            vec![
                vec!["p".into(), self.to.target.clone()],
                vec!["expiration".into(), (now + LINK_TTL_SECS).to_string()],
            ],
            content,
        ))
    }
}

/// Read a link message addressed to `offer`. `Err` for anything else.
pub fn open(offer: &Offer, event: &Event, now: u64) -> Result<Received> {
    let bad = || Error::Invalid("not a link message for this code".into());
    let me = offer.key.public_hex();
    if event.kind != KIND_LINK
        || event.tag("p").and_then(|t| t.first()).map(String::as_str) != Some(me.as_str())
        || !event.verify()
        || event.created_at + LINK_TTL_SECS < now
        || event.created_at + 60 < offer.created
    {
        return Err(bad());
    }
    let conv = ConversationKey::new(&secret(&offer.key)?, &xonly(&event.pubkey)?);
    let plain = Zeroizing::new(nip44::decrypt(&conv, &event.content).map_err(|_| bad())?);
    let body: Body = serde_json::from_str(&plain).map_err(|_| bad())?;
    let expect = |what: &str| hex(&mac(&offer.secret, &[what, &me, &event.pubkey]));
    match body {
        Body::Hello { mac, name } if mac == expect("hello") => Ok(Received::Hello {
            code: check_code(&offer.secret, &me, &event.pubkey),
            sender: event.pubkey.clone(),
            name,
        }),
        Body::Seed { mac, phrase } if mac == expect("seed") => Ok(Received::Seed {
            sender: event.pubkey.clone(),
            phrase: Zeroizing::new(phrase),
        }),
        _ => Err(bad()),
    }
}

fn mac(secret: &[u8; 16], parts: &[&str]) -> [u8; 32] {
    let mut e = hmac::HmacEngine::<sha256::Hash>::new(&secret[..]);
    for p in parts {
        e.input(&(p.len() as u32).to_be_bytes());
        e.input(p.as_bytes());
    }
    hmac::Hmac::<sha256::Hash>::from_engine(e).to_byte_array()
}

fn secret(key: &NostrKey) -> Result<SecretKey> {
    SecretKey::from_slice(&key.secret_bytes()[..]).map_err(|e| Error::Crypto(e.to_string()))
}

fn xonly(hex: &str) -> Result<XOnlyPublicKey> {
    sessions_nwc::xonly_from_hex(hex).map_err(|e| Error::Invalid(e.to_string()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn links_end_to_end() {
        let now = 1_800_000_000;
        let offer = Offer::new(now);
        let scanned = parse(&offer.qr()).unwrap();
        let sender = Sender::new(scanned);

        let hello = sender.hello("Ben's phone", now + 5).unwrap();
        let Received::Hello { code, name, sender: from } = open(&offer, &hello, now + 6).unwrap() else {
            panic!("expected hello")
        };
        assert_eq!(code, sender.code());
        assert_eq!(code.len(), 7);
        assert_eq!(name, "Ben's phone");

        let seed = sender.seed(PHRASE, now + 20).unwrap();
        assert!(!seed.content.contains("abandon"));
        let Received::Seed { phrase, sender: from2 } = open(&offer, &seed, now + 21).unwrap() else {
            panic!("expected seed")
        };
        assert_eq!(phrase.as_str(), PHRASE);
        assert_eq!(from, from2);
    }

    #[test]
    fn needs_the_secret_from_the_qr() {
        let now = 1_800_000_000;
        let offer = Offer::new(now);
        let mut wrong = parse(&offer.qr()).unwrap();
        wrong.secret[0] ^= 1;
        let s = Sender::new(wrong);
        assert!(open(&offer, &s.hello("x", now).unwrap(), now).is_err());
        assert!(open(&offer, &s.seed(PHRASE, now).unwrap(), now).is_err());
    }

    #[test]
    fn other_offers_and_old_messages_are_ignored() {
        let now = 1_800_000_000;
        let offer = Offer::new(now);
        let other = Offer::new(now);
        let s = Sender::new(parse(&offer.qr()).unwrap());
        let hello = s.hello("x", now).unwrap();
        assert!(open(&other, &hello, now).is_err());
        assert!(open(&offer, &hello, now + LINK_TTL_SECS + 1).is_err());
    }

    #[test]
    fn rejects_bad_codes() {
        assert!(parse("sessionswallet://join/K7QM").is_err());
        assert!(parse("sessionswallet://link/abcd/00").is_err());
    }
}

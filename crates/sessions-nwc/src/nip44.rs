//! NIP-44 version 2 encrypted payloads: secp256k1 ECDH, HKDF-SHA256,
//! power-of-two padding, ChaCha20 and HMAC-SHA256 over `nonce || ciphertext`.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use bitcoin::secp256k1::{SecretKey, XOnlyPublicKey};
use chacha20::cipher::{KeyIvInit, StreamCipher};
use chacha20::ChaCha20;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::crypto::shared_x;
use crate::Error;

const VERSION: u8 = 2;
const SALT: &[u8] = b"nip44-v2";
const EXTENDED_PREFIX_THRESHOLD: usize = 65536;
/// Payloads bigger than this are refused before decoding: NWC messages are
/// small, and relays cap events far below this anyway.
pub const MAX_PAYLOAD_LEN: usize = 1 << 22;

/// The long-term key between two parties: `conv(a, B) == conv(b, A)`.
#[derive(Clone)]
pub struct ConversationKey(Zeroizing<[u8; 32]>);

impl ConversationKey {
    pub fn new(sk: &SecretKey, pk: &XOnlyPublicKey) -> Self {
        let x = shared_x(sk, pk);
        let (prk, _) = Hkdf::<Sha256>::extract(Some(SALT), &x[..]);
        let mut k = Zeroizing::new([0u8; 32]);
        k.copy_from_slice(&prk);
        ConversationKey(k)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        ConversationKey(Zeroizing::new(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for ConversationKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConversationKey(…)")
    }
}

struct MessageKeys {
    chacha_key: Zeroizing<[u8; 32]>,
    chacha_nonce: [u8; 12],
    hmac_key: Zeroizing<[u8; 32]>,
}

fn message_keys(conv: &ConversationKey, nonce: &[u8; 32]) -> MessageKeys {
    let hk = Hkdf::<Sha256>::from_prk(&conv.0[..]).expect("32-byte PRK");
    let mut okm = Zeroizing::new([0u8; 76]);
    hk.expand(nonce, &mut okm[..]).expect("76 ≤ 255·32");
    let mut chacha_key = Zeroizing::new([0u8; 32]);
    let mut chacha_nonce = [0u8; 12];
    let mut hmac_key = Zeroizing::new([0u8; 32]);
    chacha_key.copy_from_slice(&okm[..32]);
    chacha_nonce.copy_from_slice(&okm[32..44]);
    hmac_key.copy_from_slice(&okm[44..76]);
    MessageKeys {
        chacha_key,
        chacha_nonce,
        hmac_key,
    }
}

/// Padded length (without the length prefix) of an `unpadded_len`-byte message.
pub fn calc_padded_len(unpadded_len: usize) -> usize {
    if unpadded_len <= 32 {
        return 32;
    }
    // 1 << (floor(log2(len − 1)) + 1)
    let next_power = 1usize << (usize::BITS - (unpadded_len - 1).leading_zeros());
    let chunk = if next_power <= 256 {
        32
    } else {
        next_power / 8
    };
    chunk * ((unpadded_len - 1) / chunk + 1)
}

fn pad(plaintext: &str) -> Result<Zeroizing<Vec<u8>>, Error> {
    let bytes = plaintext.as_bytes();
    let len = bytes.len();
    if len == 0 || len as u64 > u32::MAX as u64 {
        return Err(Error::Crypto("invalid plaintext length".into()));
    }
    let mut out = Zeroizing::new(Vec::with_capacity(6 + calc_padded_len(len)));
    if len >= EXTENDED_PREFIX_THRESHOLD {
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    } else {
        out.extend_from_slice(&(len as u16).to_be_bytes());
    }
    out.extend_from_slice(bytes);
    let total = out.len() + calc_padded_len(len) - len;
    out.resize(total, 0);
    Ok(out)
}

fn unpad(padded: &[u8]) -> Result<String, Error> {
    let bad = || Error::Crypto("invalid padding".into());
    if padded.len() < 2 {
        return Err(bad());
    }
    let first = u16::from_be_bytes([padded[0], padded[1]]) as usize;
    let (len, prefix) = if first == 0 {
        if padded.len() < 6 {
            return Err(bad());
        }
        let l = u32::from_be_bytes([padded[2], padded[3], padded[4], padded[5]]) as usize;
        if l < EXTENDED_PREFIX_THRESHOLD {
            return Err(bad());
        }
        (l, 6)
    } else {
        (first, 2)
    };
    if len == 0 || padded.len() != prefix + calc_padded_len(len) {
        return Err(bad());
    }
    let text = &padded[prefix..prefix + len];
    String::from_utf8(text.to_vec()).map_err(|_| Error::Crypto("plaintext is not UTF-8".into()))
}

fn mac(hmac_key: &[u8; 32], nonce: &[u8; 32], ciphertext: &[u8]) -> Hmac<Sha256> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(hmac_key).expect("any key length");
    m.update(nonce);
    m.update(ciphertext);
    m
}

/// Encrypt with a fresh random nonce.
pub fn encrypt(conv: &ConversationKey, plaintext: &str) -> Result<String, Error> {
    let mut nonce = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    encrypt_with_nonce(conv, plaintext, &nonce)
}

/// Encrypt with a given nonce. Only for test vectors: never reuse a nonce.
pub fn encrypt_with_nonce(
    conv: &ConversationKey,
    plaintext: &str,
    nonce: &[u8; 32],
) -> Result<String, Error> {
    let keys = message_keys(conv, nonce);
    let mut buf = pad(plaintext)?;
    ChaCha20::new((&*keys.chacha_key).into(), (&keys.chacha_nonce).into())
        .apply_keystream(&mut buf[..]);
    let tag = mac(&keys.hmac_key, nonce, &buf).finalize().into_bytes();
    let mut raw = Vec::with_capacity(1 + 32 + buf.len() + 32);
    raw.push(VERSION);
    raw.extend_from_slice(nonce);
    raw.extend_from_slice(&buf);
    raw.extend_from_slice(&tag);
    Ok(B64.encode(raw))
}

/// Decrypt and authenticate a payload.
pub fn decrypt(conv: &ConversationKey, payload: &str) -> Result<String, Error> {
    if payload.is_empty() || payload.starts_with('#') {
        return Err(Error::Crypto("unknown encryption version".into()));
    }
    if payload.len() < 132 || payload.len() > MAX_PAYLOAD_LEN {
        return Err(Error::Crypto("invalid payload size".into()));
    }
    let data = B64
        .decode(payload)
        .map_err(|_| Error::Crypto("invalid base64".into()))?;
    let dlen = data.len();
    if dlen < 99 {
        return Err(Error::Crypto("invalid data size".into()));
    }
    if data[0] != VERSION {
        return Err(Error::Crypto(format!(
            "unknown encryption version {}",
            data[0]
        )));
    }
    let nonce: [u8; 32] = data[1..33].try_into().expect("32 bytes");
    let ciphertext = &data[33..dlen - 32];
    let tag = &data[dlen - 32..];
    let keys = message_keys(conv, &nonce);
    // Constant-time comparison.
    mac(&keys.hmac_key, &nonce, ciphertext)
        .verify_slice(tag)
        .map_err(|_| Error::Crypto("invalid MAC".into()))?;
    let mut buf = Zeroizing::new(ciphertext.to_vec());
    ChaCha20::new((&*keys.chacha_key).into(), (&keys.chacha_nonce).into())
        .apply_keystream(&mut buf[..]);
    unpad(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{secret_key_from_hex, xonly_from_hex};
    use bitcoin::hex::{DisplayHex, FromHex};
    use serde_json::Value;
    use sha2::Digest;

    const VECTORS: &str = include_str!("../tests/data/nip44.vectors.json");

    fn v2() -> Value {
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        v["v2"].clone()
    }

    fn s<'a>(v: &'a Value, k: &str) -> &'a str {
        v[k].as_str().unwrap()
    }

    fn h32(s: &str) -> [u8; 32] {
        <[u8; 32]>::from_hex(s).unwrap()
    }

    #[test]
    fn vectors_file_is_the_official_one() {
        // Checksum published in NIP-44.
        assert_eq!(
            sha2::Sha256::digest(VECTORS.as_bytes()).to_lower_hex_string(),
            "269ed0f69e4c192512cc779e78c555090cebc7c785b609e338a62afc3ce25040"
        );
    }

    #[test]
    fn conversation_keys() {
        let cases = v2()["valid"]["get_conversation_key"].clone();
        let cases = cases.as_array().unwrap();
        assert!(cases.len() > 30);
        for c in cases {
            let sk = secret_key_from_hex(s(c, "sec1")).unwrap();
            let pk = xonly_from_hex(s(c, "pub2")).unwrap();
            let k = ConversationKey::new(&sk, &pk);
            assert_eq!(
                k.as_bytes().to_lower_hex_string(),
                s(c, "conversation_key"),
                "{c}"
            );
        }
    }

    #[test]
    fn invalid_conversation_keys() {
        for c in v2()["invalid"]["get_conversation_key"].as_array().unwrap() {
            let ok = secret_key_from_hex(s(c, "sec1"))
                .and_then(|sk| xonly_from_hex(s(c, "pub2")).map(|pk| (sk, pk)));
            assert!(ok.is_err(), "{c}");
        }
    }

    #[test]
    fn message_keys_vectors() {
        let mk = v2()["valid"]["get_message_keys"].clone();
        let conv = ConversationKey::from_bytes(h32(s(&mk, "conversation_key")));
        for c in mk["keys"].as_array().unwrap() {
            let k = message_keys(&conv, &h32(s(c, "nonce")));
            assert_eq!(k.chacha_key.to_lower_hex_string(), s(c, "chacha_key"));
            assert_eq!(k.chacha_nonce.to_lower_hex_string(), s(c, "chacha_nonce"));
            assert_eq!(k.hmac_key.to_lower_hex_string(), s(c, "hmac_key"));
        }
    }

    #[test]
    fn padded_lengths() {
        for c in v2()["valid"]["calc_padded_len"].as_array().unwrap() {
            let (u, p) = (c[0].as_u64().unwrap(), c[1].as_u64().unwrap());
            assert_eq!(calc_padded_len(u as usize) as u64, p, "{c}");
        }
    }

    #[test]
    fn encrypt_decrypt_vectors() {
        for c in v2()["valid"]["encrypt_decrypt"].as_array().unwrap() {
            let sec1 = secret_key_from_hex(s(c, "sec1")).unwrap();
            let sec2 = secret_key_from_hex(s(c, "sec2")).unwrap();
            let pub1 = sec1.x_only_public_key(crate::crypto::secp()).0;
            let pub2 = sec2.x_only_public_key(crate::crypto::secp()).0;
            let k12 = ConversationKey::new(&sec1, &pub2);
            assert_eq!(
                k12.as_bytes().to_lower_hex_string(),
                s(c, "conversation_key")
            );
            let payload = encrypt_with_nonce(&k12, s(c, "plaintext"), &h32(s(c, "nonce"))).unwrap();
            assert_eq!(payload, s(c, "payload"));
            let k21 = ConversationKey::new(&sec2, &pub1);
            assert_eq!(k21.as_bytes(), k12.as_bytes());
            assert_eq!(decrypt(&k21, &payload).unwrap(), s(c, "plaintext"));
        }
    }

    #[test]
    fn long_message_vectors() {
        for c in v2()["valid"]["encrypt_decrypt_long_msg"]
            .as_array()
            .unwrap()
        {
            let conv = ConversationKey::from_bytes(h32(s(c, "conversation_key")));
            let text = s(c, "pattern").repeat(c["repeat"].as_u64().unwrap() as usize);
            assert_eq!(
                sha2::Sha256::digest(text.as_bytes()).to_lower_hex_string(),
                s(c, "plaintext_sha256")
            );
            let payload = encrypt_with_nonce(&conv, &text, &h32(s(c, "nonce"))).unwrap();
            assert_eq!(
                sha2::Sha256::digest(payload.as_bytes()).to_lower_hex_string(),
                s(c, "payload_sha256")
            );
            assert_eq!(decrypt(&conv, &payload).unwrap(), text);
        }
    }

    /// The extended-prefix boundary table from the NIP text.
    #[test]
    fn extended_prefix_vectors() {
        let conv = ConversationKey::from_bytes(h32(
            "c41c775356fd92eadc63ff5a0dc1da211b268cbea22316767095b2871ea1412d",
        ));
        let nonce = h32("0000000000000000000000000000000000000000000000000000000000000001");
        for (len, payload_sha) in [
            (
                65535,
                "6d8c2810d1e870fbaa1f0a0937126cca837a15f9260e27060c331d70a3c0bc84",
            ),
            (
                65536,
                "b7b4edb36ba92e267d322d56d9aebc22e7fa96ff52e3c12adc07f07a43cbc616",
            ),
            (
                65537,
                "eeb7c7c5373894ea2c1547cfd3ccb15d5a0b2d619da852e5c79df792dcc9e435",
            ),
        ] {
            let text = "a".repeat(len);
            let payload = encrypt_with_nonce(&conv, &text, &nonce).unwrap();
            assert_eq!(
                sha2::Sha256::digest(payload.as_bytes()).to_lower_hex_string(),
                payload_sha,
                "len {len}"
            );
            assert_eq!(decrypt(&conv, &payload).unwrap(), text);
        }
    }

    #[test]
    fn invalid_lengths() {
        let conv = ConversationKey::from_bytes([7u8; 32]);
        assert!(encrypt(&conv, "").is_err());
        for len in v2()["invalid"]["encrypt_msg_lengths"].as_array().unwrap() {
            let len = len.as_u64().unwrap();
            // Only the cheap ones (the others are 4 GB): 0 must fail.
            if len == 0 {
                assert!(encrypt(&conv, "").is_err());
            }
        }
    }

    #[test]
    fn invalid_decrypt_vectors() {
        let cases = v2()["invalid"]["decrypt"].clone();
        for c in cases.as_array().unwrap() {
            let conv = ConversationKey::from_bytes(h32(s(c, "conversation_key")));
            assert!(decrypt(&conv, s(c, "payload")).is_err(), "{}", c["note"]);
        }
    }

    #[test]
    fn random_round_trip() {
        let conv = ConversationKey::from_bytes([9u8; 32]);
        let a = encrypt(&conv, "{\"method\":\"get_info\"}").unwrap();
        let b = encrypt(&conv, "{\"method\":\"get_info\"}").unwrap();
        assert_ne!(a, b);
        assert_eq!(decrypt(&conv, &a).unwrap(), "{\"method\":\"get_info\"}");
    }
}

//! NIP-04 (deprecated, kept for legacy NWC clients): AES-256-CBC with the
//! unhashed ECDH x coordinate as key, `<base64 ciphertext>?iv=<base64 iv>`.

use aes::Aes256;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use bitcoin::secp256k1::{SecretKey, XOnlyPublicKey};
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use rand::RngCore;

use crate::crypto::shared_x;
use crate::Error;

type Enc = cbc::Encryptor<Aes256>;
type Dec = cbc::Decryptor<Aes256>;

/// Same cap as NIP-44: NWC messages are small.
const MAX_PAYLOAD_LEN: usize = crate::nip44::MAX_PAYLOAD_LEN;

pub fn encrypt(sk: &SecretKey, pk: &XOnlyPublicKey, plaintext: &str) -> String {
    let mut iv = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut iv);
    encrypt_with_iv(sk, pk, plaintext, &iv)
}

fn encrypt_with_iv(sk: &SecretKey, pk: &XOnlyPublicKey, plaintext: &str, iv: &[u8; 16]) -> String {
    let key = shared_x(sk, pk);
    let ct =
        Enc::new((&*key).into(), iv.into()).encrypt_padded_vec_mut::<Pkcs7>(plaintext.as_bytes());
    format!("{}?iv={}", B64.encode(ct), B64.encode(iv))
}

pub fn decrypt(sk: &SecretKey, pk: &XOnlyPublicKey, payload: &str) -> Result<String, Error> {
    if payload.len() > MAX_PAYLOAD_LEN {
        return Err(Error::Crypto("payload too large".into()));
    }
    let (ct, iv) = payload
        .split_once("?iv=")
        .ok_or_else(|| Error::Crypto("not a NIP-04 payload".into()))?;
    let ct = B64
        .decode(ct)
        .map_err(|_| Error::Crypto("invalid base64".into()))?;
    let iv: [u8; 16] = B64
        .decode(iv)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| Error::Crypto("invalid IV".into()))?;
    let key = shared_x(sk, pk);
    let pt = Dec::new((&*key).into(), (&iv).into())
        .decrypt_padded_vec_mut::<Pkcs7>(&ct)
        .map_err(|_| Error::Crypto("couldn't decrypt".into()))?;
    String::from_utf8(pt).map_err(|_| Error::Crypto("plaintext is not UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{secp, secret_key_from_hex};

    const SK1: &str = "91ba716fa9e7ea2fcbad360cf4f8e0d312f73984da63d90f524ad61a6a1e7dbe";
    const SK2: &str = "96f6fa197aa07477ab88f6981118466ae3a982faab8ad5db9d5426870c73d220";

    fn keys() -> (SecretKey, XOnlyPublicKey, SecretKey, XOnlyPublicKey) {
        let sk1 = secret_key_from_hex(SK1).unwrap();
        let sk2 = secret_key_from_hex(SK2).unwrap();
        let pk1 = sk1.x_only_public_key(secp()).0;
        let pk2 = sk2.x_only_public_key(secp()).0;
        (sk1, pk1, sk2, pk2)
    }

    /// Vectors from nostr-tools' nip04.test.ts ("decrypt message from go-nostr").
    #[test]
    fn known_vectors() {
        let (_, pk1, sk2, _) = keys();
        assert_eq!(
            decrypt(
                &sk2,
                &pk1,
                "zJxfaJ32rN5Dg1ODjOlEew==?iv=EV5bUjcc4OX2Km/zPp4ndQ=="
            )
            .unwrap(),
            "nanana"
        );
        let big = "6f8dMstm+udOu7yipSn33orTmwQpWbtfuY95NH+eTU1kArysWJIDkYgI2D25EAGIDJsNd45jOJ2NbVOhFiL3ZP/NWsTwXokk34iyHyA/lkjzugQ1bHXoMD1fP/Ay4hB4al1NHb8HXHKZaxPrErwdRDb8qa/I6dXb/1xxyVvNQBHHvmsM5yIFaPwnCN1DZqXf2KbTA/Ekz7Hy+7R+Sy3TXLQDFpWYqykppkXc7Fs0qSuPRyxz5+anuN0dxZa9GTwTEnBrZPbthKkNRrvZMdTGJ6WumOh9aUq8OJJWy9aOgsXvs7qjN1UqcCqQqYaVnEOhCaqWNDsVtsFrVDj+SaLIBvCiomwF4C4nIgngJ5I69tx0UNI0q+ZnvOGQZ7m1PpW2NYP7Yw43HJNdeUEQAmdCPnh/PJwzLTnIxHmQU7n7SPlMdV0SFa6H8y2HHvex697GAkyE5t8c2uO24OnqIwF1tR3blIqXzTSRl0GA6QvrSj2p4UtnWjvF7xT7RiIEyTtgU/AsihTrXyXzWWZaIBJogpgw6erlZqWjCH7sZy/WoGYEiblobOAqMYxax6vRbeuGtoYksr/myX+x9rfLrYuoDRTw4woXOLmMrrj+Mf0TbAgc3SjdkqdsPU1553rlSqIEZXuFgoWmxvVQDtekgTYyS97G81TDSK9nTJT5ilku8NVq2LgtBXGwsNIw/xekcOUzJke3kpnFPutNaexR1VF3ohIuqRKYRGcd8ADJP2lfwMcaGRiplAmFoaVS1YUhQwYFNq9rMLf7YauRGV4BJg/t9srdGxf5RoKCvRo+XM/nLxxysTR9MVaEP/3lDqjwChMxs+eWfLHE5vRWV8hUEqdrWNZV29gsx5nQpzJ4PARGZVu310pQzc6JAlc2XAhhFk6RamkYJnmCSMnb/RblzIATBi2kNrCVAlaXIon188inB62rEpZGPkRIP7PUfu27S/elLQHBHeGDsxOXsBRo1gl3te+raoBHsxo6zvRnYbwdAQa5taDE63eh+fT6kFI+xYmXNAQkU8Dp0MVhEh4JQI06Ni/AKrvYpC95TXXIphZcF+/Pv/vaGkhG2X9S3uhugwWK?iv=2vWkOQQi0WynNJz/aZ4k2g==";
        assert_eq!(decrypt(&sk2, &pk1, big).unwrap(), "z".repeat(800));
    }

    #[test]
    fn round_trip_both_directions() {
        let (sk1, pk1, sk2, pk2) = keys();
        let c = encrypt(&sk1, &pk2, "hello {\"json\": true} ünïcode");
        assert!(c.contains("?iv="));
        assert_eq!(
            decrypt(&sk2, &pk1, &c).unwrap(),
            "hello {\"json\": true} ünïcode"
        );
        // Deterministic for a fixed IV, and the IV is carried along.
        let a = encrypt_with_iv(&sk1, &pk2, "x", &[1u8; 16]);
        assert_eq!(a, encrypt_with_iv(&sk2, &pk1, "x", &[1u8; 16]));
        assert!(a.ends_with("?iv=AQEBAQEBAQEBAQEBAQEBAQ=="));
    }

    #[test]
    fn rejects_garbage() {
        let (_, pk1, sk2, _) = keys();
        assert!(decrypt(&sk2, &pk1, "no-iv-here").is_err());
        assert!(decrypt(&sk2, &pk1, "zJxfaJ32rN5Dg1ODjOlEew==?iv=short").is_err());
        // Wrong key: padding check fails (or garbage that isn't the message).
        let other = secret_key_from_hex(SK1).unwrap();
        let r = decrypt(
            &other,
            &pk1,
            "zJxfaJ32rN5Dg1ODjOlEew==?iv=EV5bUjcc4OX2Km/zPp4ndQ==",
        );
        assert!(r.map(|s| s != "nanana").unwrap_or(true));
    }
}

//! One recovery phrase for everything.
//!
//! * On-chain accounts use the master phrase directly (BIP84/86/48), so the
//!   phrase restores in Sparrow, Nunchuk, etc.
//! * Spark and ecash each get their **own BIP85 child phrase**, so a bug or
//!   leak in one SDK can never expose on-chain keys, while the single master
//!   phrase still recovers every account.
//!
//! At rest the phrase is encrypted with XChaCha20-Poly1305 under a key
//! stretched from the user's passcode with Argon2id. Where the platform has
//! a hardware key store (Android Keystore), the key also mixes in a random
//! device secret that only that store can unwrap (vault version 2), so a
//! copy of the vault file can't be brute-forced off the device.

use std::str::FromStr;

use argon2::{Algorithm, Argon2, Params, Version};
use bip39::{Language, Mnemonic};
use bitcoin::bip32::{DerivationPath, Xpriv};
use bitcoin::hashes::{hmac, sha512, Hash, HashEngine};
use bitcoin::secp256k1::Secp256k1;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::types::Network;
use crate::{Error, Result};

/// BIP85 child indices. Never change these once shipped.
pub mod child {
    pub const SPARK: u32 = 0;
    pub const ECASH: u32 = 1;
    pub const SWAPS: u32 = 2;
}

pub struct Seed {
    mnemonic: Mnemonic,
}

impl Seed {
    pub fn generate(words: usize) -> Result<Self> {
        let mnemonic = Mnemonic::generate_in(Language::English, words)
            .map_err(|e| Error::InvalidMnemonic(e.to_string()))?;
        Ok(Self { mnemonic })
    }

    pub fn from_phrase(phrase: &str) -> Result<Self> {
        let normalised = phrase
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let mnemonic = Mnemonic::parse_in_normalized(Language::English, &normalised)
            .map_err(|e| Error::InvalidMnemonic(e.to_string()))?;
        Ok(Self { mnemonic })
    }

    /// The phrase for this BIP39 entropy (16 bytes: 12 words, 32: 24).
    /// Passkey wallets derive their entropy from the passkey.
    pub fn from_entropy(entropy: &[u8]) -> Result<Self> {
        let mnemonic = Mnemonic::from_entropy_in(Language::English, entropy)
            .map_err(|e| Error::InvalidMnemonic(e.to_string()))?;
        Ok(Self { mnemonic })
    }

    pub fn phrase(&self) -> Zeroizing<String> {
        Zeroizing::new(self.mnemonic.to_string())
    }

    pub fn word_count(&self) -> usize {
        self.mnemonic.word_count()
    }

    pub fn master_xpriv(&self, network: Network) -> Result<Xpriv> {
        let seed = Zeroizing::new(self.mnemonic.to_seed(""));
        Xpriv::new_master(network.to_bitcoin(), &seed[..]).map_err(|e| Error::Crypto(e.to_string()))
    }

    pub fn fingerprint(&self) -> Result<String> {
        let xprv = self.master_xpriv(Network::Bitcoin)?;
        Ok(xprv.fingerprint(&Secp256k1::new()).to_string())
    }

    /// BIP85 child mnemonic (application 39, English, 12 words).
    pub fn bip85_child(&self, index: u32) -> Result<Seed> {
        bip85_from_master(&self.master_xpriv(Network::Bitcoin)?, index)
    }
}

fn bip85_from_master(master: &Xpriv, index: u32) -> Result<Seed> {
    let words = 12u32;
    let secp = Secp256k1::new();
    let path = DerivationPath::from_str(&format!("m/83696968'/39'/0'/{words}'/{index}'"))
        .map_err(|e| Error::Crypto(e.to_string()))?;
    let derived = master
        .derive_priv(&secp, &path)
        .map_err(|e| Error::Crypto(e.to_string()))?;

    let mut engine = hmac::HmacEngine::<sha512::Hash>::new(b"bip-entropy-from-k");
    engine.input(&derived.private_key.secret_bytes());
    let mut entropy = hmac::Hmac::<sha512::Hash>::from_engine(engine).to_byte_array();

    let mnemonic = Mnemonic::from_entropy_in(Language::English, &entropy[..16])
        .map_err(|e| Error::Crypto(e.to_string()));
    entropy.zeroize();
    Ok(Seed {
        mnemonic: mnemonic?,
    })
}

/// Encrypted phrase as stored on disk / in the OS keychain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vault {
    pub version: u8,
    pub kdf: KdfParams,
    #[serde(with = "hex_bytes")]
    pub salt: Vec<u8>,
    #[serde(with = "hex_bytes")]
    pub nonce: Vec<u8>,
    #[serde(with = "hex_bytes")]
    pub ciphertext: Vec<u8>,
    /// Version 2: the device secret, wrapped by the platform key store.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_hex_bytes")]
    pub device: Option<Vec<u8>>,
}

/// A random secret held by the platform's key store, mixed into the vault key.
pub struct DeviceSecret {
    /// The secret itself (in memory only).
    pub secret: Zeroizing<[u8; 32]>,
    /// As the key store wrapped it: what's saved in the vault.
    pub wrapped: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct KdfParams {
    pub m_cost_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl Default for KdfParams {
    /// 64 MiB / 3 passes: ~0.5s on a recent phone.
    fn default() -> Self {
        Self {
            m_cost_kib: 64 * 1024,
            t_cost: 3,
            p_cost: 1,
        }
    }
}

impl Vault {
    pub fn seal(seed: &Seed, passcode: &str, kdf: KdfParams) -> Result<Vault> {
        Self::seal_with(seed, passcode, kdf, None)
    }

    /// Seal, mixing in `device` when the platform has a key store.
    pub fn seal_with(seed: &Seed, passcode: &str, kdf: KdfParams, device: Option<&DeviceSecret>) -> Result<Vault> {
        let mut salt = vec![0u8; 16];
        let mut nonce = vec![0u8; 24];
        rand::rngs::OsRng.fill_bytes(&mut salt);
        rand::rngs::OsRng.fill_bytes(&mut nonce);

        let key = derive_key(passcode, &salt, kdf, device.map(|d| &*d.secret))?;
        let cipher = XChaCha20Poly1305::new_from_slice(&key[..])
            .map_err(|e| Error::Crypto(e.to_string()))?;
        let phrase = seed.phrase();
        let ciphertext = cipher
            .encrypt(XNonce::from_slice(&nonce), phrase.as_bytes())
            .map_err(|e| Error::Crypto(e.to_string()))?;

        Ok(Vault {
            version: if device.is_some() { 2 } else { 1 },
            kdf,
            salt,
            nonce,
            ciphertext,
            device: device.map(|d| d.wrapped.clone()),
        })
    }

    pub fn open(&self, passcode: &str) -> Result<Seed> {
        self.open_with(passcode, None)
    }

    /// Open; a version 2 vault needs its device secret (unwrapped by the
    /// key store from [`Vault::device`]).
    pub fn open_with(&self, passcode: &str, device: Option<&[u8; 32]>) -> Result<Seed> {
        if self.device.is_some() && device.is_none() {
            return Err(Error::Crypto("this wallet's device key isn't available".into()));
        }
        let key = derive_key(passcode, &self.salt, self.kdf, device)?;
        let cipher = XChaCha20Poly1305::new_from_slice(&key[..])
            .map_err(|e| Error::Crypto(e.to_string()))?;
        let plain = Zeroizing::new(
            cipher
                .decrypt(XNonce::from_slice(&self.nonce), self.ciphertext.as_ref())
                .map_err(|_| Error::WrongPasscode)?,
        );
        let phrase = std::str::from_utf8(&plain).map_err(|e| Error::Crypto(e.to_string()))?;
        Seed::from_phrase(phrase)
    }
}

fn derive_key(passcode: &str, salt: &[u8], kdf: KdfParams, device: Option<&[u8; 32]>) -> Result<Zeroizing<[u8; 32]>> {
    let params = Params::new(kdf.m_cost_kib, kdf.t_cost, kdf.p_cost, Some(32))
        .map_err(|e| Error::Crypto(e.to_string()))?;
    let mut key = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passcode.as_bytes(), salt, &mut key[..])
        .map_err(|e| Error::Crypto(e.to_string()))?;
    let Some(device) = device else { return Ok(key) };
    // HMAC-SHA256 keyed by the device secret over the stretched passcode.
    let mut e = hmac::HmacEngine::<bitcoin::hashes::sha256::Hash>::new(&device[..]);
    e.input(b"sessions-vault/v2");
    e.input(&key[..]);
    let mixed = hmac::Hmac::<bitcoin::hashes::sha256::Hash>::from_engine(e).to_byte_array();
    key.copy_from_slice(&mixed);
    Ok(key)
}

/// A fresh device secret (the key store wraps it).
pub fn new_device_secret() -> Zeroizing<[u8; 32]> {
    let mut s = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(&mut s[..]);
    s
}

mod opt_hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(b) => super::hex_bytes::serialize(b, s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        match Option::<String>::deserialize(d)? {
            None => Ok(None),
            Some(s) => super::hex_bytes::deserialize(serde::de::value::StrDeserializer::<D::Error>::new(&s)).map(Some),
        }
    }
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.iter().map(|b| format!("{b:02x}")).collect::<String>())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        if s.len() % 2 != 0 {
            return Err(serde::de::Error::custom("odd hex length"));
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(serde::de::Error::custom))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn fast() -> KdfParams {
        KdfParams {
            m_cost_kib: 1024,
            t_cost: 1,
            p_cost: 1,
        }
    }

    #[test]
    fn vault_round_trip_and_wrong_passcode() {
        let seed = Seed::from_phrase(ABANDON).unwrap();
        let vault = Vault::seal(&seed, "123456", fast()).unwrap();
        let json = serde_json::to_string(&vault).unwrap();
        let vault: Vault = serde_json::from_str(&json).unwrap();
        assert_eq!(vault.open("123456").unwrap().phrase().as_str(), ABANDON);
        assert!(matches!(vault.open("000000"), Err(Error::WrongPasscode)));
    }

    #[test]
    fn device_bound_vault_needs_the_device_secret() {
        let seed = Seed::from_phrase(ABANDON).unwrap();
        let secret = new_device_secret();
        let dev = DeviceSecret { secret: secret.clone(), wrapped: vec![1, 2, 3] };
        let vault = Vault::seal_with(&seed, "123456", fast(), Some(&dev)).unwrap();
        assert_eq!(vault.version, 2);
        let vault: Vault = serde_json::from_str(&serde_json::to_string(&vault).unwrap()).unwrap();
        assert_eq!(vault.device.as_deref(), Some(&[1u8, 2, 3][..]));
        assert_eq!(vault.open_with("123456", Some(&secret)).unwrap().phrase().as_str(), ABANDON);
        // Right passcode, no device secret (a copied file): nothing.
        assert!(vault.open("123456").is_err());
        assert!(vault.open_with("123456", Some(&[7u8; 32])).is_err());
        // Old vaults still open.
        let v1: Vault = serde_json::from_str(&serde_json::to_string(&Vault::seal(&seed, "123456", fast()).unwrap()).unwrap()).unwrap();
        assert!(v1.device.is_none());
        assert_eq!(v1.open("123456").unwrap().phrase().as_str(), ABANDON);
    }

    #[test]
    fn phrase_is_normalised() {
        let messy = format!("  {}  ", ABANDON.to_uppercase().replace(' ', "   "));
        assert_eq!(
            Seed::from_phrase(&messy).unwrap().phrase().as_str(),
            ABANDON
        );
    }

    /// Official BIP85 test vector (BIP39, English, 12 words, index 0).
    #[test]
    fn bip85_vector() {
        // Master key from the BIP85 spec test vectors.
        let xprv = Xpriv::from_str(
            "xprv9s21ZrQH143K2LBWUUQRFXhucrQqBpKdRRxNVq2zBqsx8HVqFk2uYo8kmbaLLHRdqtQpUm98uKfu3vca1LqdGhUtyoFnCNkfmXRyPXLjbKb",
        )
        .unwrap();
        let m = bip85_from_master(&xprv, 0).unwrap();
        assert_eq!(
            m.phrase().as_str(),
            "girl mad pet galaxy egg matter matrix prison refuse sense ordinary nose"
        );
    }

    #[test]
    fn children_differ_and_are_deterministic() {
        let seed = Seed::from_phrase(ABANDON).unwrap();
        let a = seed.bip85_child(child::SPARK).unwrap().phrase();
        let b = seed.bip85_child(child::ECASH).unwrap().phrase();
        assert_ne!(*a, *b);
        assert_eq!(*a, *seed.bip85_child(child::SPARK).unwrap().phrase());
    }
}

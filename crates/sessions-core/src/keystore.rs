//! One recovery phrase for everything.
//!
//! * On-chain accounts use the master phrase directly (BIP84/86/48), so the
//!   phrase restores in Sparrow, Nunchuk, etc.
//! * Spark and ecash each get their **own BIP85 child phrase**, so a bug or
//!   leak in one SDK can never expose on-chain keys, while the single master
//!   phrase still recovers every account.
//!
//! At rest the phrase is encrypted with XChaCha20-Poly1305 under a key
//! stretched from the user's passcode with Argon2id.

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
        let mut salt = vec![0u8; 16];
        let mut nonce = vec![0u8; 24];
        rand::rngs::OsRng.fill_bytes(&mut salt);
        rand::rngs::OsRng.fill_bytes(&mut nonce);

        let key = derive_key(passcode, &salt, kdf)?;
        let cipher = XChaCha20Poly1305::new_from_slice(&key[..])
            .map_err(|e| Error::Crypto(e.to_string()))?;
        let phrase = seed.phrase();
        let ciphertext = cipher
            .encrypt(XNonce::from_slice(&nonce), phrase.as_bytes())
            .map_err(|e| Error::Crypto(e.to_string()))?;

        Ok(Vault {
            version: 1,
            kdf,
            salt,
            nonce,
            ciphertext,
        })
    }

    pub fn open(&self, passcode: &str) -> Result<Seed> {
        let key = derive_key(passcode, &self.salt, self.kdf)?;
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

fn derive_key(passcode: &str, salt: &[u8], kdf: KdfParams) -> Result<Zeroizing<[u8; 32]>> {
    let params = Params::new(kdf.m_cost_kib, kdf.t_cost, kdf.p_cost, Some(32))
        .map_err(|e| Error::Crypto(e.to_string()))?;
    let mut key = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passcode.as_bytes(), salt, &mut key[..])
        .map_err(|e| Error::Crypto(e.to_string()))?;
    Ok(key)
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

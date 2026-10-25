//! Every key the backup uses, derived (never stored) from one of two roots:
//!
//! * the passkey's **PRF output** (32 bytes from the WebAuthn PRF extension,
//!   evaluated with [`prf_salt`]): the passkey wallet's seed, the key of the
//!   seed envelope and the Nostr key that publishes it;
//! * the **wallet seed**, whatever its origin (phrase or passkey): the key of
//!   the state backup and the Nostr key that signs and locates it. Anyone
//!   holding the seed can find and read the state backup.
//!
//! All of it is HKDF-SHA256 with a per-root salt and a distinct `info` per
//! key, so no two keys can ever coincide, and nothing collides with BIP85
//! children the user may derive elsewhere from the same seed.

use std::sync::OnceLock;

use bitcoin::bip32::Xpriv;
use bitcoin::hex::DisplayHex;
use bitcoin::secp256k1::{All, Keypair, Secp256k1, SecretKey};
use hkdf::Hkdf;
use sessions_core::{Error, Result};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// Hashed into the PRF salt. Never change: it would change every passkey wallet.
pub const PRF_SALT_LABEL: &str = "sessions-wallet/prf/v1";
const PASSKEY_SALT: &[u8] = b"sessions-wallet/passkey/v1";
const STATE_SALT: &[u8] = b"sessions-wallet/state/v1";

/// The fixed salt the app asks the passkey to evaluate its PRF with:
/// SHA-256("sessions-wallet/prf/v1").
pub fn prf_salt() -> [u8; 32] {
    Sha256::digest(PRF_SALT_LABEL.as_bytes()).into()
}

pub(crate) fn secp() -> &'static Secp256k1<All> {
    static SECP: OnceLock<Secp256k1<All>> = OnceLock::new();
    SECP.get_or_init(Secp256k1::new)
}

fn expand<const N: usize>(salt: &[u8], ikm: &[u8], info: &str) -> Zeroizing<[u8; N]> {
    let mut out = Zeroizing::new([0u8; N]);
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info.as_bytes(), &mut out[..])
        .expect("HKDF output length is at most 255 hashes");
    out
}

/// A Nostr identity (secp256k1, BIP340 x-only public key).
pub struct NostrKey {
    keypair: Keypair,
}

impl NostrKey {
    fn from_bytes(bytes: &[u8; 32]) -> Result<Self> {
        // Fails for 0 or ≥ n: probability ~2⁻¹²⁸.
        let sk = SecretKey::from_slice(bytes).map_err(|e| Error::Crypto(e.to_string()))?;
        Ok(NostrKey {
            keypair: Keypair::from_secret_key(secp(), &sk),
        })
    }

    /// A key from its 32 secret bytes (fails for 0 or ≥ the curve order).
    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Result<Self> {
        Self::from_bytes(bytes)
    }

    /// A fresh random key (OS randomness).
    pub fn generate() -> Self {
        loop {
            let mut bytes = Zeroizing::new([0u8; 32]);
            rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes[..]);
            if let Ok(k) = Self::from_bytes(&bytes) {
                return k;
            }
        }
    }

    /// The 32 secret bytes. Never log them.
    pub fn secret_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.keypair.secret_bytes())
    }

    /// The secret as lowercase hex (64 chars). Never log it.
    pub fn secret_hex(&self) -> Zeroizing<String> {
        Zeroizing::new(self.keypair.secret_bytes().to_lower_hex_string())
    }

    /// Hex x-only public key: the Nostr `pubkey`.
    pub fn public_hex(&self) -> String {
        self.keypair
            .x_only_public_key()
            .0
            .serialize()
            .to_lower_hex_string()
    }

    pub(crate) fn keypair(&self) -> &Keypair {
        &self.keypair
    }
}

impl Drop for NostrKey {
    fn drop(&mut self) {
        self.keypair.non_secure_erase();
    }
}

/// Keys from a passkey's PRF output.
pub struct PasskeyKeys {
    /// 16 bytes → the 12-word phrase of a passkey wallet.
    pub seed_entropy: Zeroizing<[u8; 16]>,
    /// Encrypts the seed envelope (a phrase backed up with the passkey).
    pub envelope_key: Zeroizing<[u8; 32]>,
    /// Publishes and finds the envelope.
    pub nostr: NostrKey,
}

impl PasskeyKeys {
    pub fn from_prf(prf: &[u8]) -> Result<Self> {
        if prf.len() < 32 {
            return Err(Error::Crypto(format!(
                "passkey PRF output is {} bytes, expected 32",
                prf.len()
            )));
        }
        Ok(PasskeyKeys {
            seed_entropy: expand(PASSKEY_SALT, prf, "seed"),
            envelope_key: expand(PASSKEY_SALT, prf, "envelope-key"),
            nostr: NostrKey::from_bytes(&expand(PASSKEY_SALT, prf, "nostr-envelope"))?,
        })
    }
}

/// Keys from the wallet seed (its BIP32 master key).
pub struct StateKeys {
    /// Encrypts the state backup.
    pub state_key: Zeroizing<[u8; 32]>,
    /// Signs and locates the state backup.
    pub nostr: NostrKey,
}

impl StateKeys {
    /// From the BIP32 master key (network doesn't matter: only the private
    /// key and chain code are used).
    pub fn from_master(master: &Xpriv) -> Result<Self> {
        let mut ikm = Zeroizing::new([0u8; 64]);
        ikm[..32].copy_from_slice(&master.private_key.secret_bytes());
        ikm[32..].copy_from_slice(&master.chain_code.to_bytes());
        Ok(StateKeys {
            state_key: expand(STATE_SALT, &ikm[..], "state-key"),
            nostr: NostrKey::from_bytes(&expand(STATE_SALT, &ikm[..], "nostr-state"))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessions_core::keystore::Seed;
    use sessions_core::Network;

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn hex(b: &[u8]) -> String {
        b.to_lower_hex_string()
    }

    #[test]
    fn prf_salt_vector() {
        assert_eq!(
            hex(&prf_salt()),
            "7cf0bfad7441129030f2c7e018317a478ebfda94a8b4a0455006c4daf2b95130"
        );
    }

    /// Fixed vectors: these must never change once shipped, or passkey
    /// wallets and backups would no longer be found.
    #[test]
    fn passkey_vectors() {
        let k = PasskeyKeys::from_prf(&[7u8; 32]).unwrap();
        let again = PasskeyKeys::from_prf(&[7u8; 32]).unwrap();
        assert_eq!(*k.seed_entropy, *again.seed_entropy);
        assert_eq!(*k.envelope_key, *again.envelope_key);
        assert_eq!(k.nostr.public_hex(), again.nostr.public_hex());

        let seed = Seed::from_entropy(&k.seed_entropy[..]).unwrap();
        assert_eq!(seed.word_count(), 12);
        assert_eq!(hex(&k.seed_entropy[..]), PRF7_SEED);
        assert_eq!(hex(&k.envelope_key[..]), PRF7_ENVELOPE);
        assert_eq!(k.nostr.public_hex(), PRF7_NOSTR);
        assert_eq!(seed.phrase().as_str(), PRF7_PHRASE);

        // A different PRF output gives unrelated keys.
        let other = PasskeyKeys::from_prf(&[8u8; 32]).unwrap();
        assert_ne!(*k.seed_entropy, *other.seed_entropy);
        assert_ne!(k.nostr.public_hex(), other.nostr.public_hex());
        assert!(PasskeyKeys::from_prf(&[1u8; 16]).is_err());
    }

    #[test]
    fn domain_separation() {
        let k = PasskeyKeys::from_prf(&[7u8; 32]).unwrap();
        // seed ≠ envelope key ≠ nostr key, even on shared prefixes.
        assert_ne!(k.seed_entropy[..], k.envelope_key[..16]);
        assert_ne!(*k.envelope_key, k.nostr.keypair().secret_bytes());

        // The same 32 bytes as PRF output and as state root give different keys.
        let master = Seed::from_phrase(ABANDON)
            .unwrap()
            .master_xpriv(Network::Bitcoin)
            .unwrap();
        let s = StateKeys::from_master(&master).unwrap();
        assert_ne!(*s.state_key, *k.envelope_key);
        assert_ne!(*s.state_key, s.nostr.keypair().secret_bytes());
        assert_ne!(s.nostr.public_hex(), k.nostr.public_hex());
    }

    #[test]
    fn secret_round_trip() {
        let k = NostrKey::generate();
        let again = NostrKey::from_secret_bytes(&k.secret_bytes()).unwrap();
        assert_eq!(k.public_hex(), again.public_hex());
        assert_eq!(k.secret_hex().len(), 64);
        assert_ne!(k.public_hex(), NostrKey::generate().public_hex());
        assert!(NostrKey::from_secret_bytes(&[0u8; 32]).is_err());
        // Secret 1 → the generator's x coordinate.
        let mut one = [0u8; 32];
        one[31] = 1;
        assert_eq!(
            NostrKey::from_secret_bytes(&one).unwrap().public_hex(),
            "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
        );
    }

    #[test]
    fn state_vectors() {
        let seed = Seed::from_phrase(ABANDON).unwrap();
        let a = StateKeys::from_master(&seed.master_xpriv(Network::Bitcoin).unwrap()).unwrap();
        // Network only changes the xprv version bytes, not the keys.
        let b = StateKeys::from_master(&seed.master_xpriv(Network::Testnet).unwrap()).unwrap();
        assert_eq!(*a.state_key, *b.state_key);
        assert_eq!(hex(&a.state_key[..]), ABANDON_STATE_KEY);
        assert_eq!(a.nostr.public_hex(), ABANDON_STATE_NOSTR);

        let other = Seed::from_phrase(
            "legal winner thank year wave sausage worth useful legal winner thank yellow",
        )
        .unwrap();
        let c = StateKeys::from_master(&other.master_xpriv(Network::Bitcoin).unwrap()).unwrap();
        assert_ne!(*a.state_key, *c.state_key);
    }

    // Vectors computed independently (Python hmac/hashlib HKDF per RFC 5869,
    // BIP39 wordlist, textbook secp256k1).
    const PRF7_SEED: &str = "d6339810d14a2cfabdf1a10c282f05e5";
    const PRF7_ENVELOPE: &str = "5d1b18aeb855d1455ee0d95f1bcb9166066d752daf6ab6fad992c76d721ffbd7";
    const PRF7_NOSTR: &str = "003a80bdcc46533cecafac1fc0ec82ff4836f2dcb1b5aca8e83f2321af0825c8";
    const PRF7_PHRASE: &str =
        "stomach orphan aerobic penalty people later water crouch around door scrap skull";
    const ABANDON_STATE_KEY: &str =
        "d54af2d71c21cefacd048da23405cb4dcd97abe6c1698847cd5d609f95ff7c1d";
    const ABANDON_STATE_NOSTR: &str =
        "8005117957042f8e82fc63fd42ac6150772a9243d9eab600f235ccb531ac5fba";
}

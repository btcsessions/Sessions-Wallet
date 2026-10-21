//! Deterministic swap keys.
//!
//! The caller hands us the wallet's BIP85 child phrase reserved for swaps.
//! We treat that phrase exactly like Boltz's own "rescue key":
//! keys are `m/44/0/0/0/{index}` and each preimage is
//! `sha256(private key)`, which is what `boltz-client`'s `SwapMasterKey` and
//! `Preimage::from_swap_key` implement. So the same phrase works on
//! <https://boltz.exchange/rescue/external> and with Boltz's
//! `/swap/restore` API, and every swap is recoverable from the seed.

use boltz_client::bitcoin::secp256k1::Keypair;
use boltz_client::network::Network as BoltzNetwork;
use boltz_client::util::secrets::{Preimage, SwapMasterKey};
use boltz_client::PublicKey;
use sessions_core::{Error, Result};

pub struct SwapKeys {
    master: SwapMasterKey,
}

impl SwapKeys {
    pub fn new(phrase: &str, network: BoltzNetwork) -> Result<Self> {
        let normalised = phrase
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let master = SwapMasterKey::from_mnemonic(&normalised, None, network)
            .map_err(|e| Error::InvalidMnemonic(e.to_string()))?;
        Ok(Self { master })
    }

    pub fn keypair(&self, index: u64) -> Result<Keypair> {
        self.master
            .derive_swapkey(index)
            .map_err(|e| Error::Crypto(e.to_string()))
    }

    pub fn public_key(&self, index: u64) -> Result<PublicKey> {
        Ok(PublicKey {
            compressed: true,
            inner: self.keypair(index)?.public_key(),
        })
    }

    /// `sha256(private key at index)`, the Boltz convention for restorable swaps.
    pub fn preimage(&self, index: u64) -> Result<Preimage> {
        Ok(Preimage::from_swap_key(&self.keypair(index)?))
    }

    /// The xpub Boltz's restore endpoints scan, with derivation path `"m"`.
    pub fn xpub(&self) -> String {
        self.master.get_master_xpub().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BIP85 child of the "bacon ×24" phrase at Boltz's index; vectors from boltz-client.
    const CHILD: &str =
        "velvet engage shaft effort clarify annual protect client only surround sock gain";

    #[test]
    fn matches_boltz_rescue_key_vectors() {
        let k = SwapKeys::new(CHILD, BoltzNetwork::Mainnet).unwrap();
        assert_eq!(
            k.xpub(),
            "xpub6FCHSTDQB1YT3JF1NX5q2dPqYmTzDsPzicckjHf2HNjL6VEPTZAtiRcCRixiSpfKinRfWGFQ5b1yw74jzsBd1hkm25864ZpH8uND7rjKjiV"
        );
        assert_eq!(
            k.preimage(0).unwrap().to_string().unwrap(),
            "f19d42c70bf00267b6c5dcfe6e1094386f8c72389f1ced91e0132d1502bbd244"
        );
    }

    #[test]
    fn deterministic_and_distinct() {
        let a = SwapKeys::new(CHILD, BoltzNetwork::Mainnet).unwrap();
        // Messy input normalises to the same phrase.
        let b = SwapKeys::new(
            &format!("  {}  ", CHILD.to_uppercase().replace(' ', "   ")),
            BoltzNetwork::Mainnet,
        )
        .unwrap();
        for i in [0u64, 1, 7, 1_000] {
            assert_eq!(a.public_key(i).unwrap(), b.public_key(i).unwrap());
            assert_eq!(a.preimage(i).unwrap(), b.preimage(i).unwrap());
        }
        assert_ne!(a.public_key(0).unwrap(), a.public_key(1).unwrap());
        assert_ne!(a.preimage(0).unwrap().sha256, a.preimage(1).unwrap().sha256);
        // Key derivation doesn't depend on the network flag.
        let r = SwapKeys::new(CHILD, BoltzNetwork::Regtest).unwrap();
        assert_eq!(a.public_key(3).unwrap(), r.public_key(3).unwrap());
    }

    #[test]
    fn rejects_bad_phrase() {
        assert!(matches!(
            SwapKeys::new("not a phrase", BoltzNetwork::Mainnet),
            Err(Error::InvalidMnemonic(_))
        ));
    }
}

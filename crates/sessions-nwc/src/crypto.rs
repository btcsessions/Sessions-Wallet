//! Shared secp256k1 helpers for NIP-04 and NIP-44.

use std::sync::OnceLock;

use bitcoin::hex::FromHex;
use bitcoin::key::Parity;
use bitcoin::secp256k1::{ecdh, All, PublicKey, Secp256k1, SecretKey, XOnlyPublicKey};
use zeroize::Zeroizing;

use crate::Error;

pub(crate) fn secp() -> &'static Secp256k1<All> {
    static SECP: OnceLock<Secp256k1<All>> = OnceLock::new();
    SECP.get_or_init(Secp256k1::new)
}

/// A secret key from 64 hex chars (range-checked).
pub fn secret_key_from_hex(hex: &str) -> Result<SecretKey, Error> {
    let bytes = Zeroizing::new(
        <[u8; 32]>::from_hex(hex)
            .map_err(|_| Error::Key("secret key is not 32-byte hex".into()))?,
    );
    SecretKey::from_slice(&bytes[..]).map_err(|_| Error::Key("secret key out of range".into()))
}

/// An x-only (BIP340) public key from 64 hex chars (must be on the curve).
pub fn xonly_from_hex(hex: &str) -> Result<XOnlyPublicKey, Error> {
    let bytes = <[u8; 32]>::from_hex(hex)
        .map_err(|_| Error::Key("public key is not 32-byte hex".into()))?;
    XOnlyPublicKey::from_slice(&bytes).map_err(|_| Error::Key("public key not on curve".into()))
}

/// Hex x-only public key of a secret key.
pub fn public_hex(sk: &SecretKey) -> String {
    use bitcoin::hex::DisplayHex;
    sk.x_only_public_key(secp())
        .0
        .serialize()
        .to_lower_hex_string()
}

/// Unhashed x coordinate of `sk · P` (NIP-04 and NIP-44 both use this).
/// The parity of `P` doesn't matter: `sk·P` and `sk·(−P)` share their x.
pub(crate) fn shared_x(sk: &SecretKey, pk: &XOnlyPublicKey) -> Zeroizing<[u8; 32]> {
    let full = PublicKey::from_x_only_public_key(*pk, Parity::Even);
    let point = Zeroizing::new(ecdh::shared_secret_point(&full, sk));
    let mut x = Zeroizing::new([0u8; 32]);
    x.copy_from_slice(&point[..32]);
    x
}

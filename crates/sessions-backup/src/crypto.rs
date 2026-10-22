//! Authenticated encryption and compression for what goes on the relays.
//!
//! * `seal`: XChaCha20-Poly1305, random 24-byte nonce, output `nonce ‖ ciphertext`.
//!   The associated data names what is sealed (state or envelope), so one
//!   can never be passed off as the other.
//! * `pack`: deflate, then pad to a multiple of [`PAD`] bytes so the size on
//!   the relay only hints at how much is in the backup.

use std::io::{Read, Write};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use sessions_core::{Error, Result};
use zeroize::Zeroizing;

const NONCE: usize = 24;
/// Padding bucket for packed payloads.
pub const PAD: usize = 1024;
/// Refuse to inflate more than this (a corrupt or hostile payload).
const MAX_PLAIN: u64 = 16 * 1024 * 1024;

pub fn seal(key: &[u8; 32], aad: &[u8], plain: &[u8]) -> Result<Vec<u8>> {
    let cipher =
        XChaCha20Poly1305::new_from_slice(key).map_err(|e| Error::Crypto(e.to_string()))?;
    let mut nonce = [0u8; NONCE];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
        .map_err(|e| Error::Crypto(e.to_string()))?;
    let mut out = Vec::with_capacity(NONCE + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn open(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    if sealed.len() < NONCE + 16 {
        return Err(Error::Crypto("backup is too short".into()));
    }
    let cipher =
        XChaCha20Poly1305::new_from_slice(key).map_err(|e| Error::Crypto(e.to_string()))?;
    let (nonce, ct) = sealed.split_at(NONCE);
    cipher
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
        .map(Zeroizing::new)
        .map_err(|_| Error::Crypto("backup doesn't decrypt with this key".into()))
}

/// `u32 BE length ‖ deflate(plain) ‖ zero padding` to a multiple of [`PAD`].
pub fn pack(plain: &[u8]) -> Result<Vec<u8>> {
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(plain)
        .map_err(|e| Error::Storage(e.to_string()))?;
    let deflated = enc.finish().map_err(|e| Error::Storage(e.to_string()))?;
    let len =
        u32::try_from(deflated.len()).map_err(|_| Error::Storage("backup too large".into()))?;
    let mut out = Vec::with_capacity(4 + deflated.len() + PAD);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&deflated);
    out.resize(out.len().div_ceil(PAD) * PAD, 0);
    Ok(out)
}

pub fn unpack(packed: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let bad = || Error::Storage("backup is damaged".into());
    let len = u32::from_be_bytes(
        packed
            .get(..4)
            .ok_or_else(bad)?
            .try_into()
            .map_err(|_| bad())?,
    );
    let body = packed.get(4..4 + len as usize).ok_or_else(bad)?;
    let mut out = Zeroizing::new(Vec::new());
    flate2::read::DeflateDecoder::new(body)
        .take(MAX_PLAIN + 1)
        .read_to_end(&mut out)
        .map_err(|_| bad())?;
    if out.len() as u64 > MAX_PLAIN {
        return Err(bad());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_round_trip_and_wrong_key_or_aad() {
        let key = [1u8; 32];
        let sealed = seal(&key, b"state", b"hello").unwrap();
        assert_eq!(&open(&key, b"state", &sealed).unwrap()[..], b"hello");
        // Fresh nonce each time.
        assert_ne!(sealed, seal(&key, b"state", b"hello").unwrap());
        assert!(open(&[2u8; 32], b"state", &sealed).is_err());
        assert!(open(&key, b"envelope", &sealed).is_err());
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open(&key, b"state", &tampered).is_err());
        assert!(open(&key, b"state", &sealed[..20]).is_err());
    }

    #[test]
    fn pack_round_trip_pads_and_compresses() {
        let json = serde_json::to_vec(&vec!["some label"; 500]).unwrap();
        let packed = pack(&json).unwrap();
        assert_eq!(packed.len() % PAD, 0);
        assert!(packed.len() < json.len() / 4);
        assert_eq!(&unpack(&packed).unwrap()[..], &json[..]);
        assert_eq!(pack(b"{}").unwrap().len(), PAD);
        assert!(unpack(&[0, 0, 0, 9, 1]).is_err());
        assert!(unpack(&[]).is_err());
    }
}

//! The wallet's own Nostr identity, for group vaults.
//!
//! The key comes from the wallet seed through BIP85 (HEX application,
//! m/83696968'/128169'/32'/index'), so the one seed backup covers it and the
//! user never pastes an nsec. It's a *wallet* key, separate from their social
//! npub: vault groups and their chat never show up in White Noise or any other
//! Nostr client. The social npub, when linked, is only read (who they
//! follow), never signed with here.

use bitcoin::bech32::{self, Bech32, Hrp};
use bitcoin::bip32::{ChildNumber, DerivationPath, Xpriv};
use bitcoin::hashes::{hmac, sha512, Hash, HashEngine};
use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use sessions_core::{Error, Result};
use tauri::State;

use crate::state::AppState;

/// BIP85 purpose and the HEX application number.
const BIP85: u32 = 83_696_968;
const BIP85_HEX: u32 = 128_169;

/// Who may send this wallet a vault invite without a code or link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InviteFilter {
    /// People the linked npub follows, contacts, and past co-signers.
    #[default]
    Follows,
    /// Only people who follow each other with the linked npub.
    Mutual,
    /// Nobody: invites need a code, link or QR.
    CodeOnly,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NostrSettings {
    /// BIP85 index of the wallet key. Bumping it makes a fresh identity.
    #[serde(default)]
    pub index: u32,
    /// The user's social npub (hex public key), read for their follow list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub social: Option<String>,
    #[serde(default)]
    pub invites: InviteFilter,
    /// Publish "my wallet key is X" from the social npub so people they
    /// follow can invite them without swapping a code first. Off by default:
    /// it shows publicly that this npub uses Sessions Wallet vaults.
    #[serde(default)]
    pub discoverable: bool,
}

/// BIP85 HEX: `num_bytes` of entropy from the seed at `index`.
pub fn bip85_hex(master: &Xpriv, num_bytes: u32, index: u32) -> Result<Vec<u8>> {
    if !(16..=64).contains(&num_bytes) {
        return Err(Error::Invalid("BIP85 HEX needs 16 to 64 bytes".into()));
    }
    let secp = Secp256k1::new();
    let path: DerivationPath = [BIP85, BIP85_HEX, num_bytes, index]
        .into_iter()
        .map(|i| ChildNumber::from_hardened_idx(i).map_err(|e| Error::Invalid(e.to_string())))
        .collect::<Result<Vec<_>>>()?
        .into();
    let child = master
        .derive_priv(&secp, &path)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let mut engine = hmac::HmacEngine::<sha512::Hash>::new(b"bip-entropy-from-k");
    engine.input(&child.private_key.secret_bytes());
    let entropy = hmac::Hmac::<sha512::Hash>::from_engine(engine);
    Ok(entropy.to_byte_array()[..num_bytes as usize].to_vec())
}

/// The wallet's Nostr secret key at `index`.
pub fn wallet_secret(master: &Xpriv, index: u32) -> Result<SecretKey> {
    let bytes = bip85_hex(master, 32, index)?;
    // Out of range has odds of about 2^-128; treat it as an error rather
    // than silently picking another index.
    SecretKey::from_slice(&bytes).map_err(|e| Error::Invalid(e.to_string()))
}

/// The x-only public key (hex) for a secret key.
pub fn public_hex(secret: &SecretKey) -> String {
    let secp = Secp256k1::new();
    let (xonly, _) = Keypair::from_secret_key(&secp, secret).x_only_public_key();
    xonly.to_string()
}

pub fn npub(pubkey_hex: &str) -> Result<String> {
    let bytes = hex_32(pubkey_hex).ok_or_else(|| Error::Invalid("Not a Nostr public key".into()))?;
    let hrp = Hrp::parse("npub").expect("valid hrp");
    bech32::encode::<Bech32>(hrp, &bytes).map_err(|e| Error::Invalid(e.to_string()))
}

fn hex_32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// A public key from an npub, `nostr:npub…` or 64-char hex. Returns hex.
pub fn parse_pubkey(input: &str) -> Result<String> {
    let s = input.trim();
    let s = s.strip_prefix("nostr:").unwrap_or(s);
    if let Some(bytes) = hex_32(s) {
        return Ok(bytes.iter().map(|b| format!("{b:02x}")).collect());
    }
    let (hrp, data) = bech32::decode(s).map_err(|_| Error::Invalid("That isn't an npub".into()))?;
    if hrp.as_str() == "nsec" {
        return Err(Error::Invalid(
            "That's a secret key (nsec). Never share it: paste your npub instead".into(),
        ));
    }
    if hrp.as_str() != "npub" || data.len() != 32 {
        return Err(Error::Invalid("That isn't an npub".into()));
    }
    Ok(data.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityView {
    /// The wallet's npub (for vault groups).
    pub npub: String,
    pub pubkey: String,
    pub index: u32,
    /// The linked social npub.
    pub social_npub: Option<String>,
    pub invites: InviteFilter,
    pub discoverable: bool,
}

fn identity(u: &crate::state::Unlocked) -> Result<IdentityView> {
    let master = u.seed.master_xpriv(u.config.network)?;
    let s = &u.config.nostr;
    let pubkey = public_hex(&wallet_secret(&master, s.index)?);
    Ok(IdentityView {
        npub: npub(&pubkey)?,
        pubkey,
        index: s.index,
        social_npub: s.social.as_deref().map(npub).transpose()?,
        invites: s.invites,
        discoverable: s.discoverable,
    })
}

#[tauri::command]
pub async fn nostr_identity(state: State<'_, AppState>) -> Result<IdentityView> {
    state.check_screen()?;
    let guard = state.inner.lock().await;
    identity(guard.as_ref().ok_or(Error::Locked)?)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsUpdate {
    /// `Some("")` unlinks.
    pub social_npub: Option<String>,
    pub invites: Option<InviteFilter>,
    pub discoverable: Option<bool>,
}

#[tauri::command]
pub async fn nostr_update(state: State<'_, AppState>, update: SettingsUpdate) -> Result<IdentityView> {
    state.check_screen()?;
    let mut guard = state.inner.lock().await;
    let u = guard.as_mut().ok_or(Error::Locked)?;
    let mut config = u.config.clone();
    if let Some(social) = update.social_npub {
        config.nostr.social = if social.trim().is_empty() {
            None
        } else {
            let hex = parse_pubkey(&social)?;
            let own = public_hex(&wallet_secret(
                &u.seed.master_xpriv(config.network)?,
                config.nostr.index,
            )?);
            if hex == own {
                return Err(Error::Invalid(
                    "That's this wallet's own key. Link the npub you use on Nostr".into(),
                ));
            }
            Some(hex)
        };
        if config.nostr.social.is_none() {
            config.nostr.discoverable = false;
        }
    }
    if let Some(i) = update.invites {
        config.nostr.invites = i;
    }
    if let Some(d) = update.discoverable {
        if d && config.nostr.social.is_none() {
            return Err(Error::Invalid("Link your npub first".into()));
        }
        config.nostr.discoverable = d;
    }
    state.save_config(&config)?;
    u.config = config;
    identity(u)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn bip85_hex_test_vector() {
        // BIP85 HEX test vector (64 bytes, index 0).
        let master = Xpriv::from_str("xprv9s21ZrQH143K2LBWUUQRFXhucrQqBpKdRRxNVq2zBqsx8HVqFk2uYo8kmbaLLHRdqtQpUm98uKfu3vca1LqdGhUtyoFnCNkfmXRyPXLjbKb").unwrap();
        let out = bip85_hex(&master, 64, 0).unwrap();
        let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "492db4698cf3b73a5a24998aa3e9d7fa96275d85724a91e71aa2d645442f878555d078fd1f1f67e368976f04137b1f7a0d19232136ca50c44614af72b5582a5c");
    }

    #[test]
    fn npub_round_trip_and_nsec_refused() {
        let master = Xpriv::from_str("xprv9s21ZrQH143K2LBWUUQRFXhucrQqBpKdRRxNVq2zBqsx8HVqFk2uYo8kmbaLLHRdqtQpUm98uKfu3vca1LqdGhUtyoFnCNkfmXRyPXLjbKb").unwrap();
        let a = public_hex(&wallet_secret(&master, 0).unwrap());
        let b = public_hex(&wallet_secret(&master, 1).unwrap());
        assert_ne!(a, b);
        let n = npub(&a).unwrap();
        assert!(n.starts_with("npub1"));
        assert_eq!(parse_pubkey(&n).unwrap(), a);
        assert_eq!(parse_pubkey(&format!("nostr:{n}")).unwrap(), a);
        assert_eq!(parse_pubkey(&a.to_uppercase()).unwrap(), a);
        // NIP-19 example npub.
        assert_eq!(
            parse_pubkey("npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg").unwrap(),
            "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e"
        );
        let nsec = bech32::encode::<Bech32>(Hrp::parse("nsec").unwrap(), &[1u8; 32]).unwrap();
        assert!(parse_pubkey(&nsec).unwrap_err().to_string().contains("secret"));
    }
}

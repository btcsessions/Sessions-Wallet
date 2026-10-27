//! Wallet messages inside a vault group.
//!
//! Each is a custom event kind (outside the kinds Marmot reserves) with JSON
//! content. Chat uses Marmot's own chat messages. Every message carries ids
//! so receiving it twice (a resend after a fork, a retry) changes nothing.

use serde::{Deserialize, Serialize};
use sessions_core::{Error, Result};

pub const KIND_VAULT_PROPOSAL: u16 = 4601;
pub const KIND_KEY: u16 = 4602;
pub const KIND_FINALIZE: u16 = 4603;
pub const KIND_CONFIRM: u16 = 4604;
pub const KIND_SPEND: u16 = 4605;
pub const KIND_SIGNATURE: u16 = 4606;
pub const KIND_CANCEL: u16 = 4607;
pub const KIND_BROADCAST: u16 = 4608;
/// "This is me": a member's name, sent on joining.
pub const KIND_HELLO: u16 = 4609;

pub const WALLET_KINDS: [u16; 9] = [
    KIND_VAULT_PROPOSAL,
    KIND_KEY,
    KIND_FINALIZE,
    KIND_CONFIRM,
    KIND_SPEND,
    KIND_SIGNATURE,
    KIND_CANCEL,
    KIND_BROADCAST,
    KIND_HELLO,
];

/// Protocol version in every message.
pub const VERSION: u8 = 1;

/// A message as it came out of the group: the sender is authenticated by
/// the group (their wallet's Nostr public key, hex).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Msg {
    pub id: String,
    pub sender: String,
    pub kind: u16,
    pub content: String,
    pub created_at: u64,
}

/// One slot of a proposed vault: whose key goes there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Slot {
    /// The member who fills it (wallet public key, hex).
    pub holder: String,
    /// Shown to everyone: "Ben's Coldcard", "Mom".
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultProposal {
    pub id: String,
    pub name: String,
    pub threshold: u8,
    pub slots: Vec<Slot>,
    /// "bitcoin", "testnet", "signet", "regtest".
    pub network: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeySubmission {
    pub proposal: String,
    pub slot: usize,
    /// `[fingerprint/48h/0h/0h/2h]xpub…`.
    pub key: String,
    /// "Coldcard Q", "This phone"…
    #[serde(default)]
    pub device: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finalize {
    pub proposal: String,
    /// Receive descriptor, with checksum.
    pub descriptor: String,
    /// [`crate::setup::descriptor_hash`] of it.
    pub hash: String,
}

/// "I checked address #0 on my own device and it matches."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Confirm {
    pub proposal: String,
    pub hash: String,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Spend {
    pub id: String,
    /// Which vault (its descriptor hash).
    pub vault: String,
    /// Base64 PSBT, unsigned or partly signed.
    pub psbt: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Signature {
    pub spend: String,
    /// Base64 PSBT carrying the sender's signatures.
    pub psbt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cancel {
    pub spend: String,
    /// The fee bump that replaces it, when there is one.
    #[serde(default)]
    pub replacement: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Broadcast {
    pub spend: String,
    pub txid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalletMsg {
    Hello(Hello),
    Proposal(VaultProposal),
    Key(KeySubmission),
    Finalize(Finalize),
    Confirm(Confirm),
    Spend(Spend),
    Signature(Signature),
    Cancel(Cancel),
    Broadcast(Broadcast),
}

#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    v: u8,
    #[serde(flatten)]
    body: T,
}

fn enc<T: Serialize>(body: &T) -> String {
    serde_json::to_string(&Envelope { v: VERSION, body }).expect("serializable")
}

fn dec<T: for<'de> Deserialize<'de>>(content: &str) -> Result<T> {
    let e: Envelope<T> = serde_json::from_str(content)
        .map_err(|e| Error::Invalid(format!("unreadable wallet message: {e}")))?;
    if e.v != VERSION {
        return Err(Error::Invalid(format!(
            "wallet message version {} (update the app)",
            e.v
        )));
    }
    Ok(e.body)
}

impl WalletMsg {
    pub fn kind(&self) -> u16 {
        match self {
            WalletMsg::Hello(_) => KIND_HELLO,
            WalletMsg::Proposal(_) => KIND_VAULT_PROPOSAL,
            WalletMsg::Key(_) => KIND_KEY,
            WalletMsg::Finalize(_) => KIND_FINALIZE,
            WalletMsg::Confirm(_) => KIND_CONFIRM,
            WalletMsg::Spend(_) => KIND_SPEND,
            WalletMsg::Signature(_) => KIND_SIGNATURE,
            WalletMsg::Cancel(_) => KIND_CANCEL,
            WalletMsg::Broadcast(_) => KIND_BROADCAST,
        }
    }

    /// The event content.
    pub fn content(&self) -> String {
        match self {
            WalletMsg::Hello(b) => enc(b),
            WalletMsg::Proposal(b) => enc(b),
            WalletMsg::Key(b) => enc(b),
            WalletMsg::Finalize(b) => enc(b),
            WalletMsg::Confirm(b) => enc(b),
            WalletMsg::Spend(b) => enc(b),
            WalletMsg::Signature(b) => enc(b),
            WalletMsg::Cancel(b) => enc(b),
            WalletMsg::Broadcast(b) => enc(b),
        }
    }

    pub fn parse(kind: u16, content: &str) -> Result<Self> {
        Ok(match kind {
            KIND_HELLO => WalletMsg::Hello(dec(content)?),
            KIND_VAULT_PROPOSAL => WalletMsg::Proposal(dec(content)?),
            KIND_KEY => WalletMsg::Key(dec(content)?),
            KIND_FINALIZE => WalletMsg::Finalize(dec(content)?),
            KIND_CONFIRM => WalletMsg::Confirm(dec(content)?),
            KIND_SPEND => WalletMsg::Spend(dec(content)?),
            KIND_SIGNATURE => WalletMsg::Signature(dec(content)?),
            KIND_CANCEL => WalletMsg::Cancel(dec(content)?),
            KIND_BROADCAST => WalletMsg::Broadcast(dec(content)?),
            k => return Err(Error::Invalid(format!("not a wallet message kind: {k}"))),
        })
    }
}

/// A fresh random id (16 bytes, hex).
pub fn new_id() -> String {
    let mut b = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_version() {
        let m = WalletMsg::Confirm(Confirm {
            proposal: "p".into(),
            hash: "h".into(),
            address: "bc1q".into(),
        });
        let c = m.content();
        assert!(c.contains("\"v\":1"));
        assert_eq!(WalletMsg::parse(m.kind(), &c).unwrap(), m);
        let future = c.replace("\"v\":1", "\"v\":9");
        assert!(WalletMsg::parse(KIND_CONFIRM, &future).is_err());
        assert!(WalletMsg::parse(9, &c).is_err());
    }
}

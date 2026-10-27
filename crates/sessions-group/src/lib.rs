//! Group vaults: several people (or devices) sharing one multisig vault,
//! coordinated over an encrypted Marmot (MLS over Nostr) group.
//!
//! This crate is the protocol, free of any transport:
//!
//! * [`join`]: join codes. A short code (QR, link or typed) lets someone ask
//!   to join; the creator still approves every request.
//! * [`msg`]: the wallet messages members send inside the group (custom
//!   event kinds, JSON content, each with an id so a resend is harmless).
//! * [`setup`]: works out a vault's setup state from the messages: who holds
//!   which slot, the keys submitted, the finalized descriptor, confirmations.
//!
//! The group (the MLS layer) authenticates senders; this crate decides which
//! messages count (only a slot's holder can fill it, only the proposer can
//! finalize, a finalize must match the submitted keys).

pub mod join;
pub mod link;
pub mod msg;
pub mod setup;

pub use msg::{Msg, WalletMsg};
pub use setup::{SetupState, SlotState};

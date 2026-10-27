//! Passkey wallets and the encrypted wallet-state backup for Sessions Wallet.
//!
//! * [`keys`]: everything derived from a passkey's PRF output or the seed.
//! * [`envelope`]: a recovery phrase backed up with a passkey.
//! * [`state`]: the state snapshot (accounts, labels, layout, settings),
//!   sealed and chunked into Nostr events.
//! * [`relay`]: a minimal Nostr relay client.
//! * [`schedule`]: when to upload.
//!
//! Storage is Nostr relays: no account, several independent copies. Events
//! are NIP-78 app data (kind 30078, parameterized replaceable), so each relay
//! keeps only the newest per `d` tag.

pub mod crypto;
pub mod envelope;
pub mod keys;
pub mod merge;
pub mod nostr;
pub mod relay;
pub mod schedule;
pub mod state;

/// NIP-78 arbitrary app data (parameterized replaceable).
pub const KIND_APP_DATA: u16 = 30078;
/// `d` tag of the state backup (chunk *i* > 0: `…/i`).
pub const STATE_D: &str = "sessions-wallet/state/v1";
/// `d` tag of the seed envelope.
pub const ENVELOPE_D: &str = "sessions-wallet/envelope/v1";

/// Public relays the backup goes to unless the user picks others. Checked
/// (October 2026) to accept kind 30078 events of ~44 KB from a brand-new key
/// and serve them back. Left out: relays that refuse larger events from keys
/// outside their web of trust (offchain.pub, nostr.bitcoiner.social), paid
/// relays, and ones that were down (relay.nostr.band).
pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.primal.net",
    "wss://nostr.mom",
    "wss://relay.snort.social",
    "wss://nostr-pub.wellorder.net",
];

pub fn default_relays() -> Vec<String> {
    DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect()
}

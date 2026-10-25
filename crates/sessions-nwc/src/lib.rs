//! Nostr Wallet Connect (NIP-47) wallet service for Sessions Wallet.
//!
//! Lets Nostr apps (Primal, Damus, Amethyst, Zeus, …) use the wallet's
//! Lightning account through a `nostr+walletconnect://` URI, each with its own
//! keys, permissions and spending budget.
//!
//! * [`connection`]: the persisted [`Connection`] model and the URI.
//! * [`budget`]: per-payment and per-period caps.
//! * [`wallet::NwcWallet`]: what the app implements over its Lightning account.
//! * [`handler::WalletService`]: request handling (decrypt, authorise,
//!   budget, dispatch, encrypt, sign), testable without relays.
//! * [`service::start`]: the relay runner (subscriptions, info events,
//!   notifications, reconnects).
//! * [`nip44`], [`nip04`]: payload encryption. [`client`]: the client side
//!   (tests, self-checks).
//!
//! Spec: NIP-47 (core) plus the NWC extensions this implements: 02
//! (notifications), 05 (`list_transactions`) and 13 (`get_budget`).

pub mod budget;
pub mod client;
pub mod connection;
mod crypto;
pub mod handler;
pub mod nip04;
pub mod nip44;
pub mod protocol;
pub mod service;
pub mod wallet;

pub use budget::{Budget, BudgetPeriod, BudgetUsage};
pub use connection::{
    Connection, ConnectionOptions, ConnectionUri, NewConnection, Permission, SecretHex,
};
pub use crypto::{public_hex, secret_key_from_hex, xonly_from_hex};
pub use handler::{ConnectionStore, Ignored, Outcome, WalletService};
pub use protocol::*;
pub use service::{start, RelayStatus, ServiceConfig, ServiceHandle};
pub use wallet::NwcWallet;

/// Info event (replaceable): the methods a connection supports.
pub const KIND_INFO: u16 = 13194;
/// Request (ephemeral), client → wallet service.
pub const KIND_REQUEST: u16 = 23194;
/// Response (ephemeral), wallet service → client.
pub const KIND_RESPONSE: u16 = 23195;
/// Notification encrypted with NIP-04 (legacy clients).
pub const KIND_NOTIFICATION_NIP04: u16 = 23196;
/// Notification encrypted with NIP-44.
pub const KIND_NOTIFICATION_NIP44: u16 = 23197;

/// The relay new connections use unless the user picks another.
///
/// Most NWC clients only use the first `relay` of the URI, so one good relay
/// matters more than several. Primal's relay is large, free, takes events
/// from new keys without proof of work or payment, doesn't drop idle
/// connections that ping, and passes ephemeral kinds (23194/23195) through.
/// It is also the relay the Primal app (a likely first client) already uses.
pub const DEFAULT_RELAY: &str = "wss://relay.primal.net";

/// A fallback the app can offer ("add a second relay").
pub const FALLBACK_RELAY: &str = "wss://relay.damus.io";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Crypto(String),
    #[error("{0}")]
    Key(String),
    #[error("{0}")]
    Invalid(String),
}

/// Unix seconds.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

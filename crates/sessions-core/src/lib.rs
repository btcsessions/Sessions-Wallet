//! Sessions Wallet core.
//!
//! Everything money-related in the app goes through three ideas:
//!
//! * **Accounts** – a place that holds sats (on-chain savings, a multisig vault,
//!   a hardware wallet, a Spark/Lightning spending balance, an ecash mint).
//!   Each account is backed by a [`Rail`] implementation in its own crate.
//! * **Methods** – the ways sats can arrive or leave (a bitcoin address, a
//!   BOLT11 invoice, a Spark address, a Liquid address, a Cashu token…).
//! * **Moves** – the [`planner`] finds a route between any two accounts by
//!   matching what the sender can pay with what the receiver can accept,
//!   hopping through an intermediate account or swap provider when needed.
//!
//! The UI never talks to a rail directly; it talks to [`Wallet`].

pub mod error;
pub mod keystore;
pub mod modules;
pub mod parse;
pub mod planner;
pub mod private;
pub mod rail;
pub mod types;
pub mod wallet;

pub use error::{Error, Result};
pub use rail::{Rail, SwapProvider};
pub use types::*;
pub use private::PrivatePay;
pub use wallet::Wallet;

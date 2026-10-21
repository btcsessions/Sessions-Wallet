use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("wrong passcode")]
    WrongPasscode,
    #[error("the wallet is locked")]
    Locked,
    #[error("invalid recovery phrase: {0}")]
    InvalidMnemonic(String),
    #[error("account not found: {0}")]
    AccountNotFound(String),
    #[error("this account can't {0}")]
    Unsupported(String),
    #[error("not enough funds: need {needed} sats, have {available} sats")]
    InsufficientFunds { needed: u64, available: u64 },
    #[error("couldn't recognise that payment request")]
    UnrecognisedRequest,
    #[error("no route between these accounts")]
    NoRoute,
    #[error("amount required")]
    AmountRequired,
    #[error("network mismatch: {0}")]
    Network(String),
    #[error("{rail} error: {message}")]
    Rail { rail: String, message: String },
    #[error("storage error: {0}")]
    Storage(String),
    #[error("crypto error: {0}")]
    Crypto(String),
}

impl Error {
    pub fn rail(rail: impl Into<String>, e: impl std::fmt::Display) -> Self {
        Error::Rail { rail: rail.into(), message: e.to_string() }
    }
}

impl serde::Serialize for Error {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

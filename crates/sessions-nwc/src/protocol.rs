//! NIP-47 wire types: methods, error codes, transactions, results.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The methods this service knows. Anything else gets `NOT_IMPLEMENTED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Method {
    PayInvoice,
    MakeInvoice,
    LookupInvoice,
    GetBalance,
    GetInfo,
    ListTransactions,
    /// NWC-13.
    GetBudget,
}

impl Method {
    pub const ALL: [Method; 7] = [
        Method::PayInvoice,
        Method::GetBalance,
        Method::GetBudget,
        Method::GetInfo,
        Method::MakeInvoice,
        Method::LookupInvoice,
        Method::ListTransactions,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Method::PayInvoice => "pay_invoice",
            Method::MakeInvoice => "make_invoice",
            Method::LookupInvoice => "lookup_invoice",
            Method::GetBalance => "get_balance",
            Method::GetInfo => "get_info",
            Method::ListTransactions => "list_transactions",
            Method::GetBudget => "get_budget",
        }
    }

    pub fn parse(s: &str) -> Option<Method> {
        Method::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

/// NIP-47 error codes (core, plus the per-method ones).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    RateLimited,
    NotImplemented,
    InsufficientBalance,
    QuotaExceeded,
    Restricted,
    Unauthorized,
    Internal,
    UnsupportedEncryption,
    Other,
    /// `pay_invoice`.
    PaymentFailed,
    /// `lookup_invoice`.
    NotFound,
}

/// An error as sent to the client: `{ "code": …, "message": … }`.
/// Wallet implementations return it to choose the code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct NwcError {
    pub code: ErrorCode,
    pub message: String,
}

impl NwcError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        NwcError {
            code,
            message: message.into(),
        }
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
    pub fn other(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Other, message)
    }
    pub fn payment_failed(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::PaymentFailed, message)
    }
    pub fn insufficient_balance(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InsufficientBalance, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }
    pub fn restricted(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Restricted, message)
    }
    pub fn quota_exceeded(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::QuotaExceeded, message)
    }
    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotImplemented, message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransactionType {
    /// Invoices (receiving).
    Incoming,
    /// Payments (sending).
    Outgoing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransactionState {
    Pending,
    Settled,
    /// Hold invoices.
    Accepted,
    /// Invoices.
    Expired,
    /// Payments.
    Failed,
}

/// The NIP-47 transaction shape (`make_invoice`, `lookup_invoice`,
/// `list_transactions`, notifications). Amounts are millisats.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transaction {
    #[serde(rename = "type")]
    pub kind: TransactionType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<TransactionState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preimage: Option<String>,
    pub payment_hash: String,
    #[serde(rename = "amount")]
    pub amount_msat: u64,
    #[serde(rename = "fees_paid", default)]
    pub fees_paid_msat: u64,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// What the wallet says about itself (`get_info`). The service adds the
/// per-connection `methods` and `notifications`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WalletInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Hex colour, e.g. `#f7931a`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Node / wallet public key, if there is one worth sharing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pubkey: Option<String>,
    /// `mainnet`, `testnet`, `signet` or `regtest`.
    pub network: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_hash: Option<String>,
}

/// `pay_invoice` as handed to the wallet, after decoding and budget checks.
#[derive(Debug, Clone, PartialEq)]
pub struct PayInvoiceRequest {
    pub invoice: String,
    /// Set only for amountless invoices (from the request's `amount`).
    pub amount_msat: Option<u64>,
    /// What will be charged against the budget (invoice amount or `amount`).
    pub total_amount_msat: u64,
    pub payment_hash: String,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayInvoiceResponse {
    pub preimage: String,
    pub fees_paid_msat: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MakeInvoiceRequest {
    #[serde(rename = "amount")]
    pub amount_msat: u64,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub description_hash: Option<String>,
    /// Seconds from creation.
    #[serde(default)]
    pub expiry: Option<u64>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct LookupInvoiceRequest {
    #[serde(default)]
    pub payment_hash: Option<String>,
    #[serde(default)]
    pub invoice: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct ListTransactionsRequest {
    /// Inclusive, seconds. Default 0.
    #[serde(default)]
    pub from: Option<u64>,
    /// Inclusive, seconds. Default now.
    #[serde(default)]
    pub until: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub offset: Option<u64>,
    /// Include unpaid invoices.
    #[serde(default)]
    pub unpaid: bool,
    /// `None`: both.
    #[serde(default, rename = "type")]
    pub kind: Option<TransactionType>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TransactionList {
    pub transactions: Vec<Transaction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_count: Option<u64>,
}

/// The decrypted request body.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RequestBody {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// The response body (before encryption).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseBody {
    pub result_type: String,
    pub error: Option<NwcError>,
    pub result: Option<Value>,
}

/// Encryption schemes (the `encryption` tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encryption {
    Nip44V2,
    Nip04,
}

impl Encryption {
    pub fn as_str(self) -> &'static str {
        match self {
            Encryption::Nip44V2 => "nip44_v2",
            Encryption::Nip04 => "nip04",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "nip44_v2" => Some(Encryption::Nip44V2),
            "nip04" => Some(Encryption::Nip04),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationType {
    PaymentReceived,
    PaymentSent,
}

impl NotificationType {
    pub fn as_str(self) -> &'static str {
        match self {
            NotificationType::PaymentReceived => "payment_received",
            NotificationType::PaymentSent => "payment_sent",
        }
    }
}

use serde::{Deserialize, Serialize};

/// Amount in satoshis. Everything inside the core is integer sats.
pub type Sats = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Bitcoin,
    Testnet,
    Signet,
    Regtest,
}

impl Network {
    pub fn to_bitcoin(self) -> bitcoin::Network {
        match self {
            Network::Bitcoin => bitcoin::Network::Bitcoin,
            Network::Testnet => bitcoin::Network::Testnet,
            Network::Signet => bitcoin::Network::Signet,
            Network::Regtest => bitcoin::Network::Regtest,
        }
    }
}

/// Which technology an account runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RailKind {
    /// Plain bitcoin on the base chain (BDK).
    Onchain,
    /// Spark L2 with Lightning interop (Breez SDK – Spark).
    Spark,
    /// Cashu ecash held against a mint (CDK).
    Ecash,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(transparent)]
pub struct AccountId(pub String);

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for AccountId {
    fn from(s: &str) -> Self {
        AccountId(s.to_string())
    }
}

/// How an account holds its keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Custody {
    /// Keys derived from the app's recovery phrase and held on this device.
    Hot,
    /// Keys live on a hardware signer; we only hold the xpub/descriptor.
    Hardware { device: String, fingerprint: String },
    /// M-of-N multisig. `local_signers` is how many keys this device holds.
    Multisig { threshold: u8, total: u8, local_signers: u8 },
    /// Descriptor or xpub only, no signing at all.
    WatchOnly,
    /// Funds held by a third party under bearer tokens (ecash).
    Mint { url: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Balance {
    /// Spendable now.
    pub confirmed: Sats,
    /// Incoming, not yet spendable.
    pub pending_in: Sats,
    /// Outgoing, not yet final.
    pub pending_out: Sats,
}

impl Balance {
    pub fn total(&self) -> Sats {
        self.confirmed + self.pending_in
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    pub id: AccountId,
    pub name: String,
    pub rail: RailKind,
    pub custody: Custody,
    pub balance: Balance,
    /// Methods this account can be paid with.
    pub receives: Vec<Method>,
    /// Methods this account can pay out to.
    pub pays: Vec<Method>,
}

/// A way of moving sats. The planner matches these up between accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum Method {
    BitcoinAddress,
    Bolt11,
    Bolt12,
    LightningAddress,
    Lnurl,
    SparkAddress,
    LiquidAddress,
    CashuToken,
}

/// A decoded thing the user pasted or scanned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PaymentRequest {
    BitcoinAddress { address: String, amount_sat: Option<Sats>, label: Option<String> },
    Bolt11 { invoice: String },
    Bolt12 { offer: String },
    LightningAddress { address: String },
    Lnurl { lnurl: String },
    SparkAddress { address: String },
    LiquidAddress { address: String, amount_sat: Option<Sats> },
    CashuToken { token: String },
    /// BIP21 with several options; the planner picks the best one.
    Unified { options: Vec<PaymentRequest> },
}

impl PaymentRequest {
    pub fn method(&self) -> Option<Method> {
        Some(match self {
            PaymentRequest::BitcoinAddress { .. } => Method::BitcoinAddress,
            PaymentRequest::Bolt11 { .. } => Method::Bolt11,
            PaymentRequest::Bolt12 { .. } => Method::Bolt12,
            PaymentRequest::LightningAddress { .. } => Method::LightningAddress,
            PaymentRequest::Lnurl { .. } => Method::Lnurl,
            PaymentRequest::SparkAddress { .. } => Method::SparkAddress,
            PaymentRequest::LiquidAddress { .. } => Method::LiquidAddress,
            PaymentRequest::CashuToken { .. } => Method::CashuToken,
            PaymentRequest::Unified { .. } => return None,
        })
    }

    /// Every concrete option this request offers, most preferred first.
    pub fn options(&self) -> Vec<PaymentRequest> {
        match self {
            PaymentRequest::Unified { options } => options.clone(),
            other => vec![other.clone()],
        }
    }

    pub fn amount_sat(&self) -> Option<Sats> {
        match self {
            PaymentRequest::BitcoinAddress { amount_sat, .. } => *amount_sat,
            PaymentRequest::LiquidAddress { amount_sat, .. } => *amount_sat,
            PaymentRequest::Bolt11 { invoice } => crate::parse::bolt11_amount_sat(invoice),
            PaymentRequest::Unified { options } => options.iter().find_map(|o| o.amount_sat()),
            _ => None,
        }
    }
}

/// What an account hands out so someone can pay it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiveTarget {
    pub method: Method,
    /// The string to show as text / QR.
    pub request: String,
    pub amount_sat: Option<Sats>,
    /// Fee the receiver will pay to claim it, if known.
    pub fee_sat: Option<Sats>,
    /// Unix seconds.
    pub expires_at: Option<u64>,
    /// Rail-specific id used to watch for settlement.
    pub watch_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiveRequest {
    pub method: Method,
    pub amount_sat: Option<Sats>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FeePriority {
    Fast,
    #[default]
    Normal,
    Slow,
    /// Explicit sat/vB for on-chain.
    #[serde(untagged)]
    Custom(u64),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendRequest {
    pub destination: PaymentRequest,
    /// `None` means "use the amount in the request" or "send everything" when `send_max`.
    pub amount_sat: Option<Sats>,
    #[serde(default)]
    pub send_max: bool,
    #[serde(default)]
    pub fee: FeePriority,
    pub note: Option<String>,
}

/// A priced, ready-to-confirm payment. Opaque `handle` is rail-specific.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendQuote {
    pub account: AccountId,
    pub method: Method,
    pub amount_sat: Sats,
    pub fee_sat: Sats,
    pub handle: String,
    /// For hardware / multisig accounts: a PSBT that needs external signatures.
    pub needs_signature: Option<UnsignedPsbt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnsignedPsbt {
    pub psbt_base64: String,
    /// Signatures still missing (1 for a hardware single-sig).
    pub signatures_needed: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Incoming,
    Outgoing,
    /// Internal move between two of the user's own accounts.
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PaymentStatus {
    Pending,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Payment {
    pub id: String,
    pub account: AccountId,
    pub direction: Direction,
    pub amount_sat: Sats,
    pub fee_sat: Sats,
    pub status: PaymentStatus,
    pub method: Method,
    /// Unix seconds.
    pub timestamp: u64,
    pub description: Option<String>,
    /// Bearer token produced by this payment (e.g. a Cashu token to hand over).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

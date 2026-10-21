use async_trait::async_trait;

use crate::types::*;
use crate::Result;

/// One account backed by one technology. Implemented in the `rail-*` crates.
#[async_trait]
pub trait Rail: Send + Sync {
    fn id(&self) -> &AccountId;
    fn kind(&self) -> RailKind;

    /// Current snapshot, including the methods this account can use.
    async fn info(&self) -> Result<AccountInfo>;

    /// Pull fresh state from the network.
    async fn sync(&self) -> Result<()>;

    /// Produce something a payer can pay (address, invoice…).
    async fn receive(&self, req: ReceiveRequest) -> Result<ReceiveTarget>;

    /// Price a payment without moving funds.
    async fn prepare_send(&self, req: SendRequest) -> Result<SendQuote>;

    /// Execute a quote. For hardware / multisig accounts, pass the signed PSBT.
    async fn send(&self, quote: SendQuote, signed_psbt: Option<String>) -> Result<Payment>;

    /// Accept something that is redeemed rather than paid, e.g. a Cashu token.
    async fn redeem(&self, _request: PaymentRequest) -> Result<Payment> {
        Err(crate::Error::Unsupported("redeem tokens".into()))
    }

    /// Mint a bearer token for `amount_sat` (e.g. a Cashu token to give away).
    async fn create_token(&self, _amount_sat: Sats, _memo: Option<String>) -> Result<Payment> {
        Err(crate::Error::Unsupported("create tokens".into()))
    }

    async fn payments(&self, limit: usize) -> Result<Vec<Payment>>;
}

/// A conversion service that turns one method into another, e.g. Boltz turning
/// "pay this Liquid address" into "pay this Lightning invoice".
///
/// Swap providers don't hold a balance, so they aren't accounts.
#[async_trait]
pub trait SwapProvider: Send + Sync {
    fn name(&self) -> &str;

    /// `(target method, method the payer must use instead)` pairs this provider
    /// can bridge when *sending to* the target.
    fn pay_bridges(&self) -> Vec<(Method, Method)>;

    /// `(method the outside payer will use, method our account must receive with)`
    /// pairs this provider can bridge when *receiving*.
    fn receive_bridges(&self) -> Vec<(Method, Method)>;

    /// Turn an outgoing request into one our account can pay. The provider is
    /// responsible for completing the far side (claiming, broadcasting).
    async fn bridge_payment(
        &self,
        target: PaymentRequest,
        amount_sat: Sats,
        payer_method: Method,
        refund_address: Option<String>,
    ) -> Result<BridgedPayment>;

    /// Wrap one of our receive targets so an outside payer can use `outside`.
    async fn bridge_receive(&self, ours: ReceiveTarget, outside: Method) -> Result<ReceiveTarget>;

    /// Drive pending swaps forward (claims, refunds). Call periodically.
    async fn tick(&self) -> Result<Vec<SwapStatus>>;
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgedPayment {
    pub swap_id: String,
    /// What our account should actually pay.
    pub pay: PaymentRequest,
    /// Amount our account must send (target amount + swap fees).
    pub amount_sat: Sats,
    pub swap_fee_sat: Sats,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapStatus {
    pub swap_id: String,
    pub state: String,
    pub finished: bool,
}

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

    /// Change the display name in the running account. The caller persists it.
    fn set_name(&self, _name: &str) -> Result<()> {
        Err(crate::Error::Unsupported("rename this account".into()))
    }

    /// Pull fresh state from the network.
    async fn sync(&self) -> Result<()>;

    /// Quick check for a payment to one receive target (its `watch_id`),
    /// much cheaper than a full sync. `true` when something new turned up
    /// (now in the account's balance and activity). Default: nothing to check.
    async fn check_incoming(&self, _watch_id: &str) -> Result<bool> {
        Ok(false)
    }

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

    /// Chain details and speed-up / cancel options for one transaction.
    /// On-chain accounts only.
    async fn tx_details(&self, _txid: &str) -> Result<TxDetails> {
        Err(crate::Error::Unsupported("show transaction details".into()))
    }

    /// Price speeding up or cancelling an unconfirmed transaction. The quote
    /// is sent with [`Rail::send`] like any other. On-chain accounts only.
    async fn prepare_bump(
        &self,
        _txid: &str,
        _action: BumpAction,
        _fee: FeePriority,
    ) -> Result<SendQuote> {
        Err(crate::Error::Unsupported(
            "speed up or cancel transactions".into(),
        ))
    }

    /// Unspent coins, newest first. On-chain accounts only.
    async fn list_utxos(&self) -> Result<Vec<Utxo>> {
        Err(crate::Error::Unsupported("list coins".into()))
    }

    /// Set (or, with `None` / empty, remove) a label. On-chain accounts only.
    async fn set_label(
        &self,
        _kind: LabelKind,
        _reference: &str,
        _label: Option<String>,
    ) -> Result<()> {
        Err(crate::Error::Unsupported("label coins".into()))
    }

    /// Freeze or unfreeze a coin (`"txid:vout"`). Frozen coins are never
    /// picked automatically. On-chain accounts only.
    async fn set_frozen(&self, _outpoint: &str, _frozen: bool) -> Result<()> {
        Err(crate::Error::Unsupported("freeze coins".into()))
    }
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
    ///
    /// `ours.amount_sat` carries the amount the user entered. When `ours` is
    /// an on-chain address that is the amount the *payer* pays (fees come
    /// out of it); when `ours` is an invoice it is the invoice amount.
    async fn bridge_receive(&self, ours: ReceiveTarget, outside: Method) -> Result<ReceiveTarget>;

    /// Limits and fees for receiving `outside` into an account that takes
    /// `ours`, using live provider data. `amount_sat` has the same meaning as
    /// `ours.amount_sat` in [`SwapProvider::bridge_receive`]. `None` when this
    /// provider can't quote that bridge.
    async fn receive_quote(
        &self,
        _outside: Method,
        _ours: Method,
        _amount_sat: Option<Sats>,
    ) -> Result<Option<ReceiveQuote>> {
        Ok(None)
    }

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

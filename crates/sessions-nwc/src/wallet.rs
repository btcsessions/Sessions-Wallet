//! What the app implements: the Lightning account behind the service.

use async_trait::async_trait;

use crate::protocol::{
    ListTransactionsRequest, LookupInvoiceRequest, MakeInvoiceRequest, NwcError, PayInvoiceRequest,
    PayInvoiceResponse, Transaction, TransactionList, WalletInfo,
};

/// The Lightning wallet the service drives. Amounts are millisats.
///
/// Return an [`NwcError`] with the right code: `INSUFFICIENT_BALANCE`,
/// `PAYMENT_FAILED`, `NOT_FOUND`, `INTERNAL`, `OTHER`… Messages go to the
/// client as they are, so keep them user-facing and free of secrets.
///
/// The service has already checked permissions and budgets and decoded the
/// invoice before calling `pay_invoice`.
#[async_trait]
pub trait NwcWallet: Send + Sync + 'static {
    async fn get_info(&self) -> Result<WalletInfo, NwcError>;

    /// Spendable balance.
    async fn balance_msat(&self) -> Result<u64, NwcError>;

    /// Pay a BOLT11 invoice and return the preimage once settled.
    async fn pay_invoice(&self, req: PayInvoiceRequest) -> Result<PayInvoiceResponse, NwcError>;

    /// A new BOLT11 invoice (`type: incoming`, `state: pending`).
    async fn make_invoice(&self, req: MakeInvoiceRequest) -> Result<Transaction, NwcError>;

    /// An invoice or payment by payment hash (always set: the service fills it
    /// from the invoice when the client sent only that). `Ok(None)`: not found.
    async fn lookup_invoice(
        &self,
        req: LookupInvoiceRequest,
    ) -> Result<Option<Transaction>, NwcError>;

    /// Newest first. `from` / `until` are inclusive; `unpaid: false` means
    /// settled payments and paid invoices only.
    async fn list_transactions(
        &self,
        req: ListTransactionsRequest,
    ) -> Result<TransactionList, NwcError>;
}

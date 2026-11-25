//! Getting ecash unstuck.
//!
//! Linked devices share one recovery phrase, so they derive the same NUT-13
//! secrets. Each device counts its own way through them, so one device can
//! send blinded messages the mint already signed for another. The mint refuses
//! the request (some answer 20006 "already paid" instead of 10002 "already
//! signed"), CDK files the attempt as ambiguous, and its recovery then fails
//! forever because the restored signatures belong to someone else's request.
//! The quote stays paid but never issued, and CDK's sync only logs a warning.
//!
//! Here: ask the mint whether anything was issued. If nothing was, the
//! attempt definitely failed, so release the quote; catch the counters up with
//! the mint (a NUT-13 restore scan) and mint again. Errors are returned
//! instead of logged, so the app can show them.

use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::PaymentMethod;
use cdk::wallet::types::{IssueSagaState, WalletSagaState};
use cdk::{Amount, MintQuoteResponse};
use sessions_core::{Error, Result, Sats};

use crate::{rail_err, EcashAccount};

/// How many NUT-20 quote keys to try when claiming a quote by ID: more than
/// the quotes one wallet will ever create.
const QUOTE_KEY_SEARCH: u32 = 20_000;

fn bolt11() -> PaymentMethod {
    PaymentMethod::Known(KnownMethod::Bolt11)
}

/// The mint refused because these outputs (or this quote) were already used:
/// a counter collision with another device on the same phrase.
fn is_collision(e: &cdk::Error) -> bool {
    matches!(
        e,
        cdk::Error::RequestAlreadyPaid | cdk::Error::BlindedMessageAlreadySigned
    )
}

impl EcashAccount {
    /// What the mint says it has issued for `quote_id`.
    async fn issued_at_mint(&self, quote_id: &str) -> Result<(Amount, Amount)> {
        let r = self
            .client
            .get_mint_quote_status(bolt11(), quote_id)
            .await
            .map_err(rail_err)?;
        match r {
            MintQuoteResponse::Bolt11(r) => Ok((r.amount_paid, r.amount_issued)),
            _ => Err(Error::Invalid("not a Lightning quote".into())),
        }
    }

    /// Release mint attempts stuck "ambiguous" where the mint confirms it
    /// issued nothing. Returns how many were released.
    pub(crate) async fn release_failed_mints(&self) -> Result<u32> {
        let mut released = 0;
        let sagas = self
            .wallet
            .localstore
            .get_incomplete_sagas()
            .await
            .map_err(rail_err)?;
        for saga in sagas {
            if saga.mint_url != self.wallet.mint_url {
                continue;
            }
            let WalletSagaState::Issue(IssueSagaState::MintRequested) = saga.state else {
                continue;
            };
            let Some(quote_id) = saga.quote_id.clone() else {
                continue;
            };
            match self.issued_at_mint(&quote_id).await {
                Ok((_, issued)) if issued == Amount::ZERO => {
                    let db = &self.wallet.localstore;
                    if let Err(e) = db.release_mint_quote(&saga.id).await {
                        tracing::warn!(account = %self.id, "ecash: release quote: {e}");
                    }
                    db.delete_saga(&saga.id).await.map_err(rail_err)?;
                    tracing::info!(account = %self.id, saga = %saga.id, "ecash: released a mint attempt the mint never issued");
                    released += 1;
                }
                Ok(_) => {} // Something was issued: CDK's own recovery handles it.
                Err(e) => tracing::warn!(account = %self.id, "ecash: check stuck mint: {e}"),
            }
        }
        Ok(released)
    }

    /// Catch this device's counters up with the mint, so it doesn't reuse
    /// secrets another device on the same phrase already used. Also picks up
    /// any of this phrase's ecash the database doesn't know about.
    pub(crate) async fn catch_up_counters(&self) -> Result<()> {
        let restored = self.wallet.restore().await.map_err(rail_err)?;
        tracing::info!(account = %self.id, ?restored, "ecash: counters caught up with the mint");
        Ok(())
    }

    /// Mint one paid quote. On a collision: release the failed attempt, catch
    /// up, and try once more.
    async fn mint_quote(&self, quote_id: &str) -> Result<()> {
        match self.wallet.mint(quote_id, Default::default(), None).await {
            Ok(_) => Ok(()),
            Err(e) if is_collision(&e) => {
                tracing::warn!(account = %self.id, "ecash: mint collided ({e}); catching up and retrying");
                self.release_failed_mints().await?;
                self.catch_up_counters().await?;
                self.wallet
                    .mint(quote_id, Default::default(), None)
                    .await
                    .map(|_| ())
                    .map_err(rail_err)
            }
            Err(e) => Err(rail_err(e)),
        }
    }

    /// Mint every quote the mint reports paid and not yet issued. Unlike
    /// CDK's `mint_unissued_quotes`, failures are returned.
    pub(crate) async fn mint_paid_quotes(&self) -> Result<Sats> {
        let quotes = self.wallet.get_unissued_mint_quotes().await.map_err(rail_err)?;
        let mut minted = 0u64;
        let mut first_error = None;
        for q in quotes {
            // Only Lightning quotes this wallet can sign for.
            if q.payment_method != bolt11() || q.secret_key.is_none() {
                continue;
            }
            let (paid, issued) = match self.issued_at_mint(&q.id).await {
                Ok(x) => x,
                Err(e) => {
                    first_error.get_or_insert(e);
                    continue;
                }
            };
            let mintable = paid.to_u64().saturating_sub(issued.to_u64());
            if mintable == 0 {
                continue;
            }
            match self.mint_quote(&q.id).await {
                Ok(()) => minted += mintable,
                Err(e) => {
                    tracing::warn!(account = %self.id, quote = %q.id, "ecash: couldn't mint a paid quote: {e}");
                    first_error.get_or_insert(e);
                }
            }
        }
        match first_error {
            Some(e) if minted == 0 => Err(Error::Invalid(format!(
                "Couldn't collect ecash you've paid for: {e}"
            ))),
            _ => Ok(minted),
        }
    }

    /// Collect a paid quote by its ID, for when this device has lost track of
    /// it (another device made it, or its database is gone). The quote's
    /// NUT-20 key is rebuilt from the recovery phrase. Returns sats collected.
    pub async fn claim_quote(&self, quote_id: &str) -> Result<Sats> {
        let quote_id = quote_id.trim();
        if quote_id.is_empty() {
            return Err(Error::Invalid("Paste the quote ID".into()));
        }
        let (paid, issued) = self.issued_at_mint(quote_id).await.map_err(|e| {
            tracing::warn!(account = %self.id, "ecash: claim lookup: {e}");
            Error::Invalid("This mint doesn't know that quote ID".into())
        })?;
        if paid == Amount::ZERO {
            return Err(Error::Invalid("That quote hasn't been paid".into()));
        }
        let mintable = paid.to_u64().saturating_sub(issued.to_u64());
        if mintable == 0 {
            return Err(Error::Invalid("That quote has already been collected".into()));
        }
        let q = self
            .wallet
            .fetch_mint_quote_with_key_search(quote_id, Some(bolt11()), QUOTE_KEY_SEARCH)
            .await
            .map_err(rail_err)?;
        if q.secret_key.is_none() {
            // Not ours: don't leave it listed as a pending receive.
            let _ = self.wallet.localstore.remove_mint_quote(quote_id).await;
            return Err(Error::Invalid(
                "That quote wasn't made by this wallet's recovery phrase".into(),
            ));
        }
        self.release_failed_mints().await?;
        self.catch_up_counters().await?;
        self.mint_quote(quote_id).await?;
        Ok(mintable)
    }

    /// Drop ecash the mint says is spent: another device on the same phrase
    /// spent it (they hold the same ecash once both have caught up). Checked
    /// at most every [`SPENT_CHECK_EVERY`]. Returns the sats dropped.
    pub(crate) async fn drop_spent_elsewhere(&self) -> Result<u64> {
        {
            let mut last = self.last_spent_check.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|t| t.elapsed() < SPENT_CHECK_EVERY) {
                return Ok(0);
            }
            *last = Some(std::time::Instant::now());
        }
        let proofs = self.wallet.get_unspent_proofs().await.map_err(rail_err)?;
        let mut dropped = 0u64;
        for chunk in proofs.chunks(100) {
            let states = self
                .wallet
                .check_proofs_spent(chunk.to_vec())
                .await
                .map_err(rail_err)?;
            for (p, st) in chunk.iter().zip(&states) {
                if st.state == cdk::nuts::State::Spent {
                    dropped += p.amount.to_u64();
                }
            }
        }
        if dropped > 0 {
            tracing::info!(account = %self.id, sats = dropped, "ecash: dropped ecash spent on another device");
        }
        Ok(dropped)
    }
}

/// How often to ask the mint whether this device's ecash was spent elsewhere.
pub(crate) const SPENT_CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(120);

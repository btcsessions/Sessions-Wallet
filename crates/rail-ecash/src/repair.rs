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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use cdk::dhke::construct_proofs;
use cdk::nuts::{
    BlindSignature, CheckStateRequest, CurrencyUnit, KeySet, PreMint, PreMintSecrets, PublicKey,
    RestoreRequest, State,
};
use cdk::wallet::types::ProofInfo;
use serde::{Deserialize, Serialize};
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

    /// Catch up with the mint: find ecash made from this phrase that this
    /// device doesn't hold (restored from the phrase, or received on another
    /// linked device) and move this device's counters past every output the
    /// mint has signed, so it never reuses one. Returns the sats found.
    ///
    /// CDK's `restore` is for a brand-new database: it *adds* the highest
    /// counter found to the current one, so running it on a wallet in use
    /// jumps the counter far ahead, leaving gaps other devices' scans stop
    /// at. This scan never jumps (counter = highest signed + 1 at most), keeps
    /// how far it got per keyset, and with `deep` looks past long gaps
    /// (which older versions created).
    pub(crate) async fn catch_up(&self, deep: bool) -> Result<u64> {
        let _one_at_a_time = self.scan_lock.lock().await;
        let mut scan = load_scan(&self.scan_path);
        let keysets = self.wallet.keysets(Default::default()).await.map_err(rail_err)?;
        let db = &self.wallet.localstore;
        let known: HashSet<PublicKey> = db
            .get_proofs(Some(self.wallet.mint_url.clone()), None, None, None)
            .await
            .map_err(rail_err)?
            .into_iter()
            .map(|p| p.y)
            .collect();
        let mut found = 0u64;
        for ks in keysets.iter().filter(|k| k.unit == CurrencyUnit::Sat) {
            let key = ks.id.to_string();
            let mut entry = scan.get(&key).copied().unwrap_or_default();
            let inactive = ks.active == Some(false);
            if inactive && entry.done {
                continue; // an inactive keyset signs nothing new
            }
            let gap = if deep || entry.next == 0 { DEEP_GAP } else { LIGHT_GAP };
            // Another device got this far: scan at least that far.
            let peer = self
                .peer_counters
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
                .copied()
                .unwrap_or(0);
            // Resume a long stretch toward another device's counter where an
            // earlier (time-limited) scan left off.
            let mut start = if entry.scanned > entry.next && entry.scanned <= peer {
                entry.scanned
            } else {
                entry.next
            };
            let mut empty = 0;
            while empty < gap || start < peer {
                let end = start.saturating_add(BATCH);
                let pre = PreMintSecrets::restore_batch(ks.id, &self.scan_seed, start, end)
                    .map_err(rail_err)?;
                tokio::time::sleep(SCAN_PACE).await;
                let outputs = pre.blinded_messages();
                let resp = retry(|| {
                    self.client.post_restore(RestoreRequest {
                        outputs: outputs.clone(),
                    })
                })
                .await?;
                let sigs: HashMap<PublicKey, BlindSignature> = resp
                    .outputs
                    .iter()
                    .zip(resp.signatures)
                    .map(|(o, s)| (o.blinded_secret, s))
                    .collect();
                let matched: Vec<(u32, &PreMint, BlindSignature)> = pre
                    .secrets
                    .iter()
                    .enumerate()
                    .filter_map(|(i, p)| {
                        sigs.get(&p.blinded_message.blinded_secret)
                            .map(|s| (start + i as u32, p, s.clone()))
                    })
                    .collect();
                if matched.is_empty() {
                    empty += 1;
                    start = end;
                    if start <= peer {
                        // Nobody mints below another device's counter (each
                        // moves its own past it), so this stretch stays empty.
                        entry.scanned = start;
                        scan.insert(key.clone(), entry);
                        save_scan(&self.scan_path, &scan);
                    }
                    continue;
                }
                empty = 0;
                found += self.store_found(ks, &matched, &known).await?;
                if let Some(h) = matched.last().map(|m| m.0) {
                    entry.next = entry.next.max(h + 1);
                }
                entry.scanned = entry.scanned.max(end);
                scan.insert(key.clone(), entry);
                save_scan(&self.scan_path, &scan);
                start = end;
            }
            if inactive {
                entry.done = true;
            }
            scan.insert(key.clone(), entry);
            save_scan(&self.scan_path, &scan);
            // Past everything signed and every other device's counter, so
            // this device never reuses an output or mints inside a gap.
            let target = entry.next.max(peer);
            let current = db.increment_keyset_counter(&ks.id, 0).await.map_err(rail_err)?;
            if target > current {
                db.increment_keyset_counter(&ks.id, target - current)
                    .await
                    .map_err(rail_err)?;
            }
        }
        tracing::info!(account = %self.id, sats = found, deep, "ecash: caught up with the mint");
        Ok(found)
    }

    /// Counters other linked devices have reached (keyset id → counter).
    pub fn set_peer_counters(&self, counters: &BTreeMap<String, u32>) {
        *self.peer_counters.lock().unwrap_or_else(|e| e.into_inner()) = counters.clone();
    }

    /// This device's counters (keyset id → next counter), to share with the
    /// other linked devices.
    pub async fn counters(&self) -> Result<BTreeMap<String, u32>> {
        let keysets = self.wallet.keysets(Default::default()).await.map_err(rail_err)?;
        let mut out = BTreeMap::new();
        for ks in keysets.iter().filter(|k| k.unit == CurrencyUnit::Sat) {
            let c = self
                .wallet
                .localstore
                .increment_keyset_counter(&ks.id, 0)
                .await
                .map_err(rail_err)?;
            if c > 0 {
                out.insert(ks.id.to_string(), c);
            }
        }
        Ok(out)
    }

    /// "Check the mint": a deep catch-up the person asked for, with no time
    /// limit. Returns the sats found.
    pub async fn check_mint(&self) -> Result<Sats> {
        self.catch_up(true).await
    }

    /// Keep the found outputs this database doesn't already hold and the mint
    /// says aren't spent. Returns their sats.
    async fn store_found(
        &self,
        ks: &KeySet,
        matched: &[(u32, &PreMint, BlindSignature)],
        known: &HashSet<PublicKey>,
    ) -> Result<u64> {
        let proofs = construct_proofs(
            matched.iter().map(|m| m.2.clone()).collect(),
            matched.iter().map(|m| m.1.r.clone()).collect(),
            matched.iter().map(|m| m.1.secret.clone()).collect(),
            &ks.keys,
        )
        .map_err(rail_err)?;
        let mut new = Vec::new();
        for (p, m) in proofs.into_iter().zip(matched) {
            let y = p.y().map_err(rail_err)?;
            if !known.contains(&y) {
                new.push((y, p, m.0));
            }
        }
        if new.is_empty() {
            return Ok(0);
        }
        let ys: Vec<PublicKey> = new.iter().map(|n| n.0).collect();
        let states = retry(|| self.client.post_check_state(CheckStateRequest { ys: ys.clone() }))
            .await?
            .states;
        let mut infos = Vec::new();
        let mut sats = 0;
        for ((_, p, counter), st) in new.into_iter().zip(states) {
            if st.state == State::Spent {
                continue;
            }
            if st.state == State::Unspent {
                sats += p.amount.to_u64();
            }
            infos.push(
                ProofInfo::new(p, self.wallet.mint_url.clone(), st.state, ks.unit.clone())
                    .map_err(rail_err)?
                    .with_derivation_index(counter),
            );
        }
        self.wallet
            .localstore
            .update_proofs(infos, vec![])
            .await
            .map_err(rail_err)?;
        Ok(sats)
    }

    /// Mint one paid quote. On a collision: release the failed attempt, catch
    /// up, and try once more.
    async fn mint_quote(&self, quote_id: &str) -> Result<()> {
        match self.wallet.mint(quote_id, Default::default(), None).await {
            Ok(_) => Ok(()),
            Err(e) if is_collision(&e) => {
                tracing::warn!(account = %self.id, "ecash: mint collided ({e}); catching up and retrying");
                self.release_failed_mints().await?;
                self.catch_up(true).await?;
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
        self.catch_up(true).await?;
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

/// Outputs per restore request.
const BATCH: u32 = 100;
/// Empty batches before a routine scan stops.
const LIGHT_GAP: u32 = 3;
/// Empty batches before a deep scan stops: a little past small gaps. Long
/// gaps (old versions jumped counters) are crossed with the other devices'
/// shared counters instead, which costs nothing when there's no gap.
const DEEP_GAP: u32 = 10;
/// Pause between scan requests, so a scan never floods the mint.
const SCAN_PACE: std::time::Duration = std::time::Duration::from_millis(150);

/// How far a keyset has been scanned.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
struct KeysetScan {
    /// One past the highest counter the mint has signed (that we've seen).
    next: u32,
    /// An inactive keyset, scanned to the end.
    done: bool,
    /// How far a stretch toward another device's counter has been scanned.
    #[serde(default)]
    scanned: u32,
}

fn load_scan(path: &Path) -> BTreeMap<String, KeysetScan> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_scan(path: &Path, scan: &BTreeMap<String, KeysetScan>) {
    if let Ok(bytes) = serde_json::to_vec(scan) {
        let tmp = path.with_extension("scan.json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// A scan makes many requests; one dropped by a flaky connection (a phone
/// switching networks) shouldn't end it. Up to 4 tries, waiting 1, 3, 6 s.
async fn retry<T, F, Fut>(mut call: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, cdk::Error>>,
{
    let mut tries = 0;
    loop {
        match call().await {
            Ok(v) => return Ok(v),
            Err(e) if tries < 3 && is_transient(&e) => {
                tries += 1;
                tracing::info!("ecash: mint request failed ({e}); retrying");
                tokio::time::sleep(std::time::Duration::from_secs([1, 3, 6][tries - 1])).await;
            }
            Err(e) => return Err(rail_err(e)),
        }
    }
}

/// Connection trouble, not a refusal from the mint.
fn is_transient(e: &cdk::Error) -> bool {
    let m = e.to_string().to_ascii_lowercase();
    ["transport", "unreachable", "timed out", "timeout", "connection", "dns", "reset", "temporarily"]
        .iter()
        .any(|w| m.contains(w))
}

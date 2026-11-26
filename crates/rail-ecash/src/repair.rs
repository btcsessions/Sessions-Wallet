//! Linked devices, counters, and getting ecash unstuck.
//!
//! Linked devices share one recovery phrase, so they derive the same NUT-13
//! secrets. Each device takes its own range of counters ("slot", picked by
//! the app through the synced backup), so no two devices ever send the same
//! blinded messages. Each publishes how far it has got in its range (through
//! the encrypted backup, not the mint), and the others fetch exactly those
//! outputs to show the same ecash. The mint is only scanned once, when a
//! database is new (a restore).
//!
//! Before ranges, devices did collide: the mint refuses reused outputs (some
//! answer 20006 "already paid" instead of 10002 "already signed"), CDK files
//! the attempt as ambiguous, and its recovery then fails forever. Here: ask
//! the mint whether anything was issued; if nothing was, release the quote,
//! skip the counters ahead and mint again. Errors are returned instead of
//! logged, so the app can show them.

use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::PaymentMethod;
use cdk::wallet::types::{IssueSagaState, WalletSagaState};
use cdk::{Amount, MintQuoteResponse};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::Ordering;

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
            .map_err(rail_err);
        self.note_contact(&r);
        let r = r?;
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

    /// Pick this device's counter range: slot `n` owns counters
    /// `n * SLOT .. (n + 1) * SLOT`. Linked devices each take their own, so
    /// they never derive the same outputs and the mint never has to sort out
    /// who used what. 0 = not picked yet: the shared range older versions use.
    pub fn set_slot(&self, slot: u32) {
        self.slot.store(slot.min(MAX_SLOT), Ordering::SeqCst);
    }

    fn own_slot(&self) -> u32 {
        self.slot.load(Ordering::SeqCst)
    }

    /// The other devices' counters for keyset `ks`, by slot (from the shared
    /// counters: `<keyset>` for the shared range, `<keyset>@<slot>` for one).
    fn peer_slots(&self, ks: &cdk::nuts::Id) -> BTreeMap<u32, u32> {
        let id = ks.to_string();
        let peers = self.peer_counters.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = BTreeMap::new();
        for (k, v) in peers.iter() {
            let slot = if *k == id {
                Some(0)
            } else {
                k.strip_prefix(id.as_str())
                    .and_then(|r| r.strip_prefix('@'))
                    .and_then(|n| n.parse::<u32>().ok())
            };
            if let Some(s) = slot.filter(|s| *s <= MAX_SLOT) {
                out.insert(s, *v);
            }
        }
        out
    }

    async fn sat_keysets(&self) -> Result<Vec<KeySet>> {
        Ok(self
            .wallet
            .keysets(Default::default())
            .await
            .map_err(rail_err)?
            .into_iter()
            .filter(|k| k.unit == CurrencyUnit::Sat)
            .collect())
    }

    async fn known_ys(&self) -> Result<HashSet<PublicKey>> {
        Ok(self
            .wallet
            .localstore
            .get_proofs(Some(self.wallet.mint_url.clone()), None, None, None)
            .await
            .map_err(rail_err)?
            .into_iter()
            .map(|p| p.y)
            .collect())
    }

    /// A new database (restored from the phrase, or a new device): look for
    /// ecash this phrase already owns. The shared range from the start (a
    /// little past gaps), and the start of each device range (one request
    /// says whether anyone used it), going on wherever something turns up.
    /// Runs once. Returns the sats found.
    pub(crate) async fn restore_scan(&self) -> Result<u64> {
        let _one_at_a_time = self.scan_lock.lock().await;
        let mut scan = load_scan(&self.scan_path);
        let known = self.known_ys().await?;
        let own = self.own_slot();
        let mut found = 0u64;
        for ks in self.sat_keysets().await? {
            let inactive = ks.active == Some(false);
            let shared_key = slot_key(&ks.id, 0);
            if inactive && scan.get(&shared_key).is_some_and(|e| e.done) {
                continue; // an inactive keyset signs nothing new
            }
            let peers = self.peer_slots(&ks.id);
            let mut slots: BTreeSet<u32> = (0..=PROBE_SLOTS).collect();
            slots.extend(peers.keys().copied());
            slots.insert(own);
            for slot in slots {
                let key = slot_key(&ks.id, slot);
                let mut entry = scan.get(&key).copied().unwrap_or_default();
                let peer = peers.get(&slot).copied().unwrap_or(0);
                let (probe, gap) = if slot == 0 { (DEEP_GAP, DEEP_GAP) } else { (1, DEEP_GAP) };
                let st = self
                    .scan_stretch(&ks, entry.pos(slot), peer, probe, gap, &known)
                    .await?;
                found += st.sats;
                entry.record(&st, peer);
                scan.insert(key, entry);
                save_scan(&self.scan_path, &scan);
            }
            if inactive {
                scan.entry(shared_key).or_default().done = true;
                save_scan(&self.scan_path, &scan);
            }
        }
        self.catch_up_quote_counter().await?;
        tracing::info!(account = %self.id, sats = found, "ecash: restore scan done");
        Ok(found)
    }

    /// Fetch the ecash the other linked devices have added since last time.
    /// Their shared counters say exactly which outputs they've used, so this
    /// asks the mint for just those, and for nothing when they haven't added
    /// any. Returns the sats found.
    pub(crate) async fn follow_peers(&self) -> Result<u64> {
        self.follow_peers_up_to(FOLLOW_BATCHES).await
    }

    async fn follow_peers_up_to(&self, max_batches: u32) -> Result<u64> {
        let own = self.own_slot();
        let todo = |scan: &BTreeMap<String, KeysetScan>, keysets: &[KeySet]| {
            let mut out = Vec::new();
            for ks in keysets {
                for (slot, peer) in self.peer_slots(&ks.id) {
                    // This device's own range holds nothing new to it (the
                    // shared range is shared until a range is picked).
                    if slot == own && own != 0 {
                        continue;
                    }
                    let pos = scan.get(&slot_key(&ks.id, slot)).copied().unwrap_or_default().pos(slot);
                    if peer > pos {
                        out.push((ks.clone(), slot, pos, peer));
                    }
                }
            }
            out
        };
        let keysets = self.sat_keysets().await?;
        if todo(&load_scan(&self.scan_path), &keysets).is_empty() {
            self.catch_up_quote_counter().await?;
            return Ok(0);
        }
        let _one_at_a_time = self.scan_lock.lock().await;
        let mut scan = load_scan(&self.scan_path);
        let known = self.known_ys().await?;
        let mut budget = max_batches;
        let mut found = 0u64;
        for (ks, slot, pos, peer) in todo(&scan, &keysets) {
            if budget == 0 {
                break; // the rest next time
            }
            let until = peer.min(pos.saturating_add(budget.saturating_mul(BATCH)));
            let st = self.scan_stretch(&ks, pos, until, 0, 0, &known).await?;
            budget = budget.saturating_sub(st.batches);
            found += st.sats;
            let key = slot_key(&ks.id, slot);
            let mut entry = scan.get(&key).copied().unwrap_or_default();
            entry.record(&st, until);
            scan.insert(key, entry);
            save_scan(&self.scan_path, &scan);
        }
        self.catch_up_quote_counter().await?;
        if found > 0 {
            tracing::info!(account = %self.id, sats = found, "ecash: fetched ecash from linked devices");
        }
        Ok(found)
    }

    /// Keep this device's counters in its own range, past anything used
    /// there already (by it, or by a device that had the range before).
    /// Without a range: past everything known in the shared one, as before.
    pub(crate) async fn ensure_slot(&self) -> Result<()> {
        let own = self.own_slot();
        let scan = load_scan(&self.scan_path);
        let db = &self.wallet.localstore;
        for ks in self.sat_keysets().await? {
            if ks.active == Some(false) {
                continue; // signs nothing new
            }
            let entry = scan.get(&slot_key(&ks.id, own)).copied().unwrap_or_default();
            let peer = self.peer_slots(&ks.id).get(&own).copied().unwrap_or(0);
            let target = entry.next.max(peer).max(own.saturating_mul(SLOT));
            let current = db.increment_keyset_counter(&ks.id, 0).await.map_err(rail_err)?;
            if target > current {
                db.increment_keyset_counter(&ks.id, target - current)
                    .await
                    .map_err(rail_err)?;
            }
        }
        Ok(())
    }

    /// Ask the mint for outputs `from..`: up to `until`, then on until
    /// `probe` empty requests in a row (or `gap` once something turned up).
    /// Stores what it finds. Paced, so it never floods the mint.
    async fn scan_stretch(
        &self,
        ks: &KeySet,
        from: u32,
        until: u32,
        probe: u32,
        gap: u32,
        known: &HashSet<PublicKey>,
    ) -> Result<Stretch> {
        let mut st = Stretch { next: from, end: from, ..Default::default() };
        let mut start = from;
        let mut empty = 0;
        let mut seen = false;
        while start < until || empty < if seen { gap } else { probe } {
            let end = start.saturating_add(BATCH);
            if end == start {
                break; // the end of the counters
            }
            let pre = PreMintSecrets::restore_batch(ks.id, &self.scan_seed, start, end)
                .map_err(rail_err)?;
            if st.batches > 0 {
                tokio::time::sleep(SCAN_PACE).await;
            }
            let outputs = pre.blinded_messages();
            let resp = retry(|| {
                self.client.post_restore(RestoreRequest {
                    outputs: outputs.clone(),
                })
            })
            .await;
            self.note_contact(&resp);
            let resp = resp?;
            st.batches += 1;
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
            } else {
                empty = 0;
                seen = true;
                st.sats += self.store_found(ks, &matched, known).await?;
                if let Some(h) = matched.last().map(|m| m.0) {
                    st.next = st.next.max(h + 1);
                }
            }
            start = end;
            st.end = end;
        }
        Ok(st)
    }
    /// Counters other linked devices have reached (keyset id → counter).
    pub fn set_peer_counters(&self, counters: &BTreeMap<String, u32>) {
        *self.peer_counters.lock().unwrap_or_else(|e| e.into_inner()) = counters.clone();
    }

    /// This device's counters, to share with the other linked devices: per
    /// keyset, the next counter under the key of the range it's in
    /// (`<keyset>` for the shared range, `<keyset>@<slot>` for a device's).
    pub async fn counters(&self) -> Result<BTreeMap<String, u32>> {
        let mut out = BTreeMap::new();
        for ks in self.sat_keysets().await? {
            let c = self
                .wallet
                .localstore
                .increment_keyset_counter(&ks.id, 0)
                .await
                .map_err(rail_err)?;
            if c > 0 {
                out.insert(slot_key(&ks.id, c / SLOT), c);
            }
        }
        let q = self
            .wallet
            .localstore
            .increment_derivation_counter(QUOTE_COUNTER, 0)
            .await
            .map_err(rail_err)?;
        if q > 0 {
            out.insert(self.quote_counter_key(), q);
        }
        Ok(out)
    }

    /// Shared-counter key for this account's NUT-20 quote-lock counter.
    fn quote_counter_key(&self) -> String {
        format!("{QUOTE_COUNTER}:{}", self.wallet.mint_url)
    }

    /// Move the NUT-20 quote-lock counter past the other devices', so two
    /// devices don't lock their quotes with the same key (which would let
    /// the mint link them).
    async fn catch_up_quote_counter(&self) -> Result<()> {
        let peer = self
            .peer_counters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&self.quote_counter_key())
            .copied()
            .unwrap_or(0);
        let db = &self.wallet.localstore;
        let current = db
            .increment_derivation_counter(QUOTE_COUNTER, 0)
            .await
            .map_err(rail_err)?;
        if peer > current {
            db.increment_derivation_counter(QUOTE_COUNTER, peer - current)
                .await
                .map_err(rail_err)?;
        }
        Ok(())
    }

    /// "Check the mint", when ecash seems to be missing: fetch everything the
    /// linked devices have added, and look a little past the end of each
    /// range in use. At most every [`CHECK_MINT_EVERY`]. Returns sats found.
    pub async fn check_mint(&self) -> Result<Sats> {
        {
            let mut t = self.throttle.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(at) = t.check_mint.filter(|at| at.elapsed() < CHECK_MINT_EVERY) {
                let wait = (CHECK_MINT_EVERY - at.elapsed()).as_secs() / 60 + 1;
                return Err(Error::Invalid(format!(
                    "The mint was checked a moment ago. To go easy on it, try again in {wait} min"
                )));
            }
            t.check_mint = Some(std::time::Instant::now());
        }
        let mut found = self.follow_peers_up_to(CHECK_MINT_BATCHES).await?;
        {
            let _one_at_a_time = self.scan_lock.lock().await;
            let mut scan = load_scan(&self.scan_path);
            let known = self.known_ys().await?;
            for ks in self.sat_keysets().await? {
                if ks.active == Some(false) {
                    continue;
                }
                let mut slots: BTreeSet<u32> = self.peer_slots(&ks.id).into_keys().collect();
                slots.insert(0);
                slots.insert(self.own_slot());
                for slot in slots {
                    let key = slot_key(&ks.id, slot);
                    let mut entry = scan.get(&key).copied().unwrap_or_default();
                    let st = self
                        .scan_stretch(&ks, entry.pos(slot), 0, CHECK_MINT_GAP, CHECK_MINT_GAP, &known)
                        .await?;
                    found += st.sats;
                    entry.record(&st, 0);
                    scan.insert(key, entry);
                    save_scan(&self.scan_path, &scan);
                }
            }
        }
        self.ensure_slot().await?;
        tracing::info!(account = %self.id, sats = found, "ecash: checked the mint");
        Ok(found)
    }

    /// Whether a step that asks the mint is due (and if so, note it's done).
    pub(crate) fn due(
        &self,
        step: impl FnOnce(&mut Throttle) -> &mut Option<std::time::Instant>,
        every: std::time::Duration,
    ) -> bool {
        let mut t = self.throttle.lock().unwrap_or_else(|e| e.into_inner());
        let last = step(&mut t);
        if last.is_some_and(|at| at.elapsed() < every) {
            return false;
        }
        *last = Some(std::time::Instant::now());
        true
    }

    /// Whether to ask the mint about quote `id` now: when the websocket said
    /// it's paid, the first time, and then every `offline` without the
    /// websocket (every `live` with it, as a safety net).
    pub(crate) fn quote_check_due(
        &self,
        id: &str,
        offline: std::time::Duration,
        live: std::time::Duration,
    ) -> bool {
        let fresh = self.live.take_fresh(id);
        let every = if fresh {
            std::time::Duration::ZERO
        } else if self.live.paid(id) {
            QUOTE_HINT_GAP
        } else if self.live.connected() {
            live
        } else {
            offline
        };
        let mut t = self.throttle.lock().unwrap_or_else(|e| e.into_inner());
        if !fresh && t.quotes.get(id).is_some_and(|at| at.elapsed() < every) {
            return false;
        }
        t.quotes.insert(id.to_string(), std::time::Instant::now());
        true
    }

    /// Whether an unfinished operation refers to quote `id`.
    async fn quote_in_use(&self, id: &str) -> bool {
        self.wallet
            .localstore
            .get_incomplete_sagas()
            .await
            .map(|s| s.iter().any(|s| s.quote_id.as_deref() == Some(id)))
            .unwrap_or(true)
    }

    /// Forget quote `id` here (expired unpaid, or collected elsewhere).
    async fn forget_quote(&self, id: &str) {
        if self.quote_in_use(id).await {
            return;
        }
        if let Err(e) = self.wallet.localstore.remove_mint_quote(id).await {
            tracing::warn!(account = %self.id, "ecash: forget quote: {e}");
            return;
        }
        self.live.forget(id);
        self.throttle.lock().unwrap_or_else(|e| e.into_inner()).quotes.remove(id);
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
        let states = retry(|| self.client.post_check_state(CheckStateRequest { ys: ys.clone() })).await;
        self.note_contact(&states);
        let states = states?.states;
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

    /// Mint one paid quote. On a collision (another device on this phrase
    /// used these outputs: only before the devices have their own ranges, or
    /// with an older version): release the failed attempt, skip this device's
    /// counters ahead and try once more. Nothing is scanned.
    pub(crate) async fn mint_quote(&self, quote_id: &str) -> Result<()> {
        match self.wallet.mint(quote_id, Default::default(), None).await {
            Ok(_) => Ok(()),
            Err(e) if is_collision(&e) => {
                tracing::warn!(account = %self.id, "ecash: mint collided ({e}); skipping ahead and retrying");
                self.release_failed_mints().await?;
                self.skip_ahead().await?;
                self.wallet
                    .mint(quote_id, Default::default(), None)
                    .await
                    .map(|_| ())
                    .map_err(rail_err)
            }
            Err(e) => Err(rail_err(e)),
        }
    }

    /// Move this device's counters well past where they collided.
    pub(crate) async fn skip_ahead(&self) -> Result<()> {
        let jitter = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos() % COLLISION_JITTER);
        for ks in self.sat_keysets().await? {
            if ks.active != Some(false) {
                self.wallet
                    .localstore
                    .increment_keyset_counter(&ks.id, COLLISION_SKIP + jitter)
                    .await
                    .map_err(rail_err)?;
            }
        }
        Ok(())
    }

    /// Mint every quote the mint reports paid and not yet issued. Asks the
    /// mint about a quote when the websocket says it's paid, once a session,
    /// and now and then without the websocket. Expired unpaid quotes and
    /// ones collected on another device are forgotten after a last look, so
    /// they're never asked about again. Unlike CDK's `mint_unissued_quotes`,
    /// failures are returned.
    pub(crate) async fn mint_paid_quotes(&self) -> Result<Sats> {
        let quotes = self.wallet.get_unissued_mint_quotes().await.map_err(rail_err)?;
        let now = now_secs();
        let mut minted = 0u64;
        let mut first_error = None;
        for q in quotes {
            // Only Lightning quotes this wallet can sign for.
            if q.payment_method != bolt11() || q.secret_key.is_none() {
                continue;
            }
            if !self.quote_check_due(&q.id, QUOTE_RECHECK, QUOTE_RECHECK_LIVE) {
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
                let expired = q.expiry > 0 && now > q.expiry + EXPIRY_GRACE_SECS;
                if (paid == Amount::ZERO && expired) || (paid > Amount::ZERO && issued >= paid) {
                    self.forget_quote(&q.id).await;
                }
                continue;
            }
            match self.mint_quote(&q.id).await {
                Ok(()) => {
                    minted += mintable;
                    self.live.forget(&q.id);
                }
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
        self.follow_peers().await?;
        self.ensure_slot().await?;
        self.mint_quote(quote_id).await?;
        Ok(mintable)
    }

    /// Drop ecash the mint says is spent: another device on the same phrase
    /// spent it (they hold the same ecash once both have caught up). The
    /// websocket reports that as it happens; this asks at most every
    /// [`SLOW_CHECK_EVERY`] (every [`LIVE_SPENT_CHECK`] while the websocket
    /// covers all of it). Returns the sats dropped.
    pub(crate) async fn drop_spent_elsewhere(&self) -> Result<u64> {
        {
            let every = if self.live.covering() { LIVE_SPENT_CHECK } else { SLOW_CHECK_EVERY };
            let mut last = self.last_spent_check.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|t| t.elapsed() < every) {
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
                .map_err(rail_err);
            self.note_contact(&states);
            let states = states?;
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

use std::time::Duration;

/// Ask the mint about spent ecash, sent tokens and quotes this often when the
/// websocket isn't there to say.
pub(crate) const SLOW_CHECK_EVERY: Duration = Duration::from_secs(15 * 60);
/// … and about spent ecash this often while it is (a safety net).
const LIVE_SPENT_CHECK: Duration = Duration::from_secs(6 * 3600);
/// Look for mint attempts stuck after a collision this often.
pub(crate) const RELEASE_EVERY: Duration = Duration::from_secs(10 * 60);
/// Check Lightning payments left pending this often.
pub(crate) const MELTS_EVERY: Duration = Duration::from_secs(30);
/// A quote that isn't paid yet: ask again this often without the websocket…
const QUOTE_RECHECK: Duration = Duration::from_secs(60);
/// … and with it (it says when one's paid; this is a safety net).
const QUOTE_RECHECK_LIVE: Duration = Duration::from_secs(30 * 60);
/// The Receive screen and private payments wait on a quote: ask this often
/// without the websocket, and this often with it.
pub(crate) const WAIT_RECHECK: Duration = Duration::from_secs(5);
pub(crate) const WAIT_RECHECK_LIVE: Duration = Duration::from_secs(30);
/// At least this long between asks about a quote the websocket says is paid.
const QUOTE_HINT_GAP: Duration = Duration::from_secs(3);
/// A quote this long past its expiry, still unpaid, is forgotten.
const EXPIRY_GRACE_SECS: u64 = 120;
/// "Check the mint" at most this often.
const CHECK_MINT_EVERY: Duration = Duration::from_secs(30 * 60);
/// Requests "Check the mint" may spend fetching the linked devices' ecash…
const CHECK_MINT_BATCHES: u32 = 50;
/// … and empty requests past the end of each range in use.
const CHECK_MINT_GAP: u32 = 2;
/// Requests a sync may spend fetching the linked devices' ecash (the rest
/// waits for the next sync).
const FOLLOW_BATCHES: u32 = 10;
/// After a collision, skip this many counters ahead (plus up to the jitter,
/// so two devices that collided don't land together again).
const COLLISION_SKIP: u32 = 300;
const COLLISION_JITTER: u32 = 200;

/// CDK's derivation-counter namespace for NUT-20 quote-lock keys.
const QUOTE_COUNTER: &str = "nut20_quote";

/// Counters per device range (slot `n` = `n * SLOT .. (n + 1) * SLOT`): far
/// more than one device will ever use. Slot 0 is the range every device
/// shared before ranges existed (and older versions still use).
pub const SLOT: u32 = 1 << 24;
/// The highest slot (the counters are 32-bit).
pub const MAX_SLOT: u32 = 255;
/// A restore looks at the start of slots 1 to this one even when no linked
/// device has said it uses them (the app hands out the lowest free slot).
const PROBE_SLOTS: u32 = 8;
/// Outputs per restore request.
const BATCH: u32 = 100;
/// Empty requests before a restore stops: a little past gaps (failed
/// attempts, skips after a collision).
const DEEP_GAP: u32 = 10;
/// Pause between scan requests, so a scan never floods the mint.
const SCAN_PACE: Duration = Duration::from_millis(300);

/// The shared-counter and scan-state key for `slot` of keyset `ks`.
fn slot_key(ks: &cdk::nuts::Id, slot: u32) -> String {
    if slot == 0 {
        ks.to_string()
    } else {
        format!("{ks}@{slot}")
    }
}

/// What one stretch of a scan found.
#[derive(Debug, Default, Clone, Copy)]
struct Stretch {
    sats: u64,
    /// One past the highest output the mint had signed (else where it began).
    next: u32,
    /// Where the scan stopped.
    end: u32,
    /// Requests made.
    batches: u32,
}

/// Requests that don't need repeating every sync: when each last went out.
#[derive(Debug, Default)]
pub(crate) struct Throttle {
    pub release: Option<std::time::Instant>,
    pub melts: Option<std::time::Instant>,
    pub sends: Option<std::time::Instant>,
    check_mint: Option<std::time::Instant>,
    quotes: HashMap<String, std::time::Instant>,
}

/// How far a range of a keyset has been scanned.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
struct KeysetScan {
    /// One past the highest counter the mint has signed (that we've seen).
    next: u32,
    /// An inactive keyset, scanned to the end.
    done: bool,
    /// Everything below this has been fetched (another device's outputs up
    /// to its counter).
    #[serde(default)]
    scanned: u32,
}

impl KeysetScan {
    /// Where to carry on scanning `slot` from.
    fn pos(&self, slot: u32) -> u32 {
        self.next.max(self.scanned).max(slot.saturating_mul(SLOT))
    }

    /// Note a stretch scanned, which covered everything below `covered`
    /// (another device's counter) as far as it got.
    fn record(&mut self, st: &Stretch, covered: u32) {
        self.next = self.next.max(st.next);
        self.scanned = self.scanned.max(st.next).max(covered.min(st.end));
    }
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

/// One request dropped by a flaky connection (a phone switching networks)
/// shouldn't end a scan: one more try after 2 s. No more than that, so an
/// unreachable mint isn't hammered.
async fn retry<T, F, Fut>(mut call: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, cdk::Error>>,
{
    let mut tries = 0;
    loop {
        match call().await {
            Ok(v) => return Ok(v),
            Err(e) if tries < 1 && is_transient(&e) => {
                tries += 1;
                tracing::info!("ecash: mint request failed ({e}); retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
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

/// Whether this device can reach the mint, from what its last requests did.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MintContact {
    /// Unix seconds of the last request the mint answered.
    pub last_ok_at: Option<u64>,
    /// Why the latest attempt couldn't reach it; None once one gets through.
    pub error: Option<String>,
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl EcashAccount {
    /// Can this device reach the mint? (No request of its own: it reports
    /// what the account's last requests found.)
    pub fn mint_contact(&self) -> MintContact {
        self.contact.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Record how a request to the mint went: an answer (even a refusal)
    /// means it's reachable; a connection error means it isn't.
    pub(crate) fn note_contact<T>(&self, r: &Result<T>) {
        let mut c = self.contact.lock().unwrap_or_else(|e| e.into_inner());
        match r {
            Ok(_) => {
                c.last_ok_at = Some(now_secs());
                c.error = None;
            }
            Err(e) if is_unreachable(&e.to_string()) => c.error = Some(e.to_string()),
            Err(_) => {
                c.last_ok_at = Some(now_secs());
                c.error = None;
            }
        }
    }
}

/// A connection that never reached the mint (as opposed to the mint
/// answering with an error).
fn is_unreachable(m: &str) -> bool {
    let m = m.to_ascii_lowercase();
    ["transport", "unreachable", "no route", "timed out", "timeout", "connection refused", "connection reset", "dns", "failed to lookup"]
        .iter()
        .any(|w| m.contains(w))
}

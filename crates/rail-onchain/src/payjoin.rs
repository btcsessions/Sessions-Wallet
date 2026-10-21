//! Payjoin (BIP78 / BIP77) for hot on-chain accounts.
//!
//! **Sender** (always on for `Custody::Hot`): when a quote carries a payjoin
//! URI, [`OnchainAccount::send`] signs and finalizes the Original PSBT, asks
//! the receiver for a proposal (BIP77 v2 through an OHTTP relay when the URI
//! has v2 parameters, otherwise BIP78 v1 over HTTPS), lets the library's
//! sender checks validate it, runs our own backstop checks, signs our inputs
//! and broadcasts. On *any* problem the Original is broadcast instead, so a
//! payment never fails because of payjoin.
//!
//! **Receiver** (opt-in, `OnchainConfig::payjoin_receive`): `receive()` opens
//! a BIP77 session at the directory, persists it under `payjoin_dir`, and hands
//! out a BIP21 URI with `pj=`. The app calls [`OnchainAccount::payjoin_tick`]
//! periodically to drive open sessions.
//!
//! Probing mitigation (receiver): Esplora has no `testmempoolaccept`, so we
//! can't fully prove the Original is broadcastable. We check that its inputs
//! exist, are unspent and aren't ours, and then use the library's
//! non-interactive path. If neither the payjoin nor the Original has appeared
//! on the network ~10 minutes after we received the Original, we broadcast the
//! Original ourselves. A prober therefore always pays us; at worst they learn
//! one of our coins in exchange for a real payment.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ::payjoin as pdk;
use async_trait::async_trait;
use bdk_wallet::bitcoin::secp256k1::rand::{self, seq::SliceRandom, RngCore};
use bdk_wallet::bitcoin::transaction::InputWeightPrediction;
use bdk_wallet::bitcoin::{
    Address, Amount, FeeRate, OutPoint, Psbt, ScriptBuf, Transaction, TxIn, Txid, Weight,
};
use bdk_wallet::{SignOptions, Wallet};
use pdk::persist::{OptionalTransitionOutcome, SessionPersister};
use pdk::receive::v2::{ReceiveSession, ReceiverBuilder, SessionEvent as RecvEvent};
use pdk::receive::InputPair;
use pdk::ImplementationError;
use serde::{Deserialize, Serialize};

use sessions_core::types::Custody;

use crate::{err, now, Inner, OnchainAccount};

/// Default BIP77 payjoin directory (from the rust-payjoin `payjoin-cli` README).
pub const DEFAULT_DIRECTORY: &str = "https://payjo.in";
/// Default public OHTTP relays (from the rust-payjoin `payjoin-cli` README).
/// One is picked at random per request batch.
pub const DEFAULT_RELAYS: &[&str] = &[
    "https://pj.benalleng.com",
    "https://pj.bobspacebkk.com",
    "https://payjoin.achow101.com",
];

/// BIP78 v1: one request, bounded.
const V1_TIMEOUT: Duration = Duration::from_secs(30);
/// BIP77 v2: total polling budget for the sender.
const V2_POLL_BUDGET: Duration = Duration::from_secs(60);
/// Single relay request cap (the directory long-polls).
const RELAY_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Receiver sessions live this long at the directory.
const RECEIVE_SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// If neither the payjoin nor the Original is on the network this long after
/// we received the Original, we broadcast the Original ourselves.
const RECEIVER_FALLBACK_AFTER_SECS: u64 = 10 * 60;
/// Session files are removed after this long no matter what.
const SESSION_FILE_MAX_AGE_SECS: u64 = 8 * 24 * 60 * 60;
/// Most a receiver will pay (effective rate) for its own added input when the
/// sender's fee contribution doesn't cover it.
const RECEIVER_MAX_FEE_RATE_SAT_VB: u64 = 10;
/// Seen-input records are pruned after this long.
const SEEN_INPUT_TTL_SECS: u64 = 90 * 24 * 60 * 60;
/// Original PSBTs with more inputs than this aren't worth checking one by one.
const MAX_ORIGINAL_INPUTS: usize = 50;
/// Give up on a session after this many failed steps.
const MAX_SESSION_FAILURES: u32 = 6;

pub(crate) const DESCRIPTION_PAYJOIN: &str = "Payjoin";
pub(crate) const DESCRIPTION_FALLBACK: &str = "Payjoin unavailable, sent as a normal transaction";

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Minimal HTTP POST, so tests can run payjoin without the network.
#[async_trait]
pub trait Http: Send + Sync {
    async fn post(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
        timeout: Duration,
    ) -> std::result::Result<Vec<u8>, String>;
}

/// reqwest + rustls.
pub struct ReqwestHttp(reqwest::Client);

impl Default for ReqwestHttp {
    fn default() -> Self {
        Self(reqwest::Client::new())
    }
}

#[async_trait]
impl Http for ReqwestHttp {
    async fn post(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
        timeout: Duration,
    ) -> std::result::Result<Vec<u8>, String> {
        let mut res = self
            .0
            .post(url)
            .header("content-type", content_type)
            .body(body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = res.status();
        let mut out = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|e| e.to_string())? {
            out.extend_from_slice(&chunk);
            if out.len() > pdk::MAX_CONTENT_LENGTH {
                return Err("response too large".into());
            }
        }
        if !status.is_success() {
            let snippet: String = String::from_utf8_lossy(&out).chars().take(200).collect();
            return Err(format!("HTTP {status}: {snippet}"));
        }
        Ok(out)
    }
}

/// Directory and relays. Defaults to [`DEFAULT_DIRECTORY`] / [`DEFAULT_RELAYS`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayjoinEndpoints {
    pub directory: String,
    pub relays: Vec<String>,
}

impl Default for PayjoinEndpoints {
    fn default() -> Self {
        Self {
            directory: DEFAULT_DIRECTORY.into(),
            relays: DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl PayjoinEndpoints {
    fn pick_relay(&self) -> Option<&str> {
        self.relays
            .choose(&mut rand::thread_rng())
            .map(String::as_str)
    }
}

/// Per-account payjoin state.
pub(crate) struct PayjoinState {
    pub(crate) endpoints: PayjoinEndpoints,
    /// Receiver session directory; `Some` only for hot accounts with a dir.
    /// Existing sessions are always driven to completion from here.
    pub(crate) receive_dir: Option<PathBuf>,
    /// Whether new receive requests advertise payjoin. Can change at runtime.
    pub(crate) receive_on: std::sync::atomic::AtomicBool,
    pub(crate) http: Arc<dyn Http>,
    /// Serialises `payjoin_tick` and session creation.
    pub(crate) tick: tokio::sync::Mutex<()>,
}

impl PayjoinState {
    pub(crate) fn new(custody: &Custody, receive: bool, dir: Option<PathBuf>) -> Self {
        let receive_dir = match (custody, dir) {
            (Custody::Hot, Some(d)) => Some(d),
            (Custody::Hot, None) if receive => {
                tracing::warn!("payjoin receive enabled without payjoin_dir; staying off");
                None
            }
            _ => None,
        };
        let receive_on = std::sync::atomic::AtomicBool::new(receive && receive_dir.is_some());
        Self {
            endpoints: PayjoinEndpoints::default(),
            receive_dir,
            receive_on,
            http: Arc::new(ReqwestHttp::default()),
            tick: tokio::sync::Mutex::new(()),
        }
    }
}

// ---------------------------------------------------------------------------
// URI helpers
// ---------------------------------------------------------------------------

/// Which payjoin protocol a BIP21 URI asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PjVersion {
    /// BIP78: synchronous HTTPS endpoint.
    V1,
    /// BIP77: async via directory + OHTTP.
    V2,
}

fn parse_pj_uri(uri: &str, network: bdk_wallet::bitcoin::Network) -> Result<pdk::PjUri, String> {
    let parsed = pdk::Uri::from_str(uri.trim()).map_err(|e| format!("bad payjoin URI: {e}"))?;
    let checked = parsed
        .require_network(network)
        .map_err(|e| format!("payjoin URI network: {e}"))?;
    checked
        .check_pj_supported()
        .map_err(|_| "URI doesn't offer payjoin".to_string())
}

/// Detect the payjoin version of a BIP21 URI (`None` if it has no usable `pj=`).
pub fn pj_version(uri: &str, network: bdk_wallet::bitcoin::Network) -> Option<PjVersion> {
    let pj = parse_pj_uri(uri, network).ok()?;
    match pj.extras().pj_param() {
        pdk::PjParam::V1(_) => Some(PjVersion::V1),
        pdk::PjParam::V2(_) => Some(PjVersion::V2),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Sender
// ---------------------------------------------------------------------------

/// What `send` should broadcast.
#[derive(Debug, Clone)]
pub(crate) struct SendChoice {
    pub(crate) tx: Transaction,
    /// True when `tx` is the payjoin; false when it's the Original.
    pub(crate) payjoin: bool,
    /// Extra fee the sender paid towards the receiver's input (payjoin only).
    pub(crate) extra_fee_sat: u64,
}

fn psbt_fee_rate(psbt: &Psbt) -> Option<FeeRate> {
    let fee = psbt.fee().ok()?;
    let weight = psbt.clone().extract_tx_unchecked_fee_rate().weight();
    fee.div_by_weight_floor(weight)
}

/// One P2WPKH input (our hot wallets are BIP84) at `rate`: the most we'll let
/// the receiver take from our change for their input.
fn max_fee_contribution(rate: FeeRate) -> Amount {
    let w = InputWeightPrediction::P2WPKH_MAX.weight() + Weight::from_non_witness_data_size(40);
    rate.fee_wu(w).unwrap_or(Amount::ZERO)
}

/// Talk to the receiver. Returns the proposal after the library's checks.
async fn negotiate(
    original: &Psbt,
    uri: &str,
    network: bdk_wallet::bitcoin::Network,
    endpoints: &PayjoinEndpoints,
    http: &dyn Http,
) -> Result<(Psbt, Amount), String> {
    let pj_uri = parse_pj_uri(uri, network)?;
    let rate = psbt_fee_rate(original).ok_or("can't compute the Original's fee rate")?;
    let max_contrib = max_fee_contribution(rate);

    match pj_uri.extras().pj_param().clone() {
        pdk::PjParam::V1(_) => {
            let sender = pdk::send::v1::SenderBuilder::new(original.clone(), pj_uri)
                .always_disable_output_substitution()
                .build_with_additional_fee(max_contrib, None, rate, true)
                .map_err(|e| format!("build v1 sender: {e}"))?;
            let (req, ctx) = sender.create_v1_post_request();
            let body = tokio::time::timeout(
                V1_TIMEOUT,
                http.post(&req.url, req.content_type, req.body, V1_TIMEOUT),
            )
            .await
            .map_err(|_| "payjoin v1 timed out".to_string())??;
            let proposal = ctx
                .process_response(&body)
                .map_err(|e| format!("invalid v1 proposal: {e}"))?;
            Ok((proposal, max_contrib))
        }
        pdk::PjParam::V2(param) => {
            let relay = endpoints.pick_relay().ok_or("no OHTTP relay configured")?;
            let persister =
                pdk::persist::InMemoryPersister::<pdk::send::v2::SessionEvent>::default();
            let sender = pdk::send::v2::SenderBuilder::from_parts(
                original.clone(),
                &param,
                pj_uri.address(),
                pj_uri.amount(),
            )
            .always_disable_output_substitution()
            .build_with_additional_fee(max_contrib, None, rate, true)
            .map_err(|e| format!("build v2 sender: {e}"))?
            .save(&persister)
            .map_err(|e| e.to_string())?;
            let (req, ctx) = sender
                .create_v2_post_request(relay)
                .map_err(|e| format!("v2 request: {e}"))?;
            let body = http
                .post(&req.url, req.content_type, req.body, RELAY_REQUEST_TIMEOUT)
                .await?;
            let mut polling = sender
                .process_response(&body, ctx)
                .save(&persister)
                .map_err(|e| format!("v2 post: {e}"))?;
            let deadline = Instant::now() + V2_POLL_BUDGET;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err("payjoin v2: receiver didn't answer in time".into());
                }
                let started = Instant::now();
                let (req, ctx) = polling
                    .create_poll_request(relay)
                    .map_err(|e| format!("v2 poll: {e}"))?;
                let body = tokio::time::timeout(
                    left,
                    http.post(
                        &req.url,
                        req.content_type,
                        req.body,
                        left.min(RELAY_REQUEST_TIMEOUT),
                    ),
                )
                .await
                .map_err(|_| "payjoin v2: receiver didn't answer in time".to_string())??;
                match polling.process_response(&body, ctx).save(&persister) {
                    Ok(OptionalTransitionOutcome::Progress(psbt)) => {
                        return Ok((psbt, max_contrib))
                    }
                    Ok(OptionalTransitionOutcome::Stasis(p)) => polling = p,
                    Err(e) => return Err(format!("invalid v2 proposal: {e}")),
                }
                if started.elapsed() < Duration::from_secs(2) {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
        #[allow(unreachable_patterns)]
        _ => Err("unsupported payjoin version".into()),
    }
}

/// The library restores the Original's UTXO data on our inputs, but BDK's
/// finalizer had already stripped key origins, which we need to sign again.
fn restore_signing_info(proposal: &mut Psbt, unfinalized: &Psbt) {
    for (txin, input) in proposal
        .unsigned_tx
        .input
        .iter()
        .zip(proposal.inputs.iter_mut())
    {
        if let Some(i) = unfinalized
            .unsigned_tx
            .input
            .iter()
            .position(|o| o.previous_output == txin.previous_output)
        {
            let src = &unfinalized.inputs[i];
            input.bip32_derivation = src.bip32_derivation.clone();
            input.tap_key_origins = src.tap_key_origins.clone();
            input.tap_internal_key = src.tap_internal_key;
            input.witness_script = src.witness_script.clone();
            input.redeem_script = src.redeem_script.clone();
            input.witness_utxo = src.witness_utxo.clone();
            input.non_witness_utxo = src.non_witness_utxo.clone();
            input.partial_sigs.clear();
            input.final_script_sig = None;
            input.final_script_witness = None;
        }
    }
}

/// Our own checks on top of the library's: the receiver can't add our coins,
/// the payee is paid at least as much, and our net spend grows by at most the
/// fee contribution we offered. Returns the extra fee we pay.
fn backstop_checks(
    wallet: &Wallet,
    original: &Psbt,
    proposal: &Psbt,
    payee: &ScriptBuf,
    max_contrib: Amount,
) -> Result<u64, String> {
    let orig_ins: BTreeSet<OutPoint> = original
        .unsigned_tx
        .input
        .iter()
        .map(|i| i.previous_output)
        .collect();
    let mut our_in = 0u64;
    for (i, txin) in proposal.unsigned_tx.input.iter().enumerate() {
        let prev = proposal.inputs[i]
            .witness_utxo
            .clone()
            .or_else(|| {
                proposal.inputs[i]
                    .non_witness_utxo
                    .as_ref()
                    .and_then(|t| t.output.get(txin.previous_output.vout as usize).cloned())
            })
            .ok_or("proposal input without UTXO data")?;
        if orig_ins.contains(&txin.previous_output) {
            our_in += prev.value.to_sat();
        } else if wallet.is_mine(prev.script_pubkey.clone())
            || wallet.get_utxo(txin.previous_output).is_some()
        {
            return Err("receiver added one of our own coins".into());
        }
    }
    if proposal.unsigned_tx.input.len() <= original.unsigned_tx.input.len() {
        return Err("receiver didn't add an input".into());
    }
    let ours_out = |p: &Psbt| -> u64 {
        p.unsigned_tx
            .output
            .iter()
            .filter(|o| wallet.is_mine(o.script_pubkey.clone()))
            .map(|o| o.value.to_sat())
            .sum()
    };
    let to_payee = |p: &Psbt| -> u64 {
        p.unsigned_tx
            .output
            .iter()
            .filter(|o| o.script_pubkey == *payee)
            .map(|o| o.value.to_sat())
            .sum()
    };
    if to_payee(proposal) < to_payee(original) {
        return Err("proposal pays the recipient less".into());
    }
    let orig_net = original
        .unsigned_tx
        .input
        .iter()
        .enumerate()
        .map(|(i, _)| {
            original
                .spend_utxo(i)
                .map(|o| o.value.to_sat())
                .unwrap_or(u64::MAX / 4)
        })
        .sum::<u64>()
        .saturating_sub(ours_out(original));
    let new_net = our_in.saturating_sub(ours_out(proposal));
    let extra = new_net.saturating_sub(orig_net);
    if extra > max_contrib.to_sat() {
        return Err(format!(
            "proposal costs us {extra} sats more, above the {} offered",
            max_contrib.to_sat()
        ));
    }
    Ok(extra)
}

impl OnchainAccount {
    /// Try payjoin; always returns something safe to broadcast.
    ///
    /// `original` must be finalized; `unfinalized` is the same PSBT just
    /// before finalization (it still has our key origins).
    pub(crate) async fn payjoin_or_original(
        &self,
        original: &Psbt,
        unfinalized: &Psbt,
        uri: &str,
        http: &dyn Http,
    ) -> SendChoice {
        let fallback = |reason: String| {
            tracing::info!("payjoin not used, sending the original transaction: {reason}");
            SendChoice {
                tx: original.clone().extract_tx_unchecked_fee_rate(),
                payjoin: false,
                extra_fee_sat: 0,
            }
        };
        if !matches!(self.custody, Custody::Hot) {
            return fallback("payjoin is only used from hot accounts".into());
        }
        let net = self.network.to_bitcoin();
        let payee = match parse_pj_uri(uri, net) {
            Ok(u) => u.address().script_pubkey(),
            Err(e) => return fallback(e),
        };
        let (mut proposal, max_contrib) =
            match negotiate(original, uri, net, &self.pj.endpoints, http).await {
                Ok(p) => p,
                Err(e) => return fallback(e),
            };
        restore_signing_info(&mut proposal, unfinalized);
        if let Err(e) = self.sign_locally(&mut proposal) {
            return fallback(format!("signing: {e}"));
        }
        let mut inner = self.inner.lock().await;
        let extra = match backstop_checks(&inner.wallet, original, &proposal, &payee, max_contrib) {
            Ok(x) => x,
            Err(e) => return fallback(e),
        };
        match inner
            .wallet
            .finalize_psbt(&mut proposal, SignOptions::default())
        {
            Ok(true) => {}
            Ok(false) => return fallback("couldn't finalize the payjoin".into()),
            Err(e) => return fallback(format!("finalize: {e}")),
        }
        // Remember the receiver's input values so our history can show the fee.
        for (txin, input) in proposal.unsigned_tx.input.iter().zip(&proposal.inputs) {
            if let Some(txout) = &input.witness_utxo {
                if inner.wallet.get_utxo(txin.previous_output).is_none() {
                    inner
                        .wallet
                        .insert_txout(txin.previous_output, txout.clone());
                }
            }
        }
        match proposal.extract_tx() {
            Ok(tx) => {
                tracing::info!(
                    "payjoin proposal accepted: {} (extra fee {extra} sats)",
                    tx.compute_txid()
                );
                SendChoice {
                    tx,
                    payjoin: true,
                    extra_fee_sat: extra,
                }
            }
            Err(e) => {
                drop(inner);
                fallback(format!("extract: {e}"))
            }
        }
    }

    /// Broadcast, treating "already known to the server" as success.
    pub(crate) async fn broadcast_idempotent(&self, tx: &Transaction) -> sessions_core::Result<()> {
        match self.client.broadcast(tx).await {
            Ok(()) => Ok(()),
            Err(e) => match self.client.get_tx(&tx.compute_txid()).await {
                Ok(Some(_)) => Ok(()),
                _ => Err(err(e)),
            },
        }
    }

    /// Override the payjoin directory and OHTTP relays.
    pub fn with_payjoin_endpoints(mut self, endpoints: PayjoinEndpoints) -> Self {
        self.pj.endpoints = endpoints;
        self
    }
}

// ---------------------------------------------------------------------------
// Receiver: persistence
// ---------------------------------------------------------------------------

/// Our bookkeeping next to the library's event log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionMeta {
    pub(crate) created_at: u64,
    /// When the sender's Original first reached us.
    pub(crate) original_at: Option<u64>,
    /// We broadcast the Original ourselves.
    pub(crate) fallback_broadcast: bool,
    /// Our coin committed to this session.
    pub(crate) contributed: Option<OutPoint>,
    pub(crate) payjoin_txid: Option<Txid>,
    pub(crate) failures: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionFile {
    version: u32,
    events: Vec<RecvEvent>,
    closed: bool,
    meta: SessionMeta,
}

/// One receiver session as a JSON file, rewritten atomically on every change.
pub(crate) struct FilePersister {
    path: PathBuf,
    file: std::sync::Mutex<SessionFile>,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        // Best effort: make the rename durable.
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

fn lock_poisoned() -> io::Error {
    io::Error::other("payjoin session lock poisoned")
}

impl FilePersister {
    pub(crate) fn create(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let mut id = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut id);
        let name: String = id.iter().map(|b| format!("{b:02x}")).collect();
        let path = dir.join(format!("pj-{name}.json"));
        let file = SessionFile {
            version: 1,
            events: vec![],
            closed: false,
            meta: SessionMeta {
                created_at: now(),
                ..Default::default()
            },
        };
        Ok(Self {
            path,
            file: std::sync::Mutex::new(file),
        })
    }

    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let file: SessionFile = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        Ok(Self {
            path: path.to_path_buf(),
            file: std::sync::Mutex::new(file),
        })
    }

    fn write(&self, f: &SessionFile) -> io::Result<()> {
        let bytes = serde_json::to_vec(f).map_err(io::Error::other)?;
        write_atomic(&self.path, &bytes)
    }

    pub(crate) fn meta(&self) -> SessionMeta {
        self.file.lock().map(|f| f.meta.clone()).unwrap_or_default()
    }

    pub(crate) fn update_meta(&self, change: impl FnOnce(&mut SessionMeta)) -> io::Result<()> {
        let mut f = self.file.lock().map_err(|_| lock_poisoned())?;
        let mut next = f.clone();
        change(&mut next.meta);
        self.write(&next)?;
        *f = next;
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.file.lock().map(|f| f.closed).unwrap_or(false)
    }

    fn remove(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl SessionPersister for FilePersister {
    type InternalStorageError = io::Error;
    type SessionEvent = RecvEvent;

    fn save_event(&self, event: RecvEvent) -> io::Result<()> {
        let mut f = self.file.lock().map_err(|_| lock_poisoned())?;
        let mut next = f.clone();
        next.events.push(event);
        self.write(&next)?;
        *f = next;
        Ok(())
    }

    fn load(&self) -> io::Result<Box<dyn Iterator<Item = RecvEvent>>> {
        let f = self.file.lock().map_err(|_| lock_poisoned())?;
        Ok(Box::new(f.events.clone().into_iter()))
    }

    fn close(&self) -> io::Result<()> {
        let mut f = self.file.lock().map_err(|_| lock_poisoned())?;
        let mut next = f.clone();
        next.closed = true;
        self.write(&next)?;
        *f = next;
        Ok(())
    }
}

/// Inputs of Originals we've already processed: outpoint -> (session, when).
#[derive(Debug, Default, Serialize, Deserialize)]
struct SeenInputs(BTreeMap<String, (String, u64)>);

impl SeenInputs {
    fn path(dir: &Path) -> PathBuf {
        dir.join("seen-inputs.json")
    }
    fn load(dir: &Path) -> Self {
        std::fs::read(Self::path(dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
    fn save(&mut self, dir: &Path) -> io::Result<()> {
        let cutoff = now().saturating_sub(SEEN_INPUT_TTL_SECS);
        self.0.retain(|_, (_, t)| *t >= cutoff);
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        write_atomic(&Self::path(dir), &bytes)
    }
    /// Seen by a *different* session (our own session may be replayed).
    fn seen_elsewhere(&self, op: &OutPoint, session: &str) -> bool {
        self.0
            .get(&op.to_string())
            .is_some_and(|(s, _)| s != session)
    }
}

fn session_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut v: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("pj-") && n.ends_with(".json"))
        })
        .collect();
    v.sort();
    v
}

// ---------------------------------------------------------------------------
// Receiver: wallet helpers
// ---------------------------------------------------------------------------

/// Our confirmed coins, as payjoin inputs carrying our key origins.
pub(crate) fn candidate_inputs(wallet: &Wallet, reserved: &BTreeSet<OutPoint>) -> Vec<InputPair> {
    wallet
        .list_unspent()
        .filter(|u| u.chain_position.is_confirmed() && !reserved.contains(&u.outpoint))
        .filter_map(|u| {
            let op = u.outpoint;
            let psbtin = wallet.get_psbt_input(u, None, true).ok()?;
            let txin = TxIn {
                previous_output: op,
                ..Default::default()
            };
            InputPair::new(txin, psbtin, None).ok()
        })
        .collect()
}

fn owns_outpoint(wallet: &Wallet, op: &OutPoint) -> bool {
    wallet.get_utxo(*op).is_some()
        || wallet
            .tx_graph()
            .get_txout(*op)
            .is_some_and(|o| wallet.is_mine(o.script_pubkey.clone()))
}

impl OnchainAccount {
    /// Sign and finalize only our own (contributed) inputs of a payjoin
    /// proposal. The library rejects the result if any sender input got signed.
    pub(crate) fn sign_receiver_inputs(
        &self,
        wallet: &Wallet,
        psbt: &Psbt,
    ) -> std::result::Result<Psbt, ImplementationError> {
        let mut psbt = psbt.clone();
        let ours: Vec<usize> = psbt
            .inputs
            .iter()
            .enumerate()
            .filter(|(_, i)| {
                i.witness_utxo
                    .as_ref()
                    .is_some_and(|o| wallet.is_mine(o.script_pubkey.clone()))
                    && i.final_script_witness.is_none()
            })
            .map(|(n, _)| n)
            .collect();
        if ours.is_empty() {
            return Err(ImplementationError::from("no receiver input to sign"));
        }
        self.sign_locally(&mut psbt)
            .map_err(|e| ImplementationError::from(e.to_string().as_str()))?;
        // Returns false because the sender's inputs aren't ours; check ours.
        let _ = wallet
            .finalize_psbt(&mut psbt, SignOptions::default())
            .map_err(|e| ImplementationError::from(e.to_string().as_str()))?;
        if ours
            .iter()
            .any(|n| psbt.inputs[*n].final_script_witness.is_none())
        {
            return Err(ImplementationError::from("couldn't sign our payjoin input"));
        }
        Ok(psbt)
    }

    /// When receiving is on: open a v2 session and return its BIP21 URI.
    /// `None` means "hand out a plain address" (off, no confirmed coin to
    /// contribute, or the directory is unreachable).
    pub(crate) async fn payjoin_receive_uri(
        &self,
        address: &Address,
        amount_sat: Option<u64>,
    ) -> Option<String> {
        if !self.pj.receive_on.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        let dir = self.pj.receive_dir.clone()?;
        {
            let inner = self.inner.lock().await;
            if candidate_inputs(&inner.wallet, inner.labels.frozen()).is_empty() {
                tracing::info!("payjoin receive: no confirmed coin to contribute, plain address");
                return None;
            }
        }
        match self.open_receive_session(&dir, address, amount_sat).await {
            Ok(uri) => Some(uri),
            Err(e) => {
                tracing::warn!("payjoin receive unavailable, plain address: {e}");
                None
            }
        }
    }

    async fn open_receive_session(
        &self,
        dir: &Path,
        address: &Address,
        amount_sat: Option<u64>,
    ) -> Result<String, String> {
        let relay = self.pj.endpoints.pick_relay().ok_or("no OHTTP relay")?;
        let directory = self.pj.endpoints.directory.as_str();
        let keys = tokio::time::timeout(
            Duration::from_secs(15),
            pdk::io::fetch_ohttp_keys(relay, directory),
        )
        .await
        .map_err(|_| "OHTTP key fetch timed out".to_string())?
        .map_err(|e| format!("OHTTP keys: {e}"))?;
        let _guard = self.pj.tick.lock().await;
        let persister = FilePersister::create(dir).map_err(|e| e.to_string())?;
        let mut builder = ReceiverBuilder::new(address.clone(), directory, keys)
            .map_err(|e| e.to_string())?
            .with_expiration(RECEIVE_SESSION_TTL)
            .with_max_fee_rate(
                FeeRate::from_sat_per_vb(RECEIVER_MAX_FEE_RATE_SAT_VB)
                    .unwrap_or(FeeRate::BROADCAST_MIN),
            );
        if let Some(sats) = amount_sat {
            builder = builder.with_amount(Amount::from_sat(sats));
        }
        // Written to disk before the URI leaves this function.
        let receiver = builder
            .build()
            .save(&persister)
            .map_err(|e| e.to_string())?;
        let uri = receiver.pj_uri().to_string();
        tracing::info!(
            "payjoin receive session opened ({})",
            persister.path.display()
        );
        Ok(uri)
    }
}

// ---------------------------------------------------------------------------
// Receiver: driving sessions
// ---------------------------------------------------------------------------

enum Step {
    /// State advanced; replay and continue.
    Advanced,
    /// Waiting on the sender or the network; try again next tick.
    Wait,
    /// Session finished; file removed.
    Done,
}

impl OnchainAccount {
    /// Drive every open payjoin receive session one round forward: poll the
    /// directory, validate the sender's Original, contribute one confirmed
    /// coin, sign, respond, watch for the result, and fall back to
    /// broadcasting the Original when needed. Safe to call repeatedly and
    /// after restarts; a no-op when receiving is off. Call it every ~30 s
    /// while the app is open (sync the wallet first so coin ownership and
    /// confirmations are current).
    pub async fn payjoin_tick(&self) -> sessions_core::Result<()> {
        let Some(dir) = self.pj.receive_dir.clone() else {
            return Ok(());
        };
        let _guard = self.pj.tick.lock().await;
        let files = session_files(&dir);
        // Coins already promised to other live sessions.
        let mut reserved = BTreeSet::new();
        let mut sessions = Vec::new();
        for path in files {
            match FilePersister::open(&path) {
                Ok(p) => {
                    if let Some(op) = p.meta().contributed {
                        if !p.is_closed() {
                            reserved.insert(op);
                        }
                    }
                    sessions.push(p);
                }
                Err(e) => {
                    tracing::warn!("unreadable payjoin session {}: {e}", path.display());
                    let old = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age.as_secs() > SESSION_FILE_MAX_AGE_SECS);
                    if old {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
        for p in sessions {
            if let Err(e) = self.drive_session(&dir, &p, &mut reserved).await {
                tracing::warn!("payjoin session {}: {e}", p.path.display());
                let failures = p.meta().failures + 1;
                let _ = p.update_meta(|m| m.failures = failures);
                if failures >= MAX_SESSION_FAILURES {
                    tracing::info!("giving up on payjoin session {}", p.path.display());
                    let _ = self.cancel_session(&p).await;
                }
            }
        }
        Ok(())
    }

    async fn drive_session(
        &self,
        dir: &Path,
        p: &FilePersister,
        reserved: &mut BTreeSet<OutPoint>,
    ) -> Result<(), String> {
        let meta = p.meta();
        if now().saturating_sub(meta.created_at) > SESSION_FILE_MAX_AGE_SECS {
            p.remove();
            return Ok(());
        }
        if p.is_closed() {
            p.remove();
            return Ok(());
        }
        for _ in 0..16 {
            let (session, history) = match pdk::receive::v2::replay_event_log(p) {
                Ok(x) => x,
                Err(e) if e.is_expired() => {
                    if let Some(tx) = e.expiry_fallback_tx() {
                        if !p.meta().fallback_broadcast {
                            tracing::info!("payjoin session expired; broadcasting the Original");
                            self.broadcast_fallback(tx).await?;
                            let _ = p.update_meta(|m| m.fallback_broadcast = true);
                        }
                    }
                    p.remove();
                    return Ok(());
                }
                Err(e) => {
                    p.remove();
                    return Err(format!("unreplayable session removed: {e}"));
                }
            };
            if !matches!(
                session,
                ReceiveSession::Initialized(_) | ReceiveSession::Closed(_)
            ) && p.meta().original_at.is_none()
            {
                p.update_meta(|m| m.original_at = Some(now()))
                    .map_err(|e| e.to_string())?;
            }
            let is_monitor = matches!(session, ReceiveSession::Monitor(_));
            match self.step(dir, p, session, &history, reserved).await? {
                Step::Advanced => continue,
                Step::Done => return Ok(()),
                Step::Wait => {
                    self.maybe_self_broadcast(p, &history, is_monitor).await?;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// The 10-minute rule: if neither the payjoin nor the Original is on the
    /// network well after the Original arrived, broadcast the Original.
    async fn maybe_self_broadcast(
        &self,
        p: &FilePersister,
        history: &pdk::receive::v2::SessionHistory,
        is_monitor: bool,
    ) -> Result<(), String> {
        let meta = p.meta();
        let Some(at) = meta.original_at else {
            return Ok(());
        };
        if meta.fallback_broadcast || now().saturating_sub(at) < RECEIVER_FALLBACK_AFTER_SECS {
            return Ok(());
        }
        let Some(fallback) = history.fallback_tx() else {
            return Ok(());
        };
        if let Some(pj) = meta.payjoin_txid {
            if self
                .client
                .get_tx(&pj)
                .await
                .map_err(|e| e.to_string())?
                .is_some()
            {
                return Ok(());
            }
        }
        tracing::info!("payjoin not broadcast by the sender in time; broadcasting the Original");
        self.broadcast_fallback(&fallback).await?;
        p.update_meta(|m| m.fallback_broadcast = true)
            .map_err(|e| e.to_string())?;
        if !is_monitor {
            self.cancel_session(p).await?;
        }
        Ok(())
    }

    async fn broadcast_fallback(&self, tx: &Transaction) -> Result<(), String> {
        self.broadcast_idempotent(tx)
            .await
            .map_err(|e| e.to_string())?;
        let mut inner = self.inner.lock().await;
        let Inner { wallet, conn, .. } = &mut *inner;
        wallet.apply_unconfirmed_txs([(tx.clone(), now())]);
        wallet.persist(conn).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Cancel from whatever state the session is in. Sessions holding a
    /// verified Original go to "pending fallback" (broadcast next step).
    async fn cancel_session(&self, p: &FilePersister) -> Result<(), String> {
        let Ok((session, _)) = pdk::receive::v2::replay_event_log(p) else {
            p.remove();
            return Ok(());
        };
        let pending = match session {
            ReceiveSession::Initialized(r) => {
                r.cancel().save(p).map_err(|e| e.to_string())?;
                None
            }
            ReceiveSession::UncheckedOriginalPayload(r) => {
                r.cancel().save(p).map_err(|e| e.to_string())?;
                None
            }
            ReceiveSession::MaybeInputsOwned(r) => Some(r.cancel().save(p)),
            ReceiveSession::MaybeInputsSeen(r) => Some(r.cancel().save(p)),
            ReceiveSession::OutputsUnknown(r) => Some(r.cancel().save(p)),
            ReceiveSession::WantsOutputs(r) => Some(r.cancel().save(p)),
            ReceiveSession::WantsInputs(r) => Some(r.cancel().save(p)),
            ReceiveSession::WantsFeeRange(r) => Some(r.cancel().save(p)),
            ReceiveSession::ProvisionalProposal(r) => Some(r.cancel().save(p)),
            ReceiveSession::PayjoinProposal(r) => Some(r.cancel().save(p)),
            ReceiveSession::Monitor(r) => Some(r.cancel().save(p)),
            ReceiveSession::HasReplyableError(r) => {
                r.cancel().save(p).map_err(|e| e.to_string())?.map(Ok)
            }
            ReceiveSession::PendingFallback(r) => Some(Ok(r)),
            ReceiveSession::Closed(_) => None,
        };
        if let Some(r) = pending {
            let r = r.map_err(|e| e.to_string())?;
            self.broadcast_fallback(r.fallback_tx()).await?;
            r.close().save(p).map_err(|e| e.to_string())?;
        }
        if p.is_closed() {
            p.remove();
        }
        Ok(())
    }

    /// Esplora can't `testmempoolaccept`, so check what it can: every input
    /// exists, is unspent and isn't ours. Signatures aren't verified here;
    /// the 10-minute self-broadcast covers a bogus Original.
    async fn original_inputs_look_valid(&self, tx: &Transaction) -> Result<bool, String> {
        if tx.input.is_empty() || tx.input.len() > MAX_ORIGINAL_INPUTS {
            return Ok(false);
        }
        for txin in &tx.input {
            let op = txin.previous_output;
            let Some(prev) = self
                .client
                .get_tx(&op.txid)
                .await
                .map_err(|e| e.to_string())?
            else {
                return Ok(false);
            };
            let Some(out) = prev.output.get(op.vout as usize) else {
                return Ok(false);
            };
            if self
                .inner
                .lock()
                .await
                .wallet
                .is_mine(out.script_pubkey.clone())
            {
                return Ok(false);
            }
            let status = self
                .client
                .get_output_status(&op.txid, op.vout as u64)
                .await
                .map_err(|e| e.to_string())?;
            if status.is_none_or(|s| s.spent) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn post_via_relay(&self, req: pdk::Request) -> Result<Vec<u8>, String> {
        self.pj
            .http
            .post(&req.url, req.content_type, req.body, RELAY_REQUEST_TIMEOUT)
            .await
    }

    async fn step(
        &self,
        dir: &Path,
        p: &FilePersister,
        session: ReceiveSession,
        history: &pdk::receive::v2::SessionHistory,
        reserved: &mut BTreeSet<OutPoint>,
    ) -> Result<Step, String> {
        let s = |e: &dyn std::fmt::Display| e.to_string();
        let relay = self
            .pj
            .endpoints
            .pick_relay()
            .ok_or("no OHTTP relay")?
            .to_string();
        let session_id = p
            .path
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        match session {
            ReceiveSession::Initialized(r) => {
                let (req, ctx) = r.create_poll_request(relay.as_str()).map_err(|e| s(&e))?;
                let body = self.post_via_relay(req).await?;
                match r.process_response(&body, ctx).save(p).map_err(|e| s(&e))? {
                    OptionalTransitionOutcome::Progress(_) => {
                        tracing::info!("payjoin: received an Original PSBT");
                        Ok(Step::Advanced)
                    }
                    OptionalTransitionOutcome::Stasis(_) => Ok(Step::Wait),
                }
            }
            ReceiveSession::UncheckedOriginalPayload(r) => {
                let tx = r.extract_tx_to_check_broadcast_suitability();
                let ok = self.original_inputs_look_valid(&tx).await?;
                r.apply_broadcast_suitability(Some(FeeRate::BROADCAST_MIN), ok)
                    .save(p)
                    .map_err(|e| s(&e))?;
                Ok(Step::Advanced)
            }
            ReceiveSession::MaybeInputsOwned(r) => {
                let inner = self.inner.lock().await;
                let res = r
                    .check_inputs_not_owned(&mut |op| Ok(owns_outpoint(&inner.wallet, op)))
                    .save(p);
                drop(inner);
                res.map_err(|e| s(&e))?;
                Ok(Step::Advanced)
            }
            ReceiveSession::MaybeInputsSeen(r) => {
                let mut seen = SeenInputs::load(dir);
                let ops: Vec<OutPoint> = r.inputs_seen_checklist().map(|i| *i.value()).collect();
                let res = r
                    .check_no_inputs_seen_before(&mut |op| Ok(seen.seen_elsewhere(op, &session_id)))
                    .save(p);
                for op in ops {
                    seen.0
                        .entry(op.to_string())
                        .or_insert_with(|| (session_id.clone(), now()));
                }
                seen.save(dir).map_err(|e| s(&e))?;
                res.map_err(|e| s(&e))?;
                Ok(Step::Advanced)
            }
            ReceiveSession::OutputsUnknown(r) => {
                let inner = self.inner.lock().await;
                let res = r
                    .identify_receiver_outputs(&mut |spk| Ok(inner.wallet.is_mine(spk.to_owned())))
                    .save(p);
                drop(inner);
                res.map_err(|e| s(&e))?;
                Ok(Step::Advanced)
            }
            ReceiveSession::WantsOutputs(r) => {
                // No output substitution: the sender gets exactly what it asked for.
                r.commit_outputs().save(p).map_err(|e| s(&e))?;
                Ok(Step::Advanced)
            }
            ReceiveSession::WantsInputs(r) => {
                if !r.proposal_txid_is_stable() {
                    tracing::info!("payjoin: sender uses non-segwit inputs; not contributing");
                    self.cancel_session(p).await?;
                    return Ok(Step::Done);
                }
                let mine = p.meta().contributed;
                let candidates = {
                    let inner = self.inner.lock().await;
                    let mut others = reserved.clone();
                    if let Some(m) = mine {
                        others.remove(&m);
                    }
                    // Frozen coins are never contributed.
                    others.extend(inner.labels.frozen().iter().copied());
                    candidate_inputs(&inner.wallet, &others)
                };
                // After a restart, re-contribute the same coin if we can.
                let pick = match mine.and_then(|m| candidates.iter().find(|c| c.outpoint() == m)) {
                    Some(c) => c.clone(),
                    None if candidates.is_empty() => {
                        tracing::info!(
                            "payjoin: no confirmed coin to contribute; using the Original"
                        );
                        self.cancel_session(p).await?;
                        return Ok(Step::Done);
                    }
                    None => r.try_preserving_privacy(candidates).map_err(|e| s(&e))?,
                };
                let op = pick.outpoint();
                p.update_meta(|m| m.contributed = Some(op))
                    .map_err(|e| s(&e))?;
                reserved.insert(op);
                let r = r.contribute_inputs([pick]).map_err(|e| s(&e))?;
                r.commit_inputs().save(p).map_err(|e| s(&e))?;
                Ok(Step::Advanced)
            }
            ReceiveSession::WantsFeeRange(r) => {
                r.apply_fee_range(None, None).save(p).map_err(|e| s(&e))?;
                Ok(Step::Advanced)
            }
            ReceiveSession::ProvisionalProposal(r) => {
                let inner = self.inner.lock().await;
                let res = r
                    .finalize_proposal(|psbt| self.sign_receiver_inputs(&inner.wallet, psbt))
                    .save(p);
                drop(inner);
                res.map_err(|e| s(&e))?;
                Ok(Step::Advanced)
            }
            ReceiveSession::PayjoinProposal(r) => {
                let txid = r.psbt().unsigned_tx.compute_txid();
                p.update_meta(|m| m.payjoin_txid = Some(txid))
                    .map_err(|e| s(&e))?;
                let (req, ctx) = r.create_post_request(relay.as_str()).map_err(|e| s(&e))?;
                let body = self.post_via_relay(req).await?;
                r.process_response(&body, ctx).save(p).map_err(|e| s(&e))?;
                tracing::info!("payjoin: proposal {txid} sent to the sender");
                Ok(Step::Advanced)
            }
            ReceiveSession::Monitor(r) => {
                let mut found: HashMap<Txid, Transaction> = HashMap::new();
                let ids = p
                    .meta()
                    .payjoin_txid
                    .into_iter()
                    .chain(history.fallback_tx().map(|t| t.compute_txid()));
                for id in ids {
                    if let Some(tx) = self.client.get_tx(&id).await.map_err(|e| s(&e))? {
                        found.insert(id, tx);
                    }
                }
                match r
                    .check_for_transaction(|id| Ok(found.get(&id).cloned()))
                    .save(p)
                    .map_err(|e| s(&e))?
                {
                    OptionalTransitionOutcome::Progress(()) => {
                        tracing::info!("payjoin receive session settled");
                        p.remove();
                        Ok(Step::Done)
                    }
                    OptionalTransitionOutcome::Stasis(_) => Ok(Step::Wait),
                }
            }
            ReceiveSession::HasReplyableError(r) => {
                let sent = match r.create_error_request(relay.as_str()) {
                    Ok((req, ctx)) => match self.post_via_relay(req).await {
                        Ok(body) => Some(r.clone().process_error_response(&body, ctx).save(p)),
                        Err(_) => None,
                    },
                    Err(_) => None,
                };
                match sent {
                    Some(Ok(_)) => {}
                    _ => {
                        let _ = self.cancel_session(p).await;
                    }
                }
                Ok(Step::Advanced)
            }
            ReceiveSession::PendingFallback(r) => {
                self.broadcast_fallback(r.fallback_tx()).await?;
                r.close().save(p).map_err(|e| s(&e))?;
                p.remove();
                Ok(Step::Done)
            }
            ReceiveSession::Closed(_) => {
                p.remove();
                Ok(Step::Done)
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A fake OHTTP key config (any valid compressed pubkey works offline).
    pub(crate) fn fake_ohttp_keys() -> pdk::OhttpKeys {
        use bdk_wallet::bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
        let secp = Secp256k1::new();
        let sk = SecretKey::from_slice(&[7u8; 32]).unwrap();
        let pk = PublicKey::from_secret_key(&secp, &sk);
        let mut bytes = vec![1u8];
        bytes.extend_from_slice(&pk.serialize());
        pdk::OhttpKeys::try_from(&bytes[..]).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::fake_ohttp_keys;
    use super::*;
    use crate::{descriptors, OnchainConfig};
    use bdk_wallet::bitcoin::hashes::Hash;
    use bdk_wallet::bitcoin::{
        absolute, transaction, BlockHash, Network as BtcNetwork, Sequence, TxOut, Witness,
    };
    use bdk_wallet::chain::{BlockId, ConfirmationBlockTime, TxUpdate};
    use bdk_wallet::KeychainKind;
    use sessions_core::keystore::Seed;
    use sessions_core::types::*;
    use sessions_core::Rail;

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn open(dir: &Path, name: &str, phrase: Option<&str>, receive: bool) -> OnchainAccount {
        let seed = match phrase {
            Some(p) => Seed::from_phrase(p).unwrap(),
            None => Seed::generate(12).unwrap(),
        };
        let master = seed.master_xpriv(Network::Bitcoin).unwrap();
        OnchainAccount::open(OnchainConfig {
            id: name.into(),
            name: name.into(),
            network: Network::Bitcoin,
            db_path: dir.join(format!("{name}.db")),
            esplora_url: None,
            descriptors: descriptors::hot_single_sig(&master, Network::Bitcoin, 0).unwrap(),
            custody: Custody::Hot,
            signer: Some(master),
            payjoin_receive: receive,
            payjoin_dir: Some(dir.join(format!("{name}-payjoin"))),
        })
        .unwrap()
    }

    fn coin(spk: ScriptBuf, sats: u64, salt: u8) -> Transaction {
        Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: Txid::from_byte_array([salt; 32]),
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(sats),
                script_pubkey: spk,
            }],
        }
    }

    /// Unconfirmed coin at receive index 0 (enough for sending).
    async fn fund(a: &OnchainAccount, sats: u64, salt: u8) {
        let mut inner = a.inner.lock().await;
        let spk = inner
            .wallet
            .peek_address(KeychainKind::External, 0)
            .script_pubkey();
        inner
            .wallet
            .apply_unconfirmed_txs([(coin(spk, sats, salt), now())]);
    }

    /// Confirmed coin at receive index 0 (payjoin receivers only contribute these).
    async fn fund_confirmed(a: &OnchainAccount, sats: u64, salt: u8) -> OutPoint {
        let mut inner = a.inner.lock().await;
        let spk = inner
            .wallet
            .peek_address(KeychainKind::External, 0)
            .script_pubkey();
        let tx = coin(spk, sats, salt);
        let txid = tx.compute_txid();
        let block = BlockId {
            height: 100,
            hash: BlockHash::from_byte_array([salt; 32]),
        };
        let cp = inner.wallet.latest_checkpoint().insert(block);
        let mut tu = TxUpdate::default();
        tu.txs = vec![Arc::new(tx)];
        tu.anchors = [(
            ConfirmationBlockTime {
                block_id: block,
                confirmation_time: 1_700_000_000,
            },
            txid,
        )]
        .into();
        inner
            .wallet
            .apply_update(bdk_wallet::Update {
                chain: Some(cp),
                tx_update: tu,
                ..Default::default()
            })
            .unwrap();
        OutPoint { txid, vout: 0 }
    }

    fn pay(address: &str, sats: u64, uri: Option<String>) -> SendRequest {
        SendRequest {
            destination: PaymentRequest::BitcoinAddress {
                address: address.into(),
                amount_sat: Some(sats),
                label: None,
                payjoin: uri,
            },
            amount_sat: None,
            send_max: false,
            fee: FeePriority::Custom(3),
            note: None,
            style: Default::default(),
            coins: None,
        }
    }

    /// Sign + finalize the quote the way `send` does. Returns (finalized, unfinalized).
    async fn original_of(a: &OnchainAccount, q: &SendQuote) -> (Psbt, Psbt, Option<String>) {
        let (mut psbt, uri) = OnchainAccount::decode_handle(&q.handle).unwrap();
        a.sign_locally(&mut psbt).unwrap();
        let unfinalized = psbt.clone();
        assert!(a
            .inner
            .lock()
            .await
            .wallet
            .finalize_psbt(&mut psbt, SignOptions::default())
            .unwrap());
        (psbt, unfinalized, uri)
    }

    // --- transports -------------------------------------------------------

    struct Unreachable;

    #[async_trait]
    impl Http for Unreachable {
        async fn post(&self, _: &str, _: &str, _: Vec<u8>, _: Duration) -> Result<Vec<u8>, String> {
            Err("connection refused".into())
        }
    }

    /// Answers with a fixed body.
    struct Canned(Vec<u8>);

    #[async_trait]
    impl Http for Canned {
        async fn post(&self, _: &str, _: &str, _: Vec<u8>, _: Duration) -> Result<Vec<u8>, String> {
            Ok(self.0.clone())
        }
    }

    struct Hdrs(String);
    impl pdk::receive::v1::Headers for Hdrs {
        fn get_header(&self, key: &str) -> Option<&str> {
            match key {
                "content-type" => Some("text/plain"),
                "content-length" => Some(&self.0),
                _ => None,
            }
        }
    }

    /// An in-process BIP78 receiver driven by the library's v1 state machine.
    struct V1Receiver(Arc<OnchainAccount>, std::sync::Mutex<Option<OutPoint>>);

    #[async_trait]
    impl Http for V1Receiver {
        async fn post(
            &self,
            url: &str,
            content_type: &str,
            body: Vec<u8>,
            _: Duration,
        ) -> Result<Vec<u8>, String> {
            assert_eq!(content_type, "text/plain");
            let s = |e: &dyn std::fmt::Display| e.to_string();
            let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
            let unchecked = pdk::receive::v1::UncheckedOriginalPayload::from_request(
                &body,
                query,
                Hdrs(body.len().to_string()),
            )
            .map_err(|e| s(&e))?;
            let b = &self.0;
            let inner = b.inner.lock().await;
            let w = &inner.wallet;
            let p = unchecked
                .apply_broadcast_suitability(Some(FeeRate::BROADCAST_MIN), true)
                .map_err(|e| s(&e))?
                .check_inputs_not_owned(&mut |op| Ok(owns_outpoint(w, op)))
                .map_err(|e| s(&e))?
                .check_no_inputs_seen_before(&mut |_| Ok(false))
                .map_err(|e| s(&e))?
                .identify_receiver_outputs(&mut |spk| Ok(w.is_mine(spk.to_owned())))
                .map_err(|e| s(&e))?
                .commit_outputs();
            let pick = p
                .try_preserving_privacy(candidate_inputs(w, &BTreeSet::new()))
                .map_err(|e| s(&e))?;
            *self.1.lock().unwrap() = Some(pick.outpoint());
            let proposal = p
                .contribute_inputs([pick])
                .map_err(|e| s(&e))?
                .commit_inputs()
                .apply_fee_range(None, FeeRate::from_sat_per_vb(10))
                .map_err(|e| s(&e))?
                .finalize_proposal(|psbt| b.sign_receiver_inputs(w, psbt))
                .map_err(|e| s(&e))?;
            Ok(proposal.psbt().to_string().into_bytes())
        }
    }

    // --- tests ------------------------------------------------------------

    const V1_URI: &str = "bitcoin:bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq?amount=0.0004&pj=https://example.com/pj";

    #[test]
    fn detects_v1_and_v2_uris() {
        let net = BtcNetwork::Bitcoin;
        assert_eq!(pj_version(V1_URI, net), Some(PjVersion::V1));
        // A real v2 URI as a receiver produces it.
        let addr = Address::from_str("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq")
            .unwrap()
            .assume_checked();
        let r = ReceiverBuilder::new(addr, DEFAULT_DIRECTORY, fake_ohttp_keys())
            .unwrap()
            .with_amount(Amount::from_sat(40_000))
            .build()
            .save(&pdk::persist::InMemoryPersister::default())
            .unwrap();
        let v2 = r.pj_uri().to_string();
        assert!(v2.contains("pj="), "{v2}");
        assert_eq!(pj_version(&v2, net), Some(PjVersion::V2));
        // No pj, plain-http pj, or the wrong network: no payjoin.
        assert_eq!(
            pj_version("bitcoin:bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq", net),
            None
        );
        assert_eq!(
            pj_version(
                "bitcoin:bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq?pj=http://example.com/pj",
                net
            ),
            None
        );
        assert_eq!(pj_version(V1_URI, BtcNetwork::Testnet), None);

        // The quote handle carries the URI intact (v2 URIs contain '#' and '+').
        let psbt = Psbt::from_unsigned_tx(coin(ScriptBuf::new(), 1, 1)).unwrap_or_else(|_| {
            let mut tx = coin(ScriptBuf::new(), 1, 1);
            tx.input[0].script_sig = ScriptBuf::new();
            Psbt::from_unsigned_tx(tx).unwrap()
        });
        for uri in [V1_URI.to_string(), v2] {
            let h = crate::encode_handle(&psbt, Some(&uri));
            let (p, u) = OnchainAccount::decode_handle(&h).unwrap();
            assert_eq!(p, psbt);
            assert_eq!(u.as_deref(), Some(uri.as_str()));
        }
        let (_, none) = OnchainAccount::decode_handle(&crate::encode_handle(&psbt, None)).unwrap();
        assert_eq!(none, None);
    }

    #[tokio::test]
    async fn quote_only_offers_payjoin_for_efficient_hot_sends() {
        let dir = tempfile::tempdir().unwrap();
        let a = open(dir.path(), "a", Some(ABANDON), false);
        fund(&a, 100_000, 3).await;
        let dest = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
        let q = a
            .prepare_send(pay(dest, 40_000, Some(V1_URI.into())))
            .await
            .unwrap();
        assert!(q.payjoin);
        assert!(q.handle.ends_with(V1_URI));
        let mut max = pay(dest, 40_000, Some(V1_URI.into()));
        max.send_max = true;
        let q = a.prepare_send(max).await.unwrap();
        assert!(!q.payjoin && !q.handle.contains('\n'));
    }

    #[tokio::test]
    async fn v1_round_trip_between_two_wallets() {
        let dir = tempfile::tempdir().unwrap();
        let sender = open(dir.path(), "sender", Some(ABANDON), false);
        let receiver = Arc::new(open(dir.path(), "receiver", None, false));
        fund(&sender, 100_000, 11).await;
        let theirs = fund_confirmed(&receiver, 70_000, 12).await;

        let pay_to = receiver
            .inner
            .lock()
            .await
            .wallet
            .peek_address(KeychainKind::External, 5)
            .address;
        let uri = format!("bitcoin:{pay_to}?amount=0.0004&pj=https://example.com/pj");
        let q = sender
            .prepare_send(pay(&pay_to.to_string(), 40_000, Some(uri.clone())))
            .await
            .unwrap();
        assert!(q.payjoin);
        let (original, unfinalized, handle_uri) = original_of(&sender, &q).await;
        assert_eq!(handle_uri.as_deref(), Some(uri.as_str()));
        let original_tx = original.clone().extract_tx().unwrap();

        let http = V1Receiver(receiver.clone(), Default::default());
        let choice = sender
            .payjoin_or_original(&original, &unfinalized, &uri, &http)
            .await;
        assert!(choice.payjoin, "payjoin should have succeeded");
        let tx = choice.tx;
        assert_ne!(tx.compute_txid(), original_tx.compute_txid());

        // Inputs from both wallets.
        let contributed = http.1.lock().unwrap().expect("receiver contributed");
        assert_eq!(contributed, theirs);
        let ins: Vec<OutPoint> = tx.input.iter().map(|i| i.previous_output).collect();
        assert!(ins.contains(&theirs));
        for i in &original_tx.input {
            assert!(ins.contains(&i.previous_output));
        }
        assert_eq!(ins.len(), original_tx.input.len() + 1);
        assert!(tx.input.iter().all(|i| !i.witness.is_empty()));
        // Both the Original (the fallback) and the payjoin stay replaceable,
        // including the receiver's input.
        assert!(original_tx.input.iter().all(|i| i.sequence.is_rbf()));
        assert!(tx.input.iter().all(|i| i.sequence.is_rbf()));

        // The receiver's output grew by exactly the contributed coin (the
        // sender's fee contribution covered the extra input).
        let spk = pay_to.script_pubkey();
        let paid = |t: &Transaction| -> u64 {
            t.output
                .iter()
                .filter(|o| o.script_pubkey == spk)
                .map(|o| o.value.to_sat())
                .sum()
        };
        assert_eq!(paid(&original_tx), 40_000);
        assert_eq!(paid(&tx), 40_000 + 70_000);

        // The sender paid the amount, the quoted fee, and at most one input's
        // worth of extra fee at the original rate.
        let w = &sender.inner.lock().await.wallet;
        let change = |t: &Transaction| -> u64 {
            t.output
                .iter()
                .filter(|o| w.is_mine(o.script_pubkey.clone()))
                .map(|o| o.value.to_sat())
                .sum()
        };
        let spent_original = 100_000 - change(&original_tx);
        let spent_payjoin = 100_000 - change(&tx);
        assert_eq!(spent_original, 40_000 + q.fee_sat);
        let rate = psbt_fee_rate(&original).unwrap();
        let cap = max_fee_contribution(rate).to_sat();
        assert_eq!(spent_payjoin, spent_original + choice.extra_fee_sat);
        assert!(choice.extra_fee_sat > 0 && choice.extra_fee_sat <= cap);
        // And the payjoin still pays the requested 3 sat/vB. (Not compared
        // with the Original's exact measured rate: re-signing can make a DER
        // signature one byte longer.)
        let in_value = 100_000 + 70_000;
        let fee = in_value - tx.output.iter().map(|o| o.value.to_sat()).sum::<u64>();
        assert!(fee >= q.fee_sat + choice.extra_fee_sat);
        assert!(Amount::from_sat(fee) / tx.weight() >= FeeRate::from_sat_per_vb(3).unwrap());
    }

    #[tokio::test]
    async fn falls_back_to_the_original_on_any_failure() {
        let dir = tempfile::tempdir().unwrap();
        let sender = open(dir.path(), "sender", Some(ABANDON), false);
        fund(&sender, 100_000, 21).await;
        let dest = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
        let q = sender
            .prepare_send(pay(dest, 40_000, Some(V1_URI.into())))
            .await
            .unwrap();
        let (original, unfinalized, uri) = original_of(&sender, &q).await;
        let uri = uri.unwrap();
        let want = original.clone().extract_tx().unwrap();

        let check = |c: SendChoice| {
            assert!(!c.payjoin);
            assert_eq!(c.extra_fee_sat, 0);
            assert_eq!(c.tx, want);
        };
        // Endpoint down.
        check(
            sender
                .payjoin_or_original(&original, &unfinalized, &uri, &Unreachable)
                .await,
        );
        // A real HTTP client against a closed local port (no network needed).
        let closed = format!("bitcoin:{dest}?amount=0.0004&pj=https://127.0.0.1:1/pj");
        check(
            sender
                .payjoin_or_original(&original, &unfinalized, &closed, &ReqwestHttp::default())
                .await,
        );
        // Garbage, a JSON error, and an echo of the Original (no receiver input).
        for body in [
            b"not a psbt".to_vec(),
            br#"{"errorCode":"unavailable","message":"nope"}"#.to_vec(),
            original.to_string().into_bytes(),
            unfinalized.to_string().into_bytes(),
        ] {
            check(
                sender
                    .payjoin_or_original(&original, &unfinalized, &uri, &Canned(body))
                    .await,
            );
        }
        // Not a payjoin URI at all.
        check(
            sender
                .payjoin_or_original(&original, &unfinalized, "bitcoin:nonsense", &Unreachable)
                .await,
        );
    }

    #[tokio::test]
    async fn session_persists_and_replays() {
        let dir = tempfile::tempdir().unwrap();
        let addr = Address::from_str("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq")
            .unwrap()
            .assume_checked();
        let p = FilePersister::create(dir.path()).unwrap();
        let r = ReceiverBuilder::new(addr, DEFAULT_DIRECTORY, fake_ohttp_keys())
            .unwrap()
            .with_amount(Amount::from_sat(12_345))
            .build()
            .save(&p)
            .unwrap();
        let uri = r.pj_uri().to_string();
        let op = OutPoint {
            txid: Txid::from_byte_array([9; 32]),
            vout: 1,
        };
        p.update_meta(|m| {
            m.contributed = Some(op);
            m.original_at = Some(42);
        })
        .unwrap();
        let path = p.path.clone();
        drop(p);

        // Only the session file, no leftover temp file.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        assert_eq!(session_files(dir.path()), vec![path.clone()]);

        let p = FilePersister::open(&path).unwrap();
        let meta = p.meta();
        assert_eq!(meta.contributed, Some(op));
        assert_eq!(meta.original_at, Some(42));
        assert!(!p.is_closed());
        let (session, history) = pdk::receive::v2::replay_event_log(&p).unwrap();
        assert!(matches!(session, ReceiveSession::Initialized(_)));
        assert_eq!(history.pj_uri().to_string(), uri);
        assert!(history.fallback_tx().is_none());

        // Closing is persisted too.
        p.close().unwrap();
        assert!(FilePersister::open(&path).unwrap().is_closed());

        // Seen-input records survive a reload and are per-session.
        let mut seen = SeenInputs::load(dir.path());
        seen.0.insert(op.to_string(), ("pj-a".into(), now()));
        seen.save(dir.path()).unwrap();
        let seen = SeenInputs::load(dir.path());
        assert!(seen.seen_elsewhere(&op, "pj-b"));
        assert!(!seen.seen_elsewhere(&op, "pj-a"));
    }

    #[tokio::test]
    async fn receive_without_confirmed_coins_gives_a_plain_request() {
        let dir = tempfile::tempdir().unwrap();
        let a = open(dir.path(), "r", Some(ABANDON), true);
        fund(&a, 50_000, 31).await; // unconfirmed: not contributable
        let t = a
            .receive(ReceiveRequest {
                method: Method::BitcoinAddress,
                amount_sat: Some(1_000),
                description: None,
            })
            .await
            .unwrap();
        assert!(!t.request.contains("pj="), "{}", t.request);
        assert!(t.request.starts_with("bitcoin:"));
        // Nothing to drive, and no sessions were created.
        a.payjoin_tick().await.unwrap();
        assert!(session_files(&dir.path().join("r-payjoin")).is_empty());

        // Non-hot accounts never receive with payjoin.
        let st = PayjoinState::new(&Custody::WatchOnly, true, Some(dir.path().into()));
        assert!(st.receive_dir.is_none());
    }

    #[test]
    fn receiver_signs_only_its_own_input() {
        // Covered end to end by v1_round_trip_between_two_wallets; here we make
        // sure a PSBT with no receiver input is refused.
        let dir = tempfile::tempdir().unwrap();
        let a = open(dir.path(), "x", Some(ABANDON), false);
        let psbt = Psbt::from_unsigned_tx(coin(ScriptBuf::new(), 1, 1)).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let inner = a.inner.lock().await;
            assert!(a.sign_receiver_inputs(&inner.wallet, &psbt).is_err());
        });
    }
}

//! Chain lookups by address and transaction over Electrum, for code outside
//! the on-chain accounts (the Spending account's deposit watcher).
//!
//! [`ElectrumChain`] answers the questions an Esplora REST API answers
//! directly (an address's unspent outputs, a transaction's confirmation
//! status, who spent an output, fee estimates) from Electrum's script-hash
//! calls. It runs on the app's shared Electrum [`Pool`] (one connection,
//! server rotation, per-call retries), never on async workers.
//!
//! Electrum has no "status of txid" call that every server supports, so a
//! transaction's height comes from the history of one of its scripts. Scripts
//! of addresses asked about (and [`ElectrumChain::prefer_address`]) are tried
//! first: they are the wallet's own, with short histories, unlike an
//! exchange's change address on the same transaction.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use bdk_electrum::electrum_client::ElectrumApi;
use bdk_wallet::bitcoin::address::NetworkUnchecked;
use bdk_wallet::bitcoin::block::Header;
use bdk_wallet::bitcoin::consensus::encode::{deserialize, deserialize_hex, serialize_hex};
use bdk_wallet::bitcoin::{Address, Network, OutPoint, Script, ScriptBuf, Transaction, Txid};

use crate::electrum::{self, Pool, ServerUrl};

/// A failed lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// Not a valid address for this network.
    InvalidAddress(String),
    /// The server answered that it doesn't know the transaction or output.
    NotFound(String),
    /// No server could be reached, or the connection dropped (worth retrying).
    Connectivity(String),
    /// The server answered with some other error, or an answer made no sense.
    Other(String),
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryError::InvalidAddress(m) => write!(f, "invalid address: {m}"),
            QueryError::NotFound(m) => write!(f, "not found: {m}"),
            QueryError::Connectivity(m) => write!(f, "can't reach the chain server: {m}"),
            QueryError::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for QueryError {}

/// Whether a server's error message means "no such transaction" (bitcoind's
/// "No such mempool or blockchain transaction", passed on by Fulcrum and
/// ElectrumX; electrs' "… not found").
pub fn is_not_found_message(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("not found") || m.contains("no such") || m.contains("missing transaction")
}

impl QueryError {
    /// From an Electrum client error: transport problems are connectivity,
    /// the server's own answers are told apart by message.
    pub fn from_electrum(e: &electrum::electrum_client::Error) -> Self {
        let c = crate::chain::from_electrum(e);
        if c.transient {
            QueryError::Connectivity(c.message)
        } else if is_not_found_message(&c.message) {
            QueryError::NotFound(c.message)
        } else {
            QueryError::Other(c.message)
        }
    }
}

type QResult<T> = std::result::Result<T, QueryError>;

/// An unspent output as `blockchain.scripthash.listunspent` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unspent {
    pub txid: Txid,
    pub vout: u32,
    pub value: u64,
    /// Block height, 0 while unconfirmed.
    pub height: u32,
}

/// The Electrum calls [`ElectrumChain`] needs. Blocking. The app's [`Pool`]
/// implements it; tests use a fake.
pub trait ElectrumSource: Send + Sync {
    /// `blockchain.scripthash.get_history`: (txid, height); height is 0 or
    /// -1 while unconfirmed.
    fn history(&self, script: &Script) -> QResult<Vec<(Txid, i32)>>;
    /// `blockchain.scripthash.listunspent`.
    fn unspent(&self, script: &Script) -> QResult<Vec<Unspent>>;
    /// `blockchain.transaction.get` (cached).
    fn transaction(&self, txid: Txid) -> QResult<Arc<Transaction>>;
    /// Several transactions (one round trip where the server allows).
    fn transactions(&self, txids: &[Txid]) -> QResult<Vec<Arc<Transaction>>> {
        txids.iter().map(|t| self.transaction(*t)).collect()
    }
    /// The timestamp of the block at `height`.
    fn header_time(&self, height: u32) -> QResult<u32>;
    /// Several block timestamps (one round trip where the server allows).
    fn header_times(&self, heights: &[u32]) -> QResult<Vec<u32>> {
        heights.iter().map(|h| self.header_time(*h)).collect()
    }
    /// The chain tip's height.
    fn tip(&self) -> QResult<u32>;
    /// `blockchain.estimatefee` for each target, in BTC/kvB (-1: unknown).
    fn estimate_fees(&self, targets: &[usize]) -> QResult<Vec<f64>>;
    /// `blockchain.relayfee`, in BTC/kvB.
    fn relay_fee(&self) -> QResult<f64>;
    fn broadcast(&self, tx: &Transaction) -> QResult<Txid>;
}

/// Requests per batch.
const BATCH: usize = 50;

impl ElectrumSource for Pool {
    fn history(&self, script: &Script) -> QResult<Vec<(Txid, i32)>> {
        Ok(self
            .conn()
            .script_get_history(script)
            .map_err(|e| QueryError::from_electrum(&e))?
            .into_iter()
            .map(|h| (h.tx_hash, h.height))
            .collect())
    }

    fn unspent(&self, script: &Script) -> QResult<Vec<Unspent>> {
        self.conn()
            .script_list_unspent(script)
            .map_err(|e| QueryError::from_electrum(&e))?
            .into_iter()
            .map(|u| {
                Ok(Unspent {
                    txid: u.tx_hash,
                    vout: u32::try_from(u.tx_pos)
                        .map_err(|_| QueryError::Other("output index out of range".into()))?,
                    value: u.value,
                    height: u32::try_from(u.height).unwrap_or(0),
                })
            })
            .collect()
    }

    fn transaction(&self, txid: Txid) -> QResult<Arc<Transaction>> {
        self.client
            .fetch_tx(txid)
            .map_err(|e| QueryError::from_electrum(&e))
    }

    fn transactions(&self, txids: &[Txid]) -> QResult<Vec<Arc<Transaction>>> {
        if txids.len() < 2 {
            return txids.iter().map(|t| self.transaction(*t)).collect();
        }
        let mut raws = Vec::with_capacity(txids.len());
        for chunk in txids.chunks(BATCH) {
            raws.extend(
                self.conn()
                    .batch_transaction_get_raw(chunk.iter())
                    .map_err(|e| QueryError::from_electrum(&e))?,
            );
        }
        let txs = raws
            .iter()
            .map(|raw| {
                deserialize::<Transaction>(raw)
                    .map(Arc::new)
                    .map_err(|e| QueryError::Other(format!("invalid transaction: {e}")))
            })
            .collect::<QResult<Vec<_>>>()?;
        if txs.len() != txids.len() || txs.iter().zip(txids).any(|(t, id)| t.compute_txid() != *id)
        {
            return Err(QueryError::Other(
                "the server sent different transactions than asked for".into(),
            ));
        }
        self.client.populate_tx_cache(txs.iter().cloned());
        Ok(txs)
    }

    fn header_time(&self, height: u32) -> QResult<u32> {
        Ok(self
            .conn()
            .block_header(height as usize)
            .map_err(|e| QueryError::from_electrum(&e))?
            .time)
    }

    fn header_times(&self, heights: &[u32]) -> QResult<Vec<u32>> {
        let mut raws = Vec::with_capacity(heights.len());
        for chunk in heights.chunks(BATCH) {
            raws.extend(
                self.conn()
                    .batch_block_header_raw(chunk.iter().copied())
                    .map_err(|e| QueryError::from_electrum(&e))?,
            );
        }
        raws.iter()
            .map(|raw| {
                deserialize::<Header>(raw)
                    .map(|h| h.time)
                    .map_err(|e| QueryError::Other(format!("invalid block header: {e}")))
            })
            .collect()
    }

    fn tip(&self) -> QResult<u32> {
        let h = self
            .conn()
            .block_headers_subscribe()
            .map_err(|e| QueryError::from_electrum(&e))?
            .height;
        u32::try_from(h).map_err(|_| QueryError::Other(format!("odd tip height {h}")))
    }

    fn estimate_fees(&self, targets: &[usize]) -> QResult<Vec<f64>> {
        self.conn()
            .batch_estimate_fee(targets.iter().copied())
            .map_err(|e| QueryError::from_electrum(&e))
    }

    fn relay_fee(&self) -> QResult<f64> {
        self.conn()
            .relay_fee()
            .map_err(|e| QueryError::from_electrum(&e))
    }

    fn broadcast(&self, tx: &Transaction) -> QResult<Txid> {
        let txid = self
            .client
            .transaction_broadcast(tx)
            .map_err(|e| QueryError::from_electrum(&e))?;
        // Don't fetch it back later.
        self.client.populate_tx_cache([tx.clone()]);
        Ok(txid)
    }
}

/// Where a transaction is: `height` is `None` while unconfirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxState {
    pub height: Option<u32>,
    /// Block timestamp (when confirmed and the header could be read).
    pub time: Option<u32>,
}

impl TxState {
    pub const UNCONFIRMED: TxState = TxState {
        height: None,
        time: None,
    };

    pub fn confirmed(&self) -> bool {
        self.height.is_some()
    }
}

/// An output paying to an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressOutput {
    pub txid: Txid,
    pub vout: u32,
    pub value: u64,
    pub state: TxState,
}

/// Whether an output has been spent, and by what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spend {
    Unspent,
    Spent {
        txid: Txid,
        /// The spending input's index.
        vin: u32,
        state: TxState,
    },
}

/// Fee rates in sat/vB, mempool.space's `fees/recommended` shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeRates {
    /// Next block.
    pub fastest: u64,
    /// ~3 blocks.
    pub half_hour: u64,
    /// ~6 blocks.
    pub hour: u64,
    /// ~25 blocks.
    pub economy: u64,
    /// The floor: relay fee, or the longest estimate if higher.
    pub minimum: u64,
}

/// Confirmation targets asked for, matching the fields of [`FeeRates`]
/// (the same targets Breez reads from an Esplora `fee-estimates`).
pub const FEE_RATE_TARGETS: [usize; 5] = [1, 3, 6, 25, 1008];

fn btc_per_kvb_to_sat_per_vb(r: f64) -> Option<f64> {
    Some(r * 100_000.0).filter(|r| r.is_finite() && *r > 0.0)
}

/// Electrum's estimates (BTC/kvB per [`FEE_RATE_TARGETS`], -1 when unknown)
/// and relay fee as [`FeeRates`]. Unknown targets borrow the next longer
/// known one; sooner never costs less than later; nothing goes below the
/// relay fee (or 1 sat/vB). With no estimates at all, everything is the floor.
pub fn fee_rates_from(btc_per_kvb: &[f64], relay_btc_per_kvb: Option<f64>) -> FeeRates {
    let floor = relay_btc_per_kvb
        .and_then(btc_per_kvb_to_sat_per_vb)
        .map_or(1, |r| r.ceil() as u64)
        .max(1);
    let mut out = [floor; FEE_RATE_TARGETS.len()];
    let mut later: Option<u64> = None;
    for i in (0..FEE_RATE_TARGETS.len()).rev() {
        let here = btc_per_kvb
            .get(i)
            .copied()
            .and_then(btc_per_kvb_to_sat_per_vb)
            .map(|r| (r.ceil() as u64).max(floor));
        let r = match (here, later) {
            (Some(h), Some(l)) => h.max(l),
            (Some(h), None) => h,
            (None, Some(l)) => l,
            (None, None) => floor,
        };
        out[i] = r;
        later = Some(r);
    }
    FeeRates {
        fastest: out[0],
        half_hour: out[1],
        hour: out[2],
        economy: out[3],
        minimum: out[4],
    }
}

struct Inner {
    source: Arc<dyn ElectrumSource>,
    network: Network,
    /// Block height → timestamp.
    times: Mutex<HashMap<u32, u32>>,
    /// Scripts to look a transaction's height up by first.
    preferred: Mutex<HashSet<ScriptBuf>>,
}

/// Address and transaction lookups over Electrum. Cheap to clone.
#[derive(Clone)]
pub struct ElectrumChain {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ElectrumChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ElectrumChain")
            .field("network", &self.inner.network)
            .finish_non_exhaustive()
    }
}

impl ElectrumChain {
    /// On the app's shared connection to `servers` (the same one the
    /// on-chain accounts use).
    pub fn for_servers(
        servers: &[ServerUrl],
        network: sessions_core::Network,
    ) -> sessions_core::Result<Self> {
        let pool = electrum::pool_for(servers)?;
        Ok(Self::new(pool, network))
    }

    pub fn new(source: Arc<dyn ElectrumSource>, network: sessions_core::Network) -> Self {
        ElectrumChain {
            inner: Arc::new(Inner {
                source,
                network: network.to_bitcoin(),
                times: Mutex::new(HashMap::new()),
                preferred: Mutex::new(HashSet::new()),
            }),
        }
    }

    /// Look transactions touching `address` up by its history first.
    pub fn prefer_address(&self, address: &str) -> QResult<()> {
        let script = self.inner.script(address)?;
        self.inner.prefer(script);
        Ok(())
    }

    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Inner) -> QResult<T> + Send + 'static,
    ) -> QResult<T> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || f(&inner))
            .await
            .map_err(|e| QueryError::Other(e.to_string()))?
    }

    /// Unspent outputs paying to `address` (confirmed and in the mempool).
    pub async fn address_utxos(&self, address: &str) -> QResult<Vec<AddressOutput>> {
        let address = address.to_string();
        self.run(move |i| i.address_utxos(&address)).await
    }

    /// Every output ever paid to `address`, spent or not.
    pub async fn address_txos(&self, address: &str) -> QResult<Vec<AddressOutput>> {
        let address = address.to_string();
        self.run(move |i| i.address_txos(&address)).await
    }

    pub async fn tx_state(&self, txid: Txid) -> QResult<TxState> {
        self.run(move |i| i.tx_state(txid)).await
    }

    pub async fn tip_height(&self) -> QResult<u32> {
        self.run(|i| i.source.tip()).await
    }

    /// The raw transaction, as hex.
    pub async fn tx_hex(&self, txid: Txid) -> QResult<String> {
        self.run(move |i| Ok(serialize_hex(&*i.source.transaction(txid)?)))
            .await
    }

    pub async fn outspend(&self, txid: Txid, vout: u32) -> QResult<Spend> {
        self.run(move |i| i.outspend(txid, vout)).await
    }

    /// Broadcast a raw transaction given as hex.
    pub async fn broadcast_hex(&self, hex: &str) -> QResult<Txid> {
        let tx: Transaction = deserialize_hex(hex.trim())
            .map_err(|e| QueryError::Other(format!("invalid transaction: {e}")))?;
        self.run(move |i| i.source.broadcast(&tx)).await
    }

    pub async fn fee_rates(&self) -> QResult<FeeRates> {
        self.run(|i| {
            let est = i.source.estimate_fees(&FEE_RATE_TARGETS)?;
            // Only a floor: estimates without it are still good.
            let relay = i.source.relay_fee().ok();
            Ok(fee_rates_from(&est, relay))
        })
        .await
    }
}

impl Inner {
    fn script(&self, address: &str) -> QResult<ScriptBuf> {
        let a = Address::<NetworkUnchecked>::from_str(address.trim())
            .map_err(|e| QueryError::InvalidAddress(e.to_string()))?
            .require_network(self.network)
            .map_err(|e| QueryError::InvalidAddress(e.to_string()))?;
        Ok(a.script_pubkey())
    }

    fn prefer(&self, script: ScriptBuf) {
        let mut p = self.preferred.lock().unwrap_or_else(|e| e.into_inner());
        if p.len() > 1_000 {
            p.clear();
        }
        p.insert(script);
    }

    fn is_preferred(&self, script: &Script) -> bool {
        self.preferred
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(script)
    }

    /// The block time at `height`, cached; `None` if the header can't be read
    /// (the time is a nicety, never a reason to fail a lookup).
    fn time_at(&self, height: u32) -> Option<u32> {
        if let Some(t) = self
            .times
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&height)
        {
            return Some(*t);
        }
        match self.source.header_time(height) {
            Ok(t) => {
                let mut times = self.times.lock().unwrap_or_else(|e| e.into_inner());
                if times.len() > 10_000 {
                    times.clear();
                }
                times.insert(height, t);
                Some(t)
            }
            Err(e) => {
                tracing::debug!("block time at {height}: {e}");
                None
            }
        }
    }

    /// Read the block times of `heights` not cached yet, in one go.
    fn prefetch_times(&self, heights: impl IntoIterator<Item = i64>) {
        let missing: Vec<u32> = {
            let times = self.times.lock().unwrap_or_else(|e| e.into_inner());
            let mut v: Vec<u32> = heights
                .into_iter()
                .filter_map(|h| u32::try_from(h).ok().filter(|h| *h > 0))
                .filter(|h| !times.contains_key(h))
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        if missing.len() < 2 {
            return; // `time_at` reads a single one
        }
        match self.source.header_times(&missing) {
            Ok(ts) if ts.len() == missing.len() => {
                let mut times = self.times.lock().unwrap_or_else(|e| e.into_inner());
                if times.len() > 10_000 {
                    times.clear();
                }
                times.extend(missing.into_iter().zip(ts));
            }
            Ok(_) => {}
            Err(e) => tracing::debug!("block times: {e}"),
        }
    }

    /// The state for an Electrum history height (> 0: confirmed there).
    fn state_at(&self, height: i64) -> TxState {
        match u32::try_from(height) {
            Ok(h) if h > 0 => TxState {
                height: Some(h),
                time: self.time_at(h),
            },
            _ => TxState::UNCONFIRMED,
        }
    }

    fn address_utxos(&self, address: &str) -> QResult<Vec<AddressOutput>> {
        let script = self.script(address)?;
        self.prefer(script.clone());
        let unspent = self.source.unspent(&script)?;
        self.prefetch_times(unspent.iter().map(|u| i64::from(u.height)));
        Ok(unspent
            .into_iter()
            .map(|u| AddressOutput {
                txid: u.txid,
                vout: u.vout,
                value: u.value,
                state: self.state_at(i64::from(u.height)),
            })
            .collect())
    }

    fn address_txos(&self, address: &str) -> QResult<Vec<AddressOutput>> {
        let script = self.script(address)?;
        self.prefer(script.clone());
        let mut seen = HashSet::new();
        let history: Vec<(Txid, i32)> = self
            .source
            .history(&script)?
            .into_iter()
            .filter(|(t, _)| seen.insert(*t))
            .collect();
        let txids: Vec<Txid> = history.iter().map(|(t, _)| *t).collect();
        let txs = self.source.transactions(&txids)?;
        self.prefetch_times(history.iter().map(|(_, h)| i64::from(*h)));
        let mut out = Vec::new();
        for ((txid, height), tx) in history.into_iter().zip(txs) {
            let paying: Vec<(u32, u64)> = tx
                .output
                .iter()
                .enumerate()
                .filter(|(_, o)| o.script_pubkey == script)
                .filter_map(|(vout, o)| Some((u32::try_from(vout).ok()?, o.value.to_sat())))
                .collect();
            if paying.is_empty() {
                continue; // only spends from it
            }
            let state = self.state_at(i64::from(height));
            out.extend(paying.into_iter().map(|(vout, value)| AddressOutput {
                txid,
                vout,
                value,
                state,
            }));
        }
        Ok(out)
    }

    /// The script whose history tells where `tx` is: one of ours it pays
    /// to, else one of ours it spends from (a refund), else its first
    /// spendable output, else what its first input spent.
    fn status_script(&self, tx: &Transaction) -> QResult<Option<ScriptBuf>> {
        if let Some(o) = tx
            .output
            .iter()
            .find(|o| self.is_preferred(&o.script_pubkey))
        {
            return Ok(Some(o.script_pubkey.clone()));
        }
        let prevout_script = |i: &bdk_wallet::bitcoin::TxIn| -> QResult<Option<ScriptBuf>> {
            let prev = self.source.transaction(i.previous_output.txid)?;
            Ok(prev
                .output
                .get(i.previous_output.vout as usize)
                .map(|o| o.script_pubkey.clone()))
        };
        if !tx.is_coinbase() && tx.input.len() <= 2 {
            for i in &tx.input {
                // Only a shortcut: a failed lookup falls through to the outputs.
                let prev = prevout_script(i).ok().flatten();
                if let Some(s) = prev.filter(|s| self.is_preferred(s)) {
                    return Ok(Some(s));
                }
            }
        }
        if let Some(o) = tx.output.iter().find(|o| !o.script_pubkey.is_op_return()) {
            return Ok(Some(o.script_pubkey.clone()));
        }
        match tx.input.first() {
            Some(i) if !tx.is_coinbase() => prevout_script(i),
            _ => Ok(None),
        }
    }

    fn tx_state(&self, txid: Txid) -> QResult<TxState> {
        let tx = self.source.transaction(txid)?;
        let Some(script) = self.status_script(&tx)? else {
            return Ok(TxState::UNCONFIRMED);
        };
        // The server has the transaction, so it's in this history unless it
        // left the mempool in between: unconfirmed either way.
        Ok(self
            .source
            .history(&script)?
            .into_iter()
            .find(|(t, _)| *t == txid)
            .map_or(TxState::UNCONFIRMED, |(_, h)| self.state_at(i64::from(h))))
    }

    fn outspend(&self, txid: Txid, vout: u32) -> QResult<Spend> {
        let tx = self.source.transaction(txid)?;
        let out = tx
            .output
            .get(vout as usize)
            .ok_or_else(|| QueryError::NotFound(format!("{txid} has no output {vout}")))?;
        let outpoint = OutPoint { txid, vout };
        let others: Vec<(Txid, i32)> = self
            .source
            .history(&out.script_pubkey)?
            .into_iter()
            .filter(|(t, _)| *t != txid)
            .collect();
        let ids: Vec<Txid> = others.iter().map(|(t, _)| *t).collect();
        let spenders = self.source.transactions(&ids)?;
        for ((t, height), spender) in others.into_iter().zip(spenders) {
            if let Some(vin) = spender
                .input
                .iter()
                .position(|i| i.previous_output == outpoint)
            {
                return Ok(Spend::Spent {
                    txid: t,
                    vin: u32::try_from(vin).unwrap_or(u32::MAX),
                    state: self.state_at(i64::from(height)),
                });
            }
        }
        Ok(Spend::Unspent)
    }
}

/// A fake [`ElectrumSource`] for tests (here and in other crates): a few
/// transactions at given heights, histories derived from them.
pub mod fake {
    use super::*;

    #[derive(Default)]
    pub struct FakeSource {
        /// txid → (transaction, height: > 0 confirmed, 0 mempool).
        pub txs: Mutex<HashMap<Txid, (Arc<Transaction>, i32)>>,
        pub tip: u32,
        /// BTC/kvB per target asked for (by target), -1 when missing.
        pub fees: HashMap<usize, f64>,
        pub relay: Option<f64>,
        pub broadcasts: Mutex<Vec<Txid>>,
        /// `history` calls, by script.
        pub history_calls: Mutex<Vec<ScriptBuf>>,
        /// Every call fails as if the server were unreachable.
        pub offline: bool,
    }

    impl FakeSource {
        pub fn add(&self, tx: Transaction, height: i32) -> Txid {
            let txid = tx.compute_txid();
            self.txs
                .lock()
                .unwrap()
                .insert(txid, (Arc::new(tx), height));
            txid
        }

        fn up(&self) -> QResult<()> {
            if self.offline {
                Err(QueryError::Connectivity("timed out".into()))
            } else {
                Ok(())
            }
        }

        /// Transactions touching `script` (paying to it, or spending an
        /// output that paid to it), in height order like Electrum.
        fn touching(&self, script: &Script) -> Vec<(Txid, i32)> {
            let txs = self.txs.lock().unwrap();
            let pays = |t: &Transaction| t.output.iter().any(|o| o.script_pubkey == *script);
            let spends = |t: &Transaction| {
                t.input.iter().any(|i| {
                    txs.get(&i.previous_output.txid).is_some_and(|(p, _)| {
                        p.output
                            .get(i.previous_output.vout as usize)
                            .is_some_and(|o| o.script_pubkey == *script)
                    })
                })
            };
            let mut v: Vec<(Txid, i32)> = txs
                .iter()
                .filter(|(_, (t, _))| pays(t) || spends(t))
                .map(|(id, (_, h))| (*id, *h))
                .collect();
            v.sort_by_key(|(id, h)| (if *h <= 0 { i32::MAX } else { *h }, *id));
            v
        }
    }

    impl ElectrumSource for FakeSource {
        fn history(&self, script: &Script) -> QResult<Vec<(Txid, i32)>> {
            self.up()?;
            self.history_calls.lock().unwrap().push(script.to_owned());
            Ok(self.touching(script))
        }

        fn unspent(&self, script: &Script) -> QResult<Vec<Unspent>> {
            self.up()?;
            let all = self.touching(script);
            let txs = self.txs.lock().unwrap();
            let spent: HashSet<OutPoint> = txs
                .values()
                .flat_map(|(t, _)| t.input.iter().map(|i| i.previous_output))
                .collect();
            let mut out = Vec::new();
            for (txid, h) in all {
                let (tx, _) = &txs[&txid];
                for (vout, o) in tx.output.iter().enumerate() {
                    let vout = vout as u32;
                    if o.script_pubkey == *script && !spent.contains(&OutPoint { txid, vout }) {
                        out.push(Unspent {
                            txid,
                            vout,
                            value: o.value.to_sat(),
                            height: u32::try_from(h).unwrap_or(0),
                        });
                    }
                }
            }
            Ok(out)
        }

        fn transaction(&self, txid: Txid) -> QResult<Arc<Transaction>> {
            self.up()?;
            self.txs
                .lock()
                .unwrap()
                .get(&txid)
                .map(|(t, _)| t.clone())
                .ok_or_else(|| {
                    QueryError::NotFound("No such mempool or blockchain transaction".to_string())
                })
        }

        fn header_time(&self, height: u32) -> QResult<u32> {
            self.up()?;
            Ok(1_700_000_000 + height * 600)
        }

        fn tip(&self) -> QResult<u32> {
            self.up()?;
            Ok(self.tip)
        }

        fn estimate_fees(&self, targets: &[usize]) -> QResult<Vec<f64>> {
            self.up()?;
            Ok(targets
                .iter()
                .map(|t| self.fees.get(t).copied().unwrap_or(-1.0))
                .collect())
        }

        fn relay_fee(&self) -> QResult<f64> {
            self.up()?;
            self.relay
                .ok_or_else(|| QueryError::Other("unknown method".into()))
        }

        fn broadcast(&self, tx: &Transaction) -> QResult<Txid> {
            self.up()?;
            let txid = tx.compute_txid();
            if self.txs.lock().unwrap().contains_key(&txid) {
                return Err(QueryError::Other(
                    "the transaction was rejected by network rules.\n\ntxn-already-known".into(),
                ));
            }
            self.broadcasts.lock().unwrap().push(txid);
            self.add(tx.clone(), 0);
            Ok(txid)
        }
    }

    /// A transaction spending `inputs` and paying `outputs` (script, sats).
    pub fn tx(inputs: &[OutPoint], outputs: &[(&ScriptBuf, u64)]) -> Transaction {
        use bdk_wallet::bitcoin::{absolute, transaction, Amount, Sequence, TxIn, TxOut, Witness};
        Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: inputs
                .iter()
                .map(|o| TxIn {
                    previous_output: *o,
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::new(),
                })
                .collect(),
            output: outputs
                .iter()
                .map(|(s, v)| TxOut {
                    value: Amount::from_sat(*v),
                    script_pubkey: (*s).clone(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{tx, FakeSource};
    use super::*;

    // Mainnet addresses (any valid ones will do).
    const DEPOSIT: &str = "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu";
    const EXCHANGE: &str = "bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g";
    const REFUND_TO: &str = "bc1qp59yckz4ae5c4efgw2s5wfyvrz0ala7rgvuz8z";

    fn spk(a: &str) -> ScriptBuf {
        Address::from_str(a)
            .unwrap()
            .assume_checked()
            .script_pubkey()
    }

    fn coinbase_like(n: u8) -> OutPoint {
        OutPoint {
            txid: Txid::from_str(&format!("{n:02x}").repeat(32)).unwrap(),
            vout: 0,
        }
    }

    #[test]
    fn fee_rates_convert_and_stay_ordered() {
        // BTC/kvB for 1, 3, 6, 25, 1008 blocks.
        let r = fee_rates_from(&[0.0002, 0.00015, -1.0, 0.00005, 0.00001], Some(0.00001));
        assert_eq!(
            r,
            FeeRates {
                fastest: 20,
                half_hour: 15,
                hour: 5, // unknown: borrows from 25 blocks
                economy: 5,
                minimum: 1,
            }
        );
        // Fractions round up; sooner never cheaper than later.
        let r = fee_rates_from(&[0.000011, 0.00003, 0.000021, 0.000021, 0.000021], None);
        assert_eq!(
            (r.fastest, r.half_hour, r.hour, r.economy, r.minimum),
            (3, 3, 3, 3, 3)
        );
        // A relay fee above the estimates is the floor.
        let r = fee_rates_from(
            &[0.00001, 0.00001, 0.00001, 0.00001, 0.00001],
            Some(0.00002),
        );
        assert_eq!(r.fastest, 2);
        assert_eq!(r.minimum, 2);
        // Nothing known: everything at the floor.
        let r = fee_rates_from(&[-1.0; 5], None);
        assert_eq!(r.fastest, 1);
        assert_eq!(fee_rates_from(&[], Some(0.00003)).economy, 3);
    }

    #[test]
    fn not_found_messages() {
        assert!(is_not_found_message(
            "No such mempool or blockchain transaction. Use gettransaction for wallet transactions."
        ));
        assert!(is_not_found_message("Transaction not found"));
        assert!(!is_not_found_message("excessive resource usage"));
    }

    fn chain(src: &Arc<FakeSource>) -> ElectrumChain {
        ElectrumChain::new(src.clone(), sessions_core::Network::Bitcoin)
    }

    #[tokio::test]
    async fn address_outputs_status_and_outspend() {
        let src = Arc::new(FakeSource {
            tip: 900_010,
            ..Default::default()
        });
        let (deposit, exchange, refund_to) = (spk(DEPOSIT), spk(EXCHANGE), spk(REFUND_TO));
        // An exchange pays the deposit address (with change to itself), confirmed.
        let funding = src.add(
            tx(
                &[coinbase_like(1)],
                &[(&exchange, 5_000_000), (&deposit, 50_000)],
            ),
            900_001,
        );
        // Unrelated activity on the exchange's address.
        for n in 2..6 {
            src.add(tx(&[coinbase_like(n)], &[(&exchange, 1_000)]), 900_002);
        }
        // A second deposit, still in the mempool.
        let pending = src.add(tx(&[coinbase_like(9)], &[(&deposit, 7_000)]), 0);
        let c = chain(&src);

        let mut utxos = c.address_utxos(DEPOSIT).await.unwrap();
        utxos.sort_by_key(|u| u.value);
        assert_eq!(utxos.len(), 2);
        assert_eq!(
            utxos[0],
            AddressOutput {
                txid: pending,
                vout: 0,
                value: 7_000,
                state: TxState::UNCONFIRMED
            }
        );
        assert_eq!(utxos[1].txid, funding);
        assert_eq!(utxos[1].vout, 1);
        assert_eq!(utxos[1].state.height, Some(900_001));
        assert_eq!(utxos[1].state.time, Some(1_700_000_000 + 900_001 * 600));

        // Status: looked up by the deposit address's history (asked about
        // above), not the exchange's.
        src.history_calls.lock().unwrap().clear();
        let st = c.tx_state(funding).await.unwrap();
        assert_eq!(st.height, Some(900_001));
        assert_eq!(*src.history_calls.lock().unwrap(), vec![deposit.clone()]);
        assert_eq!(c.tx_state(pending).await.unwrap(), TxState::UNCONFIRMED);
        let unknown = Txid::from_str(&"ab".repeat(32)).unwrap();
        assert!(matches!(
            c.tx_state(unknown).await,
            Err(QueryError::NotFound(_))
        ));
        assert_eq!(c.tip_height().await.unwrap(), 900_010);

        // Unspent until a refund spends it.
        assert_eq!(c.outspend(funding, 1).await.unwrap(), Spend::Unspent);
        assert!(matches!(
            c.outspend(funding, 7).await,
            Err(QueryError::NotFound(_))
        ));
        let refund = tx(
            &[OutPoint {
                txid: funding,
                vout: 1,
            }],
            &[(&refund_to, 49_000)],
        );
        let hex = serialize_hex(&refund);
        let refund_id = c.broadcast_hex(&hex).await.unwrap();
        assert_eq!(*src.broadcasts.lock().unwrap(), vec![refund_id]);
        // A second broadcast is the server's "already known".
        let again = c.broadcast_hex(&hex).await.unwrap_err();
        assert!(again.to_string().contains("txn-already"));
        assert_eq!(
            c.outspend(funding, 1).await.unwrap(),
            Spend::Spent {
                txid: refund_id,
                vin: 0,
                state: TxState::UNCONFIRMED
            }
        );
        // The refund's status comes from the deposit script it spends.
        src.history_calls.lock().unwrap().clear();
        assert_eq!(c.tx_state(refund_id).await.unwrap(), TxState::UNCONFIRMED);
        assert_eq!(*src.history_calls.lock().unwrap(), vec![deposit.clone()]);
        assert_eq!(c.tx_hex(refund_id).await.unwrap(), hex);

        // Every output ever paid, spent or not.
        let mut txos = c.address_txos(DEPOSIT).await.unwrap();
        txos.sort_by_key(|u| u.value);
        assert_eq!(
            txos.iter().map(|t| (t.txid, t.vout)).collect::<Vec<_>>(),
            vec![(pending, 0), (funding, 1)]
        );
        let utxos = c.address_utxos(DEPOSIT).await.unwrap();
        assert_eq!(utxos.len(), 1);

        // Wrong network or not an address.
        assert!(matches!(
            c.address_utxos("tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx")
                .await,
            Err(QueryError::InvalidAddress(_))
        ));
        assert!(matches!(
            c.address_utxos("nope").await,
            Err(QueryError::InvalidAddress(_))
        ));
    }

    #[tokio::test]
    async fn status_without_a_preferred_script_uses_the_first_output() {
        let src = Arc::new(FakeSource::default());
        let a = spk(EXCHANGE);
        let txid = src.add(tx(&[coinbase_like(1)], &[(&a, 1)]), 800_000);
        let c = chain(&src);
        assert_eq!(c.tx_state(txid).await.unwrap().height, Some(800_000));
        assert_eq!(*src.history_calls.lock().unwrap(), vec![a]);
    }

    #[tokio::test]
    async fn offline_is_connectivity() {
        let src = Arc::new(FakeSource {
            offline: true,
            ..Default::default()
        });
        let c = chain(&src);
        assert!(matches!(
            c.tip_height().await,
            Err(QueryError::Connectivity(_))
        ));
        assert!(matches!(
            c.fee_rates().await,
            Err(QueryError::Connectivity(_))
        ));
    }

    /// Live check (manual):
    /// `cargo test -p rail-onchain live_electrum_chain -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_electrum_chain() {
        let servers = electrum::default_servers(sessions_core::Network::Bitcoin).unwrap();
        let c = ElectrumChain::for_servers(&servers, sessions_core::Network::Bitcoin).unwrap();
        println!("tip {}", c.tip_height().await.unwrap());
        let pool = electrum::pool_for(&servers).unwrap();
        println!(
            "raw estimates (BTC/kvB) {:?}, relay {:?}",
            pool.estimate_fees(&FEE_RATE_TARGETS),
            pool.relay_fee()
        );
        println!("fees {:?}", c.fee_rates().await.unwrap());
        // BIP84 "abandon … about" account 0, first receive address: used,
        // with a modest history.
        let address = "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu";
        let utxos = c.address_utxos(address).await.unwrap();
        let txos = c.address_txos(address).await.unwrap();
        println!("{address}: {} utxos, {} txos", utxos.len(), txos.len());
        let first = txos
            .iter()
            .filter_map(|u| u.state.height.map(|h| (h, u)))
            .min_by_key(|(h, _)| *h)
            .map(|(_, u)| u.clone())
            .unwrap();
        println!("oldest {first:?}");
        let st = c.tx_state(first.txid).await.unwrap();
        println!("status {st:?}");
        assert_eq!(st.height, first.state.height);
        let spend = c.outspend(first.txid, first.vout).await.unwrap();
        println!("outspend {spend:?}");
        let unspent = utxos
            .iter()
            .any(|u| u.txid == first.txid && u.vout == first.vout);
        assert_eq!(spend == Spend::Unspent, unspent);
        // Block 170: the first bitcoin transaction (Satoshi to Hal Finney),
        // whose output 1 (Satoshi's change) was spent later.
        let t170 =
            Txid::from_str("f4184fc596403b9d638783cf57adfe4c75c605f6356fbc91338530e9831e9e16")
                .unwrap();
        let st = c.tx_state(t170).await.unwrap();
        println!("block 170 tx {st:?}");
        assert_eq!(st.height, Some(170));
        println!("its output 0: {:?}", c.outspend(t170, 0).await.unwrap());
        assert!(c.tx_hex(t170).await.unwrap().starts_with("0100"));
    }
}

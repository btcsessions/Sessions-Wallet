//! Which chain server an account talks to, and every request it makes.
//!
//! Electrum is the default (mainnet): one shared, batched connection with
//! server fallback (see [`crate::electrum`]). Esplora (REST) stays as an
//! alternative for anyone who points the app at their own Esplora, and for
//! test networks.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bdk_electrum::electrum_client::ElectrumApi;
use bdk_esplora::esplora_client::{self, AsyncClient};
use bdk_esplora::EsploraAsyncExt;
use bdk_wallet::bitcoin::{OutPoint, Script, Transaction, Txid};
use bdk_wallet::chain::spk_client::{FullScanRequest, FullScanResponse, SyncRequest, SyncResponse};
use bdk_wallet::chain::{ConfirmationBlockTime, TxUpdate};
use bdk_wallet::Wallet;
use sessions_core::{Error, Network, Result};

use crate::electrum::{self, Pool, ServerUrl};
use crate::net::{self, Lanes};

/// Spks per Electrum history batch (one round trip per batch).
pub const ELECTRUM_BATCH: usize = 25;
/// Esplora requests in flight per call.
pub const ESPLORA_PARALLEL: usize = 2;

/// Where on-chain data comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainServer {
    /// Electrum servers, tried in order.
    Electrum(Vec<ServerUrl>),
    /// An Esplora REST base URL.
    Esplora(String),
}

impl ChainServer {
    /// The server for `network`:
    ///
    /// 1. `electrum_url` when set (your own server; no fallback to public ones);
    /// 2. `esplora_url` when set (your own Esplora), unless it's just the
    ///    built-in mempool.space default, which older versions saved;
    /// 3. otherwise the public Electrum servers (mainnet), or the default
    ///    Esplora (test networks, which have no built-in Electrum servers).
    pub fn choose(
        network: Network,
        electrum_url: Option<&str>,
        esplora_url: Option<&str>,
    ) -> Result<ChainServer> {
        if let Some(e) = electrum_url.map(str::trim).filter(|s| !s.is_empty()) {
            return Ok(ChainServer::Electrum(vec![ServerUrl::parse(e)?]));
        }
        let default_esplora = crate::default_esplora(network);
        let esplora = esplora_url
            .map(|u| u.trim().trim_end_matches('/'))
            .filter(|u| !u.is_empty());
        match (esplora, electrum::default_servers(network)) {
            (Some(u), Some(servers)) if u.eq_ignore_ascii_case(default_esplora) => {
                Ok(ChainServer::Electrum(servers))
            }
            (Some(u), _) => Ok(ChainServer::Esplora(u.to_string())),
            (None, Some(servers)) => Ok(ChainServer::Electrum(servers)),
            (None, None) => Ok(ChainServer::Esplora(default_esplora.to_string())),
        }
    }

    /// A client for it. Electrum clients are shared: every account (and
    /// discovery, fees…) on the same servers uses one connection.
    pub fn backend(&self, network: Network) -> Result<Backend> {
        match self {
            ChainServer::Electrum(servers) => {
                Ok(Backend::Electrum(electrum::pool_for(servers)?, network))
            }
            ChainServer::Esplora(url) => Ok(Backend::Esplora {
                client: net::client(url)?,
                lanes: net::lanes_for(url),
            }),
        }
    }
}

impl std::fmt::Display for ChainServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainServer::Electrum(s) => write!(
                f,
                "Electrum {}",
                s.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ChainServer::Esplora(u) => write!(f, "Esplora {u}"),
        }
    }
}

/// A failed request. `transient`: a network problem worth retrying (a
/// timeout, a dropped connection, a busy server), not an answer.
#[derive(Debug, Clone)]
pub struct ChainError {
    pub message: String,
    pub transient: bool,
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<ChainError> for Error {
    fn from(e: ChainError) -> Error {
        Error::rail(crate::RAIL, e.message)
    }
}

impl ChainError {
    fn answer(e: impl std::fmt::Display) -> Self {
        ChainError {
            message: e.to_string(),
            transient: false,
        }
    }
}

fn from_esplora(e: &esplora_client::Error) -> ChainError {
    let transient = match e {
        esplora_client::Error::Reqwest(r) => {
            r.is_timeout() || r.is_connect() || r.is_request() || r.is_body()
        }
        esplora_client::Error::HttpResponse { status, .. } => *status == 429 || *status >= 500,
        _ => false,
    };
    ChainError {
        message: e.to_string(),
        transient,
    }
}

pub(crate) fn from_electrum(e: &electrum::electrum_client::Error) -> ChainError {
    ChainError {
        message: match e {
            electrum::electrum_client::Error::AllAttemptsErrored(v) => v
                .last()
                .map(ToString::to_string)
                .unwrap_or_else(|| e.to_string()),
            electrum::electrum_client::Error::Protocol(v) => v
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string()),
            _ => e.to_string(),
        },
        transient: electrum::is_transport(e),
    }
}

/// What a wallet already has, so Electrum caches don't download it again.
#[derive(Default)]
pub struct Known {
    txs: Vec<Arc<Transaction>>,
    anchors: Vec<(Txid, ConfirmationBlockTime)>,
}

impl Known {
    pub fn of(wallet: &Wallet) -> Self {
        let graph = wallet.tx_graph();
        Known {
            txs: graph.full_txs().map(|t| t.tx.clone()).collect(),
            anchors: graph
                .all_anchors()
                .iter()
                .flat_map(|(txid, a)| a.iter().map(|a| (*txid, *a)))
                .collect(),
        }
    }

    fn warm(self, pool: &Pool) {
        pool.conn()
            .mark_known(self.txs.iter().map(|t| t.compute_txid()));
        pool.client.populate_tx_cache(self.txs);
        let mut by_tx: HashMap<Txid, Vec<ConfirmationBlockTime>> = HashMap::new();
        for (t, a) in self.anchors {
            by_tx.entry(t).or_default().push(a);
        }
        pool.client.populate_anchor_cache(by_tx);
    }
}

/// The previous outputs of unconfirmed transactions, so their fees are
/// known (a speed-up of an incoming payment needs its parent's fee).
/// Esplora includes them; Electrum doesn't.
fn add_prevouts(
    pool: &Pool,
    update: &mut TxUpdate<ConfirmationBlockTime>,
) -> std::result::Result<(), electrum::electrum_client::Error> {
    let confirmed: std::collections::HashSet<Txid> =
        update.anchors.iter().map(|(_, t)| *t).collect();
    let mut wanted = Vec::new();
    for tx in &update.txs {
        if tx.is_coinbase() || confirmed.contains(&tx.compute_txid()) {
            continue;
        }
        wanted.extend(tx.input.iter().map(|i| i.previous_output));
    }
    for op in wanted {
        if update.txouts.contains_key(&op) {
            continue;
        }
        // Cached when it's one of ours; one request otherwise.
        let prev = pool.client.fetch_tx(op.txid)?;
        if let Some(out) = prev.output.get(op.vout as usize) {
            update.txouts.insert(op, out.clone());
        }
    }
    Ok(())
}

/// The chain server, ready for requests.
#[derive(Clone)]
pub enum Backend {
    Esplora {
        client: AsyncClient,
        lanes: Arc<Lanes>,
    },
    /// The pool, and the network (for the Esplora fee fallback).
    Electrum(Arc<Pool>, Network),
}

impl Backend {
    /// The request budget shared by everything using this server.
    pub(crate) fn lanes(&self) -> &Arc<Lanes> {
        match self {
            Backend::Esplora { lanes, .. } => lanes,
            Backend::Electrum(p, _) => &p.lanes,
        }
    }

    /// Extra tries for a whole scan or sync after a network problem.
    /// Electrum already retries each call on a fresh connection.
    pub fn extra_tries(&self) -> u32 {
        match self {
            Backend::Esplora { .. } => 2,
            Backend::Electrum(..) => 0,
        }
    }

    pub fn electrum(&self) -> Option<&Arc<Pool>> {
        match self {
            Backend::Electrum(p, _) => Some(p),
            Backend::Esplora { .. } => None,
        }
    }

    /// Blocking Electrum work, off the async workers.
    async fn blocking<T: Send + 'static>(
        pool: &Arc<Pool>,
        f: impl FnOnce(&Pool) -> std::result::Result<T, electrum::electrum_client::Error>
            + Send
            + 'static,
    ) -> std::result::Result<T, ChainError> {
        let pool = pool.clone();
        tokio::task::spawn_blocking(move || f(&pool).map_err(|e| from_electrum(&e)))
            .await
            .map_err(ChainError::answer)?
    }

    pub async fn full_scan<K: Ord + Clone + Send + 'static>(
        &self,
        request: FullScanRequest<K>,
        stop_gap: usize,
        known: Known,
    ) -> std::result::Result<FullScanResponse<K>, ChainError> {
        match self {
            Backend::Esplora { client, .. } => client
                .full_scan(request, stop_gap, ESPLORA_PARALLEL)
                .await
                .map_err(|e| from_esplora(&e)),
            Backend::Electrum(pool, _) => {
                Self::blocking(pool, move |p| {
                    known.warm(p);
                    let mut r = p
                        .client
                        .full_scan(request, stop_gap, ELECTRUM_BATCH, false)?;
                    add_prevouts(p, &mut r.tx_update)?;
                    Ok(r)
                })
                .await
            }
        }
    }

    pub async fn sync<I: Send + 'static>(
        &self,
        request: SyncRequest<I>,
        known: Known,
    ) -> std::result::Result<SyncResponse, ChainError> {
        match self {
            Backend::Esplora { client, .. } => client
                .sync(request, ESPLORA_PARALLEL)
                .await
                .map_err(|e| from_esplora(&e)),
            Backend::Electrum(pool, _) => {
                Self::blocking(pool, move |p| {
                    known.warm(p);
                    let mut r = p.client.sync(request, ELECTRUM_BATCH, false)?;
                    add_prevouts(p, &mut r.tx_update)?;
                    Ok(r)
                })
                .await
            }
        }
    }

    /// A transaction, or `None` if the server doesn't know it.
    pub async fn get_tx(&self, txid: Txid) -> std::result::Result<Option<Transaction>, ChainError> {
        match self {
            Backend::Esplora { client, .. } => {
                client.get_tx(&txid).await.map_err(|e| from_esplora(&e))
            }
            Backend::Electrum(pool, _) => {
                Self::blocking(pool, move |p| match p.client.fetch_tx(txid) {
                    Ok(tx) => Ok(Some((*tx).clone())),
                    // The server answered: it doesn't have it.
                    Err(e) if !electrum::is_transport(&e) => Ok(None),
                    Err(e) => Err(e),
                })
                .await
            }
        }
    }

    pub async fn broadcast(&self, tx: &Transaction) -> std::result::Result<(), ChainError> {
        match self {
            Backend::Esplora { client, .. } => {
                client.broadcast(tx).await.map_err(|e| from_esplora(&e))
            }
            Backend::Electrum(pool, _) => {
                let tx = tx.clone();
                Self::blocking(pool, move |p| {
                    p.client.transaction_broadcast(&tx)?;
                    // Ours now: don't fetch it back later.
                    p.client.populate_tx_cache([tx]);
                    Ok(())
                })
                .await
            }
        }
    }

    /// Transactions paying to or spending from `script` (confirmed and
    /// unconfirmed).
    pub async fn script_txids(
        &self,
        script: &Script,
    ) -> std::result::Result<Vec<Txid>, ChainError> {
        match self {
            Backend::Esplora { client, .. } => Ok(client
                .scripthash_txs(script, None)
                .await
                .map_err(|e| from_esplora(&e))?
                .into_iter()
                .map(|t| t.txid)
                .collect()),
            Backend::Electrum(pool, _) => {
                let script = script.to_owned();
                Self::blocking(pool, move |p| {
                    Ok(p.conn()
                        .script_get_history(&script)?
                        .into_iter()
                        .map(|h| h.tx_hash)
                        .collect())
                })
                .await
            }
        }
    }

    /// Whether `outpoint` (paying to `script`) exists and is unspent.
    pub async fn is_unspent(
        &self,
        outpoint: OutPoint,
        script: &Script,
    ) -> std::result::Result<bool, ChainError> {
        match self {
            Backend::Esplora { client, .. } => Ok(client
                .get_output_status(&outpoint.txid, outpoint.vout as u64)
                .await
                .map_err(|e| from_esplora(&e))?
                .is_some_and(|s| !s.spent)),
            Backend::Electrum(pool, _) => {
                let script = script.to_owned();
                Self::blocking(pool, move |p| {
                    Ok(p.conn()
                        .script_list_unspent(&script)?
                        .iter()
                        .any(|u| u.tx_hash == outpoint.txid && u.tx_pos == outpoint.vout as usize))
                })
                .await
            }
        }
    }

    /// Fee estimates in sat/vB by confirmation target (blocks), in the shape
    /// Esplora's `fee-estimates` has (see [`esplora_client::convert_fee_rate`]).
    pub async fn fee_estimates(&self) -> std::result::Result<HashMap<u16, f64>, ChainError> {
        // A server can say anything: never take more than this from it, so a
        // dishonest one can't make every send overpay.
        self.raw_fee_estimates()
            .await
            .map(|m| m.into_iter().map(|(t, r)| (t, r.min(MAX_SERVER_FEE_RATE))).collect())
    }

    async fn raw_fee_estimates(&self) -> std::result::Result<HashMap<u16, f64>, ChainError> {
        match self {
            Backend::Esplora { client, .. } => client
                .get_fee_estimates()
                .await
                .map_err(|e| from_esplora(&e)),
            Backend::Electrum(pool, network) => {
                let est = Self::blocking(pool, |p| {
                    p.conn()
                        .batch_estimate_fee(FEE_TARGETS.iter().map(|t| *t as usize))
                })
                .await
                .and_then(|btc_per_kvb| {
                    electrum_fees(&btc_per_kvb).ok_or_else(|| {
                        ChainError::answer("the Electrum server has no fee estimates")
                    })
                });
                match est {
                    Ok(e) => Ok(e),
                    // Fall back to the public Esplora fee estimates.
                    Err(e) => {
                        tracing::info!("Electrum fee estimates unavailable ({e}); asking Esplora");
                        let url = crate::default_esplora(*network);
                        net::client(url)
                            .map_err(ChainError::answer)?
                            .get_fee_estimates()
                            .await
                            .map_err(|e| from_esplora(&e))
                    }
                }
            }
        }
    }
}

/// The most a chain server's fee estimate is believed (sat/vB). Past peaks
/// stayed well under this; a custom fee can still go higher.
pub const MAX_SERVER_FEE_RATE: f64 = 1_000.0;

/// Targets asked for from Electrum (`blockchain.estimatefee`).
pub const FEE_TARGETS: [u16; 6] = [1, 2, 3, 6, 12, 144];

/// Electrum's estimates (BTC/kvB per [`FEE_TARGETS`], -1 when unknown) as
/// sat/vB by target. Unknown targets borrow the next longer known one;
/// `None` if none is known.
pub fn electrum_fees(btc_per_kvb: &[f64]) -> Option<HashMap<u16, f64>> {
    let rates: Vec<Option<f64>> = btc_per_kvb
        .iter()
        .map(|r| Some(*r * 100_000.0).filter(|r| r.is_finite() && *r > 0.0))
        .collect();
    let mut out = HashMap::new();
    let mut later: Option<f64> = None;
    for (i, target) in FEE_TARGETS.iter().enumerate().rev() {
        let r = rates.get(i).copied().flatten().or(later);
        if let Some(r) = r {
            // Longer targets never cost more than shorter ones.
            let r = later.map_or(r, |l| r.max(l));
            out.insert(*target, r);
            later = Some(r);
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(not(test))]
const RETRY_PAUSE: Duration = Duration::from_millis(500);
#[cfg(test)]
const RETRY_PAUSE: Duration = Duration::from_millis(5);

/// Retry `f` (a whole scan or sync, rebuilt each time) after a transient
/// network problem: `tries` more times, quickly. For Esplora, whose client
/// only retries on HTTP status codes, so one stalled request would
/// otherwise fail a whole scan.
pub async fn with_retries<T, F, Fut>(
    what: &str,
    tries: u32,
    mut f: F,
) -> std::result::Result<T, ChainError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, ChainError>>,
{
    let mut attempt = 0;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if e.transient && attempt < tries => {
                attempt += 1;
                tracing::info!("{what} failed ({e}); trying again ({attempt}/{tries})");
                tokio::time::sleep(RETRY_PAUSE * 2u32.pow(attempt)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_electrum_by_default_on_mainnet() {
        let defaults = electrum::default_servers(Network::Bitcoin).unwrap();
        assert_eq!(
            ChainServer::choose(Network::Bitcoin, None, None).unwrap(),
            ChainServer::Electrum(defaults.clone())
        );
        // The old saved default (mempool.space) isn't a choice of server.
        assert_eq!(
            ChainServer::choose(Network::Bitcoin, None, Some("https://mempool.space/api/"))
                .unwrap(),
            ChainServer::Electrum(defaults)
        );
        // Your own Esplora is used as is.
        assert_eq!(
            ChainServer::choose(Network::Bitcoin, None, Some("http://umbrel.local:3002")).unwrap(),
            ChainServer::Esplora("http://umbrel.local:3002".into())
        );
        // Your own Electrum wins, alone (no public fallback).
        assert_eq!(
            ChainServer::choose(
                Network::Bitcoin,
                Some("tcp://umbrel.local:50001"),
                Some("http://umbrel.local:3002")
            )
            .unwrap(),
            ChainServer::Electrum(vec![ServerUrl::parse("tcp://umbrel.local:50001").unwrap()])
        );
        assert!(ChainServer::choose(Network::Bitcoin, Some("https://nope"), None).is_err());
        // Blank means unset.
        assert!(matches!(
            ChainServer::choose(Network::Bitcoin, Some("  "), None).unwrap(),
            ChainServer::Electrum(v) if v.len() > 1
        ));
    }

    #[test]
    fn test_networks_use_esplora() {
        assert_eq!(
            ChainServer::choose(Network::Signet, None, None).unwrap(),
            ChainServer::Esplora(crate::default_esplora(Network::Signet).into())
        );
        assert_eq!(
            ChainServer::choose(Network::Regtest, None, None).unwrap(),
            ChainServer::Esplora("http://127.0.0.1:3002".into())
        );
        assert!(matches!(
            ChainServer::choose(Network::Testnet, Some("ssl://testnet.example:60002"), None)
                .unwrap(),
            ChainServer::Electrum(_)
        ));
    }

    #[test]
    fn electrum_fee_estimates_in_sat_per_vb() {
        // BTC/kvB for targets 1, 2, 3, 6, 12, 144.
        let e = electrum_fees(&[0.0002, 0.00015, -1.0, 0.00005, 0.00002, 0.00001]).unwrap();
        let at = |t| esplora_client::convert_fee_rate(t, e.clone()).unwrap();
        assert!((at(1) - 20.0).abs() < 1e-3);
        // 3 unknown: borrows from 6.
        assert!((at(3) - 5.0).abs() < 1e-3);
        assert!((at(6) - 5.0).abs() < 1e-3);
        assert!((at(144) - 1.0).abs() < 1e-3);
        assert!((at(1000) - 1.0).abs() < 1e-3);
        // Never cheaper to confirm sooner.
        let e = electrum_fees(&[0.00001, 0.00003, 0.00003, 0.00003, 0.00003, 0.00003]).unwrap();
        assert!(e[&1] >= e[&2]);
        assert!(electrum_fees(&[-1.0; 6]).is_none());
        assert!(electrum_fees(&[]).is_none());
    }

    #[tokio::test]
    async fn retries_only_transient_errors() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = AtomicU32::new(0);
        let r: std::result::Result<(), _> = with_retries("scan", 2, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(ChainError {
                message: "timed out".into(),
                transient: true,
            })
        })
        .await;
        assert!(r.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        calls.store(0, Ordering::SeqCst);
        let r: std::result::Result<(), _> = with_retries("scan", 2, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(ChainError::answer("bad request"))
        })
        .await;
        assert!(r.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        calls.store(0, Ordering::SeqCst);
        let r = with_retries("scan", 2, || async {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(ChainError {
                    message: "reset".into(),
                    transient: true,
                })
            } else {
                Ok(7)
            }
        })
        .await;
        assert_eq!(r.unwrap(), 7);
    }
}

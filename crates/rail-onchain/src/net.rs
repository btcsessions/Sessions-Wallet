//! How on-chain accounts use their chain server.
//!
//! * **Lanes.** Every account talking to the same Esplora server shares one
//!   request budget, so several accounts syncing at once queue instead of
//!   bursting (bursts get rate limited, and the retries make it worse).
//!   Long work (first scans, wide syncs), light syncs and quick checks each
//!   get their own lane, so a long scan never starves the Receive screen.
//! * **Background jobs.** An account's first full scan and its periodic wide
//!   sync run in a task the account owns, not in the caller's future. A
//!   caller that gives up (the 30 s sync round, a closed screen) doesn't
//!   cancel them, so a big wallet's first scan always finishes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bdk_esplora::esplora_client::AsyncClient;
use bdk_esplora::EsploraAsyncExt;
use bdk_wallet::chain::spk_client::FullScanRequest;
use bdk_wallet::{KeychainKind, Wallet};
use sessions_core::{AccountId, Error, Result};
use tokio::sync::{Mutex, Notify, Semaphore};

use crate::{err, Inner};

/// Unused addresses in a row before a scan stops (BIP44's gap limit).
pub const STOP_GAP: usize = 20;
/// Requests in flight per account job.
pub const PARALLEL_REQUESTS: usize = 2;
/// Per-request limit, so one stuck connection can't hold a sync for minutes.
pub const REQUEST_TIMEOUT_SECS: u64 = 15;

/// Longest a background job may take before it's stopped and retried.
const JOB_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// First-scan retries: 10 s, 20 s, 40 s, then every 80 s.
#[cfg(not(test))]
const RETRY_BASE: Duration = Duration::from_secs(10);
#[cfg(test)]
const RETRY_BASE: Duration = Duration::from_millis(20);
const RETRY_MAX: Duration = Duration::from_secs(80);

/// Long jobs (first scans, wide syncs) at once per server.
// Three at a time, so small wallets don't wait behind one big one
// (with 2 requests each, at most 6 long-job requests in flight).
pub(crate) const BACKGROUND_PERMITS: usize = 3;
/// Light syncs at once per server.
const LIGHT_PERMITS: usize = 2;
/// Quick checks (an address on screen, a pushed transaction) at once per server.
const QUICK_PERMITS: usize = 2;

/// Called with the account id when an account's balance or history changes.
pub type ChangeHook = Arc<dyn Fn(&AccountId) + Send + Sync>;

/// An Esplora client with sane limits, tuned for flaky mobile networks:
///
/// * HTTP/1.1 only. On some mobile networks HTTP/2 connections to the chain
///   server stall ("keep-alive timed out") while new HTTP/1.1 requests work.
/// * IPv4 first (IPv6 as a fallback). A common mobile failure is an IPv6 path
///   that connects but drops large responses, so every request times out.
/// * Short connect timeout and TCP keepalive, so dead connections are noticed.
pub fn client(url: &str) -> Result<AsyncClient> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(8))
        .http1_only()
        .pool_idle_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(20))
        .dns_resolver(Arc::new(PreferIpv4))
        .build()
        .map_err(err)?;
    Ok(AsyncClient::from_client(url.to_string(), http))
}

/// System DNS, with IPv4 addresses tried before IPv6 ones.
struct PreferIpv4;

impl reqwest::dns::Resolve for PreferIpv4 {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = format!("{}:0", name.as_str());
            let mut addrs: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host(host).await?.collect();
            ipv4_first(&mut addrs);
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn ipv4_first(addrs: &mut [std::net::SocketAddr]) {
    addrs.sort_by_key(|a| !a.is_ipv4());
}

/// The request budget for one server, shared by every account using it.
pub(crate) struct Lanes {
    pub background: Semaphore,
    pub light: Semaphore,
    pub quick: Semaphore,
}

impl Lanes {
    fn new() -> Self {
        Lanes {
            background: Semaphore::new(BACKGROUND_PERMITS),
            light: Semaphore::new(LIGHT_PERMITS),
            quick: Semaphore::new(QUICK_PERMITS),
        }
    }
}

fn server_key(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// The lanes for `url`, created on first use.
pub(crate) fn lanes_for(url: &str) -> Arc<Lanes> {
    static ALL: OnceLock<std::sync::Mutex<HashMap<String, Arc<Lanes>>>> = OnceLock::new();
    let mut all = ALL
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    all.entry(server_key(url))
        .or_insert_with(|| Arc::new(Lanes::new()))
        .clone()
}

/// What a background job is doing.
#[derive(Debug, Clone)]
pub(crate) enum Phase {
    Idle,
    /// Waiting for a free slot on the server.
    Queued(Instant),
    /// Talking to the server about these addresses.
    Scanning(&'static str, Instant),
    /// Failed with this error; the next attempt is at this time.
    Retrying(String, Instant),
}

/// A background job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Job {
    /// First full scan: receive addresses, then change addresses.
    FirstScan,
    /// Every revealed address (catches payments to old, reused addresses).
    Wide,
}

/// An account's network state, shared with its background job.
pub(crate) struct Net {
    pub id: AccountId,
    pub client: AsyncClient,
    pub lanes: Arc<Lanes>,
    /// Written once the first full scan has finished (see [`Inner::scanned`]).
    pub marker: PathBuf,
    /// A background job is queued or running.
    running: AtomicBool,
    /// ... and has its turn on the server (not just queued).
    active: AtomicBool,
    /// Woken when a job finishes or a keychain finishes its first scan.
    pub progress: Notify,
    /// One light sync at a time per account.
    pub light: Mutex<()>,
    hook: std::sync::RwLock<Option<ChangeHook>>,
    /// What the background job is doing, for the "Loading history" note.
    phase: std::sync::Mutex<Phase>,
    /// Why the last background job failed (cleared when one succeeds).
    error: std::sync::Mutex<Option<String>>,
    /// The background job, stopped when the account is closed so it never
    /// writes to a database another (re-opened) account now owns.
    job: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
}

impl Net {
    pub fn new(id: AccountId, url: &str, marker: PathBuf) -> Result<Self> {
        Ok(Net {
            id,
            client: client(url)?,
            lanes: lanes_for(url),
            marker,
            running: AtomicBool::new(false),
            active: AtomicBool::new(false),
            progress: Notify::new(),
            light: Mutex::new(()),
            hook: std::sync::RwLock::new(None),
            error: std::sync::Mutex::new(None),
            phase: std::sync::Mutex::new(Phase::Idle),
            job: std::sync::Mutex::new(None),
        })
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// A background job is talking to the server right now.
    pub fn active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// Stop the background job, if any (the account is closing).
    pub fn abort(&self) {
        if let Some(job) = self.job.lock().unwrap_or_else(|e| e.into_inner()).take() {
            job.abort();
        }
    }

    pub(crate) fn set_phase(&self, p: Phase) {
        *self.phase.lock().unwrap_or_else(|e| e.into_inner()) = p;
    }

    /// Plain words for what the history scan is doing right now.
    pub fn status(&self) -> Option<String> {
        let secs = |t: &Instant| t.elapsed().as_secs();
        match &*self.phase.lock().unwrap_or_else(|e| e.into_inner()) {
            Phase::Idle => None,
            Phase::Queued(t) => Some(format!(
                "Waiting for other wallets to finish loading ({} s)",
                secs(t)
            )),
            Phase::Scanning(what, t) => Some(format!("Checking {what} ({} s)", secs(t))),
            Phase::Retrying(e, at) => Some(format!(
                "Couldn't load it: {e}. Trying again in {} s",
                at.saturating_duration_since(Instant::now()).as_secs()
            )),
        }
    }

    /// Why the last background job failed, if it did.
    pub fn last_error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_error(&self, e: Option<String>) {
        *self.error.lock().unwrap_or_else(|e| e.into_inner()) = e;
    }

    pub fn set_hook(&self, hook: ChangeHook) {
        *self.hook.write().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    /// Tell the app this account changed.
    pub fn changed(&self) {
        let hook = self.hook.read().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(h) = hook {
            h(&self.id);
        }
    }
}

/// What the user can see of a wallet: when this changes, the UI refreshes.
/// Leaves out the chain tip so a new block alone doesn't count.
pub(crate) type Fingerprint = (usize, usize, u64, u64, u64);

pub(crate) fn fingerprint(w: &Wallet) -> Fingerprint {
    let b = w.balance();
    let (mut all, mut confirmed) = (0, 0);
    for t in w.transactions() {
        all += 1;
        if t.chain_position.is_confirmed() {
            confirmed += 1;
        }
    }
    (
        all,
        confirmed,
        b.confirmed.to_sat(),
        b.trusted_pending.to_sat(),
        (b.untrusted_pending + b.immature).to_sat(),
    )
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Start `job` in the background unless one is already queued or running.
/// Returns `true` when a job (this one or an earlier one) is under way.
pub(crate) fn start(job: Job, inner: &Arc<Mutex<Inner>>, net: &Arc<Net>) -> bool {
    if net.running.swap(true, Ordering::SeqCst) {
        return true;
    }
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        net.running.store(false, Ordering::SeqCst);
        return false;
    };
    let mut slot = net.job.lock().unwrap_or_else(|e| e.into_inner());
    let (inner, net) = (inner.clone(), net.clone());
    let handle = rt.spawn(async move {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let started = Instant::now();
            if job == Job::FirstScan {
                net.set_phase(Phase::Queued(started));
            }
            // Its own task, so a panic can't leave `running` stuck at true;
            // and a time limit, so a hung connection can't hold the server
            // lane (and every other account's scan) forever.
            let (i, n) = (inner.clone(), net.clone());
            let task =
                tokio::spawn(
                    async move { tokio::time::timeout(JOB_TIMEOUT, run(job, &i, &n)).await },
                );
            // If this job is aborted (account closing), stop the inner task too.
            let _stop = AbortOnDrop(task.abort_handle());
            let res = match task.await {
                Ok(Ok(r)) => r,
                Ok(Err(_)) => Err(Error::Network(format!(
                    "the server didn't finish within {} minutes",
                    JOB_TIMEOUT.as_secs() / 60
                ))),
                Err(e) if e.is_panic() => {
                    Err(Error::Storage("internal error while scanning".into()))
                }
                Err(_) => return, // aborted: the account is closing
            };
            net.active.store(false, Ordering::SeqCst);
            match res {
                Ok(changed) => {
                    tracing::info!(
                        account = %net.id,
                        "{job:?} finished in {:.1} s",
                        started.elapsed().as_secs_f32()
                    );
                    let had_error = net.last_error().is_some();
                    net.set_error(None);
                    net.set_phase(Phase::Idle);
                    net.running.store(false, Ordering::SeqCst);
                    net.progress.notify_waiters();
                    if changed || had_error {
                        net.changed();
                    }
                    return;
                }
                Err(e) => {
                    tracing::warn!(account = %net.id, "{job:?} attempt {attempt} failed: {e}");
                    net.set_error(Some(e.to_string()));
                    net.progress.notify_waiters();
                    net.changed();
                    // Only the first scan is worth retrying on its own; a wide
                    // sync just runs again on its schedule.
                    if job != Job::FirstScan {
                        net.running.store(false, Ordering::SeqCst);
                        return;
                    }
                    let wait =
                        (RETRY_BASE * 2u32.saturating_pow(attempt.min(4) - 1)).min(RETRY_MAX);
                    net.set_phase(Phase::Retrying(short_error(&e), Instant::now() + wait));
                    net.changed();
                    tokio::time::sleep(wait).await;
                }
            }
        }
    });
    *slot = Some(handle.abort_handle());
    true
}

/// Wait until the receive addresses have been scanned once, the background
/// job stops (e.g. offline), or `limit` passes, whichever comes first.
pub(crate) async fn wait_for_receive_scan(inner: &Mutex<Inner>, net: &Net, limit: Duration) {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        // Registered before checking, so a wake-up in between isn't lost.
        let woken = net.progress.notified();
        if inner.lock().await.external_scanned || !net.running() {
            return;
        }
        if tokio::time::timeout_at(deadline, woken).await.is_err() {
            return;
        }
    }
}

/// Run one job. `Ok(true)` when something the user sees changed.
async fn run(job: Job, inner: &Mutex<Inner>, net: &Net) -> Result<bool> {
    let _permit = net
        .lanes
        .background
        .acquire()
        .await
        .map_err(|e| err(e.to_string()))?;
    net.active.store(true, Ordering::SeqCst);
    match job {
        Job::FirstScan => first_scan(inner, net).await,
        Job::Wide => wide_sync(inner, net).await,
    }
}

/// An error short enough for a one-line note.
fn short_error(e: &Error) -> String {
    let s = e.to_string().replace('\n', " ");
    let s = s.trim();
    if s.chars().count() > 120 {
        s.chars().take(120).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

fn storage(e: impl std::fmt::Display) -> Error {
    Error::Storage(e.to_string())
}

/// Scan one keychain from index 0 until [`STOP_GAP`] unused addresses in a row.
fn keychain_scan(w: &Wallet, k: KeychainKind) -> FullScanRequest<KeychainKind> {
    let index = w.spk_index();
    let mut b = FullScanRequest::builder().chain_tip(w.local_chain().tip());
    if let Some(spks) = index.unbounded_spk_iter(k) {
        b = b.spks_for_keychain(k, spks);
    }
    if let Some(last) = index.last_revealed_index(k) {
        b = b.last_revealed_for_keychain(k, last);
    }
    b.build()
}

/// Receive addresses first, so the Receive screen can hand out a fresh
/// address as soon as possible; then change addresses. Each keychain is
/// saved as soon as it's done.
async fn first_scan(inner: &Mutex<Inner>, net: &Net) -> Result<bool> {
    for k in [KeychainKind::External, KeychainKind::Internal] {
        let request = {
            let inner = inner.lock().await;
            if k == KeychainKind::External && inner.external_scanned {
                continue; // an earlier attempt got this far
            }
            keychain_scan(&inner.wallet, k)
        };
        net.set_phase(Phase::Scanning(
            if k == KeychainKind::External {
                "receive addresses"
            } else {
                "change addresses"
            },
            Instant::now(),
        ));
        tracing::info!(account = %net.id, "first scan: {k:?} keychain");
        let update = net
            .client
            .full_scan(request, STOP_GAP, PARALLEL_REQUESTS)
            .await
            .map_err(err)?;
        let mut guard = inner.lock().await;
        let i = &mut *guard;
        i.wallet.apply_update(update).map_err(err)?;
        i.wallet.persist(&mut i.conn).map_err(storage)?;
        if k == KeychainKind::External {
            i.external_scanned = true;
            net.progress.notify_waiters();
        }
    }
    let mut i = inner.lock().await;
    i.scanned = true;
    i.last_wide_sync = Some(Instant::now());
    if let Err(e) = std::fs::write(&net.marker, b"1") {
        tracing::warn!(account = %net.id, "couldn't record the finished scan: {e}");
    }
    // Always tell the UI: `scanning` went from true to false.
    Ok(true)
}

async fn wide_sync(inner: &Mutex<Inner>, net: &Net) -> Result<bool> {
    let (request, before) = {
        let i = inner.lock().await;
        (
            i.wallet.start_sync_with_revealed_spks().build(),
            fingerprint(&i.wallet),
        )
    };
    let update = net
        .client
        .sync(request, PARALLEL_REQUESTS)
        .await
        .map_err(err)?;
    let mut guard = inner.lock().await;
    let i = &mut *guard;
    i.wallet.apply_update(update).map_err(err)?;
    i.last_wide_sync = Some(Instant::now());
    i.wallet.persist(&mut i.conn).map_err(storage)?;
    Ok(fingerprint(&i.wallet) != before)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_addresses_come_first() {
        let mut a: Vec<std::net::SocketAddr> = vec![
            "[2001:db8::1]:443".parse().unwrap(),
            "192.0.2.1:443".parse().unwrap(),
            "[2001:db8::2]:443".parse().unwrap(),
            "192.0.2.2:443".parse().unwrap(),
        ];
        ipv4_first(&mut a);
        assert!(a[0].is_ipv4() && a[1].is_ipv4() && a[2].is_ipv6());
        assert_eq!(a[0].to_string(), "192.0.2.1:443");
    }

    #[test]
    fn lanes_are_shared_per_server() {
        let a = lanes_for("https://mempool.space/api");
        let b = lanes_for("https://Mempool.space/api/");
        let c = lanes_for("https://blockstream.info/api");
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(a.background.available_permits(), BACKGROUND_PERMITS);
    }
}

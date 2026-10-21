//! Finding accounts a restored wallet used before (BIP44 account discovery).
//!
//! Scans are throwaway: an in-memory BDK wallet per account, nothing saved.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use bdk_esplora::esplora_client;
use bdk_esplora::EsploraAsyncExt;
use bdk_wallet::Wallet;
use sessions_core::{Error, Network, Result};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::descriptors::DescriptorPair;

/// Unused addresses in a row before an account's scan stops.
pub const ADDRESS_GAP: usize = 20;
/// Empty accounts in a row, past the highest used one, before discovery stops.
pub const ACCOUNT_GAP: u32 = 2;
/// Accounts scanned at once.
pub const CONCURRENCY: usize = 4;
/// Esplora requests in flight per account.
const PARALLEL_REQUESTS: usize = 2;

/// What a quick scan found for one account.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Activity {
    pub balance_sat: u64,
    pub tx_count: usize,
}

impl Activity {
    pub fn used(&self) -> bool {
        self.tx_count > 0
    }
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::rail("discovery", e)
}

/// Scan one descriptor pair against Esplora without saving anything.
pub async fn scan(esplora_url: &str, network: Network, pair: &DescriptorPair) -> Result<Activity> {
    let mut wallet = Wallet::create(pair.external.clone(), pair.internal.clone())
        .network(network.to_bitcoin())
        .create_wallet_no_persist()
        .map_err(|e| Error::Crypto(e.to_string()))?;
    let client = esplora_client::Builder::new(esplora_url)
        .timeout(20)
        .build_async()
        .map_err(err)?;
    let request = wallet.start_full_scan().build();
    let update = client
        .full_scan(request, ADDRESS_GAP, PARALLEL_REQUESTS)
        .await
        .map_err(err)?;
    wallet.apply_update(update).map_err(err)?;
    Ok(Activity {
        balance_sat: wallet.balance().total().to_sat(),
        tx_count: wallet.transactions().count(),
    })
}

/// How many leading account numbers discovery has to look at, given the ones
/// known to be used: up to `gap` empty accounts past the highest used one,
/// always at least account 0, never more than `max`.
pub fn accounts_to_check(used: impl IntoIterator<Item = u32>, gap: u32, max: u32) -> u32 {
    used.into_iter()
        .max()
        .map_or(gap, |h| h.saturating_add(1).saturating_add(gap))
        .max(1)
        .min(max)
}

/// Run `scan` for each index, `concurrency` at a time, into `out`.
/// Returns `false` if `deadline` passed first.
async fn scan_batch<F, Fut>(
    indices: &[u32],
    concurrency: usize,
    deadline: Instant,
    scan: &F,
    out: &mut BTreeMap<u32, Activity>,
) -> Result<bool>
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = Result<Activity>> + Send + 'static,
{
    for chunk in indices.chunks(concurrency.max(1)) {
        let mut set = JoinSet::new();
        for &i in chunk {
            let f = scan(i);
            set.spawn(async move { (i, f.await) });
        }
        loop {
            match tokio::time::timeout_at(deadline, set.join_next()).await {
                Ok(Some(joined)) => {
                    let (i, res) = joined.map_err(err)?;
                    out.insert(i, res?);
                }
                Ok(None) => break,
                // Dropping the set aborts the scans still running.
                Err(_) => return Ok(false),
            }
        }
    }
    Ok(true)
}

fn finish(found: BTreeMap<u32, Activity>, complete: bool) -> Result<Vec<(u32, Activity)>> {
    let used: Vec<_> = found.into_iter().filter(|(_, a)| a.used()).collect();
    if !complete {
        if used.is_empty() {
            return Err(err(
                "the server took too long. Try again, or check your server in Settings",
            ));
        }
        tracing::warn!("account discovery timed out; returning what was found");
    }
    Ok(used)
}

/// BIP44 account discovery over account numbers `0..max`. `known` are
/// accounts already added: treated as used, never scanned. Returns the used
/// accounts found, in order. On timeout, returns what it found so far (an
/// error if that's nothing).
pub async fn discover<F, Fut>(
    known: &BTreeSet<u32>,
    max: u32,
    gap: u32,
    concurrency: usize,
    deadline: Instant,
    scan: F,
) -> Result<Vec<(u32, Activity)>>
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = Result<Activity>> + Send + 'static,
{
    let mut checked = BTreeMap::new();
    loop {
        let used = known.iter().copied().chain(
            checked
                .iter()
                .filter(|(_, a): &(&u32, &Activity)| a.used())
                .map(|(i, _)| *i),
        );
        let n = accounts_to_check(used, gap, max);
        let todo: Vec<u32> = (0..n)
            .filter(|i| !known.contains(i) && !checked.contains_key(i))
            .collect();
        if todo.is_empty() {
            return finish(checked, true);
        }
        if !scan_batch(&todo, concurrency, deadline, &scan, &mut checked).await? {
            return finish(checked, false);
        }
    }
}

/// Scan `count` independent accounts (by position) and return the used ones.
pub async fn scan_each<F, Fut>(
    count: u32,
    concurrency: usize,
    deadline: Instant,
    scan: F,
) -> Result<Vec<(u32, Activity)>>
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = Result<Activity>> + Send + 'static,
{
    let mut checked = BTreeMap::new();
    let all: Vec<u32> = (0..count).collect();
    let complete = scan_batch(&all, concurrency, deadline, &scan, &mut checked).await?;
    finish(checked, complete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn stop_rule() {
        // Nothing used: accounts 0 and 1.
        assert_eq!(accounts_to_check([], 2, 20), 2);
        // Highest used 0 → check through 2; highest 5 → through 7.
        assert_eq!(accounts_to_check([0], 2, 20), 3);
        assert_eq!(accounts_to_check([0, 5, 3], 2, 20), 8);
        // Capped, and account 0 is always checked.
        assert_eq!(accounts_to_check([19], 2, 20), 20);
        assert_eq!(accounts_to_check([], 0, 20), 1);
    }

    /// Run discovery against a fixed list of per-account activity.
    async fn simulate(activity: &[bool], known: &[u32]) -> (Vec<u32>, Vec<u32>) {
        let scanned = Arc::new(Mutex::new(Vec::new()));
        let activity = Arc::new(activity.to_vec());
        let log = scanned.clone();
        let found = discover(
            &known.iter().copied().collect(),
            20,
            ACCOUNT_GAP,
            3,
            Instant::now() + Duration::from_secs(5),
            move |i| {
                log.lock().unwrap().push(i);
                let used = activity.get(i as usize).copied().unwrap_or(false);
                async move {
                    Ok(Activity {
                        balance_sat: if used { 1000 } else { 0 },
                        tx_count: used as usize,
                    })
                }
            },
        )
        .await
        .unwrap();
        let mut scanned = scanned.lock().unwrap().clone();
        scanned.sort();
        (found.into_iter().map(|(i, _)| i).collect(), scanned)
    }

    #[tokio::test]
    async fn discovery_walks_until_two_empty() {
        // Fresh wallet: 0 and 1 checked, nothing found.
        assert_eq!(simulate(&[], &[]).await, (vec![], vec![0, 1]));
        // 0, 1, 3 used (2 is a hole): keeps going through 5.
        assert_eq!(
            simulate(&[true, true, false, true], &[]).await,
            (vec![0, 1, 3], vec![0, 1, 2, 3, 4, 5])
        );
        // Two empty in a row hide later accounts (as BIP44 intends).
        assert_eq!(
            simulate(&[true, false, false, true], &[]).await,
            (vec![0], vec![0, 1, 2])
        );
        // Already-added accounts aren't scanned but count as used.
        assert_eq!(
            simulate(&[false, false, false, true], &[0, 1]).await,
            (vec![3], vec![2, 3, 4, 5])
        );
        // Index 0 is checked even if only later ones are added.
        assert_eq!(simulate(&[true], &[2]).await, (vec![0], vec![0, 1, 3, 4]));
        // Never past account 19.
        let all = [true; 30];
        let (found, scanned) = simulate(&all, &[]).await;
        assert_eq!(found, (0..20).collect::<Vec<_>>());
        assert_eq!(scanned, (0..20).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn timeout_keeps_partial_results() {
        let deadline = Instant::now() + Duration::from_millis(200);
        let found = scan_each(4, 4, deadline, |i| async move {
            if i == 0 {
                Ok(Activity {
                    balance_sat: 5,
                    tx_count: 1,
                })
            } else {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(Activity::default())
            }
        })
        .await
        .unwrap();
        assert_eq!(found.len(), 1);
        // Nothing found before the deadline is an error, not "no accounts".
        let deadline = Instant::now() + Duration::from_millis(50);
        assert!(scan_each(2, 2, deadline, |_| async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(Activity::default())
        })
        .await
        .is_err());
    }
}

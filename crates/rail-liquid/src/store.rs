//! On-disk swap journal.
//!
//! Every swap is written here before anything the user could pay is handed
//! out, and every state change (especially a signed claim / refund
//! transaction) is written *before* it is broadcast, so a crash at any point
//! leaves enough on disk to finish the swap on the next [`crate::BoltzSwaps`]
//! start.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sessions_core::{Error, Network, Result, Sats};

pub const STORE_FILE: &str = "boltz-swaps.json";
const STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SwapKind {
    /// Lightning -> L-BTC (we pay Boltz's invoice, claim L-BTC to the target).
    Reverse,
    /// BTC -> L-BTC chain swap (we lock BTC, claim L-BTC to the target).
    ChainBtcToLbtc,
    /// L-BTC -> Lightning (an outside sender locks L-BTC, Boltz pays our invoice).
    Submarine,
    /// Lightning -> BTC receive (an outside payer pays Boltz's hold invoice;
    /// we claim Boltz's BTC lockup to one of our on-chain addresses).
    ReverseToBtc,
    /// L-BTC -> BTC receive chain swap (an outside sender locks L-BTC; we
    /// claim Boltz's BTC lockup to one of our on-chain addresses).
    ChainLbtcToBtc,
}

impl SwapKind {
    /// Our claim transaction spends a Bitcoin (not Liquid) lockup.
    pub fn claims_on_bitcoin(self) -> bool {
        matches!(self, SwapKind::ReverseToBtc | SwapKind::ChainLbtcToBtc)
    }

    /// A refund (if ever needed) spends an L-BTC lockup that an outside
    /// sender funded, so it goes back to a Liquid address.
    pub fn refunds_on_liquid(self) -> bool {
        matches!(self, SwapKind::Submarine | SwapKind::ChainLbtcToBtc)
    }
}

/// Our view of a swap. Boltz's own status is kept separately in
/// [`SwapRecord::boltz_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    /// Waiting on the payer or on Boltz.
    Pending,
    /// Submarine only: Boltz paid our invoice; waiting for Boltz to sweep.
    Paid,
    /// Our claim transaction is signed and stored; waiting for it to confirm.
    Claiming,
    /// Our BTC refund transaction is signed and stored; waiting to confirm.
    Refunding,
    /// L-BTC sits in a lockup we can refund but have no Liquid address to
    /// refund to. Needs the Boltz rescue flow (see README).
    NeedsRescue,
    /// Finished successfully.
    Complete,
    /// Finished: our locked funds came back.
    Refunded,
    /// Finished: nothing of ours is at stake (e.g. never paid, LN HTLC returned).
    Failed,
}

impl Phase {
    pub fn finished(self) -> bool {
        matches!(self, Phase::Complete | Phase::Refunded | Phase::Failed)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Pending => "pending",
            Phase::Paid => "paid",
            Phase::Claiming => "claiming",
            Phase::Refunding => "refunding",
            Phase::NeedsRescue => "refund_required",
            Phase::Complete => "complete",
            Phase::Refunded => "refunded",
            Phase::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapRecord {
    pub id: String,
    pub kind: SwapKind,
    /// Claim key for reverse / chain swaps, refund key for submarine swaps.
    pub key_index: u64,
    /// Chain swaps only: the key that can refund our BTC lockup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refund_key_index: Option<u64>,
    /// Reverse / chain swaps: sha256 of the preimage. The preimage itself is
    /// re-derived from the claim key, so it never sits on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preimage_hash: Option<String>,
    /// Unix seconds.
    pub created_at: u64,
    /// Amount the destination receives (pay bridges) / our invoice amount (receive).
    pub target_amount_sat: Sats,
    /// Amount the payer sends (our account for pay bridges, the outside sender for receive).
    pub pay_amount_sat: Sats,
    /// Absolute fee for our Liquid claim transaction. For BTC claims, the
    /// estimate quoted to the user (the real fee follows the fee rate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_fee_sat: Option<Sats>,
    /// BTC claims: fee rate (sat/vB) of the latest signed claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_fee_rate: Option<f64>,
    /// BTC claims: when the latest claim was signed (unix seconds), to bump it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_signed_at: Option<u64>,
    /// L-BTC -> BTC chain swaps: Boltz's new BTC lockup amount we accepted
    /// after the sender locked a different amount than agreed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_quote_sat: Option<Sats>,
    /// Liquid address the user is paying (pay bridges), or our own BTC
    /// address the claim pays (BTC receive bridges).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    /// BTC address our chain-swap lockup is refunded to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refund_address: Option<String>,
    /// Reverse: Boltz's invoice we pay. Submarine: our invoice Boltz pays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoice: Option<String>,
    /// Boltz's create-swap response, verbatim.
    pub boltz_response: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boltz_status: Option<String>,
    pub phase: Phase,
    /// Boltz has reported the payer's lockup transaction at least once.
    #[serde(default)]
    pub payer_locked: bool,
    /// We have handed our preimage to Boltz or the chain. From then on a
    /// chain swap must be claimed, never refunded (Boltz can take our BTC).
    #[serde(default)]
    pub preimage_revealed: bool,
    /// `txid:vout` of the lockup output our claim spends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lockup_outpoint: Option<String>,
    /// Every claim transaction we have signed (latest last).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claim_txids: Vec<String>,
    /// The latest signed claim transaction, kept for idempotent rebroadcast.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_tx_hex: Option<String>,
    /// `txid:vout` of every lockup output our refund spends.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refund_outpoints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refund_txids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refund_tx_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refund_broadcast_at: Option<u64>,
    /// Submarine: we already partial-signed Boltz's cooperative sweep.
    #[serde(default)]
    pub cooperated: bool,
    /// When we first saw the swap expire / fail (unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Store {
    pub version: u32,
    pub network: Network,
    /// Next unused swap key index. Reconciled with Boltz's restore index once
    /// per process before the first new swap, so restoring the seed on a new
    /// device never reuses a key (and so never reuses a preimage).
    pub next_index: u64,
    pub swaps: Vec<SwapRecord>,
}

impl Store {
    pub fn new(network: Network) -> Self {
        Store {
            version: STORE_VERSION,
            network,
            next_index: 0,
            swaps: Vec::new(),
        }
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(STORE_FILE)
    }

    /// Load the journal. A missing file is an empty journal; an unreadable or
    /// corrupt one is an error (never silently start over: that would forget
    /// swaps that may still hold funds).
    pub fn load(data_dir: &Path, network: Network) -> Result<Store> {
        let path = Self::path(data_dir);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::new(network)),
            Err(e) => return Err(Error::Storage(format!("read {}: {e}", path.display()))),
        };
        let store: Store = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Storage(format!("parse {}: {e}", path.display())))?;
        if store.version > STORE_VERSION {
            return Err(Error::Storage(format!(
                "{} was written by a newer version (v{}) of this app",
                path.display(),
                store.version
            )));
        }
        if store.network != network {
            return Err(Error::Network(format!(
                "swap journal is for {:?} but the wallet is on {:?}",
                store.network, network
            )));
        }
        Ok(store)
    }

    /// Atomic write: temp file in the same directory, fsync, rename, fsync dir.
    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let storage = |what: &str, e: std::io::Error| Error::Storage(format!("{what}: {e}"));
        fs::create_dir_all(data_dir).map_err(|e| storage("create data dir", e))?;
        let path = Self::path(data_dir);
        let tmp = data_dir.join(format!("{STORE_FILE}.tmp"));
        let json = serde_json::to_vec_pretty(self).map_err(|e| Error::Storage(e.to_string()))?;

        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .map_err(|e| storage("open temp journal", e))?;
        f.write_all(&json)
            .map_err(|e| storage("write temp journal", e))?;
        f.sync_all().map_err(|e| storage("sync temp journal", e))?;
        drop(f);
        fs::rename(&tmp, &path).map_err(|e| storage("replace journal", e))?;
        #[cfg(unix)]
        if let Ok(dir) = fs::File::open(data_dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&SwapRecord> {
        self.swaps.iter().find(|s| s.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut SwapRecord> {
        self.swaps.iter_mut().find(|s| s.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str) -> SwapRecord {
        SwapRecord {
            id: id.into(),
            kind: SwapKind::ChainBtcToLbtc,
            key_index: 4,
            refund_key_index: Some(5),
            preimage_hash: Some("ab".repeat(32)),
            created_at: 1_700_000_000,
            target_amount_sat: 50_000,
            pay_amount_sat: 50_321,
            claim_fee_sat: Some(40),
            claim_fee_rate: None,
            claim_signed_at: None,
            accepted_quote_sat: None,
            destination: Some("lq1qq…".into()),
            refund_address: Some("bc1q…".into()),
            invoice: None,
            boltz_response: serde_json::json!({"id": id, "claimDetails": {"amount": 50_040}}),
            boltz_status: Some("transaction.mempool".into()),
            phase: Phase::Claiming,
            payer_locked: true,
            preimage_revealed: true,
            lockup_outpoint: Some(format!("{}:1", "cd".repeat(32))),
            claim_txids: vec!["ef".repeat(32)],
            claim_tx_hex: Some("0200".into()),
            refund_outpoints: vec![],
            refund_txids: vec![],
            refund_tx_hex: None,
            refund_broadcast_at: None,
            cooperated: false,
            ended_at: None,
            last_error: Some("esplora timeout".into()),
            updated_at: 1_700_000_100,
        }
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::new(Network::Bitcoin);
        s.next_index = 6;
        s.swaps.push(record("a"));
        s.swaps.push(SwapRecord {
            kind: SwapKind::Submarine,
            phase: Phase::Pending,
            ..record("b")
        });
        s.swaps.push(SwapRecord {
            kind: SwapKind::ReverseToBtc,
            claim_fee_rate: Some(12.5),
            claim_signed_at: Some(1_700_000_050),
            ..record("c")
        });
        s.swaps.push(SwapRecord {
            kind: SwapKind::ChainLbtcToBtc,
            accepted_quote_sat: Some(49_000),
            ..record("d")
        });
        s.save(dir.path()).unwrap();
        // No temp file left behind.
        assert!(!dir.path().join(format!("{STORE_FILE}.tmp")).exists());
        let back = Store::load(dir.path(), Network::Bitcoin).unwrap();
        assert_eq!(back, s);

        // Overwrite works and stays atomic.
        let mut s2 = back.clone();
        s2.get_mut("a").unwrap().phase = Phase::Complete;
        s2.save(dir.path()).unwrap();
        assert_eq!(
            Store::load(dir.path(), Network::Bitcoin)
                .unwrap()
                .get("a")
                .unwrap()
                .phase,
            Phase::Complete
        );
    }

    #[test]
    fn missing_is_empty_but_corrupt_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Store::load(dir.path(), Network::Regtest).unwrap(),
            Store::new(Network::Regtest)
        );
        fs::write(Store::path(dir.path()), b"{ not json").unwrap();
        assert!(matches!(
            Store::load(dir.path(), Network::Regtest),
            Err(Error::Storage(_))
        ));
    }

    #[test]
    fn network_mismatch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        Store::new(Network::Regtest).save(dir.path()).unwrap();
        assert!(matches!(
            Store::load(dir.path(), Network::Bitcoin),
            Err(Error::Network(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn journal_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        Store::new(Network::Bitcoin).save(dir.path()).unwrap();
        let mode = fs::metadata(Store::path(dir.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }
}

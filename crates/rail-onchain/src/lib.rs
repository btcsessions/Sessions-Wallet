//! On-chain bitcoin accounts (BDK).
//!
//! One [`OnchainAccount`] covers every on-chain flavour:
//!
//! | flavour        | descriptors                 | who signs                          |
//! |----------------|-----------------------------|------------------------------------|
//! | hot single-sig | BIP84 from the app seed     | this device                        |
//! | hardware       | imported from the device    | the device (PSBT via QR / file)    |
//! | multisig       | `wsh(sortedmulti)`          | this device (optionally) + others  |
//! | watch-only     | any imported descriptor     | nobody                             |

pub mod descriptors;

use std::path::PathBuf;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bdk_esplora::esplora_client::{self, AsyncClient};
use bdk_esplora::EsploraAsyncExt;
use bdk_wallet::bitcoin::{Address, Amount, FeeRate, Psbt};
use bdk_wallet::chain::ChainPosition;
use bdk_wallet::file_store::Store;
use bdk_wallet::{ChangeSet, KeychainKind, PersistedWallet, SignOptions, Wallet};
use tokio::sync::Mutex;

use sessions_core::types::*;
use sessions_core::{Error, Rail, Result};

pub use bdk_wallet::bitcoin::bip32::Xpriv;
pub use descriptors::DescriptorPair;

const RAIL: &str = "on-chain";
/// File header for the wallet database. Never change once shipped.
const DB_MAGIC: &[u8] = b"sessions-wallet-onchain-v1";

type Db = Store<ChangeSet>;
const STOP_GAP: usize = 25;
const PARALLEL_REQUESTS: usize = 4;

pub fn default_esplora(network: Network) -> &'static str {
    match network {
        Network::Bitcoin => "https://mempool.space/api",
        Network::Testnet => "https://mempool.space/testnet/api",
        Network::Signet => "https://mempool.space/signet/api",
        Network::Regtest => "http://127.0.0.1:3002",
    }
}

pub struct OnchainConfig {
    pub id: AccountId,
    pub name: String,
    pub network: Network,
    pub db_path: PathBuf,
    /// Esplora base URL. Point this at your own node (Umbrel, Start9) for privacy.
    pub esplora_url: Option<String>,
    pub descriptors: DescriptorPair,
    pub custody: Custody,
    /// Master key from the app seed, when this device is one of the signers.
    pub signer: Option<Xpriv>,
}

struct Inner {
    wallet: PersistedWallet<Db>,
    conn: Db,
    /// Set after the first successful full scan; later syncs are incremental.
    scanned: bool,
}

pub struct OnchainAccount {
    id: AccountId,
    name: String,
    network: Network,
    custody: Custody,
    signer: Option<Xpriv>,
    client: AsyncClient,
    inner: Mutex<Inner>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::rail(RAIL, e)
}

impl OnchainAccount {
    pub fn open(cfg: OnchainConfig) -> Result<Self> {
        descriptors::validate(&cfg.descriptors)?;
        if let Some(parent) = cfg.db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Storage(e.to_string()))?;
        }
        let (mut conn, _) = Db::load_or_create(DB_MAGIC, &cfg.db_path).map_err(|e| Error::Storage(e.to_string()))?;
        let net = cfg.network.to_bitcoin();
        let ext = cfg.descriptors.external.clone();
        let int = cfg.descriptors.internal.clone();

        let loaded = Wallet::load()
            .descriptor(KeychainKind::External, Some(ext.clone()))
            .descriptor(KeychainKind::Internal, Some(int.clone()))
            .check_network(net)
            .load_wallet(&mut conn)
            .map_err(|e| Error::Storage(e.to_string()))?;
        let (wallet, scanned) = match loaded {
            Some(w) => (w, true),
            None => (
                Wallet::create(ext, int).network(net).create_wallet(&mut conn).map_err(|e| Error::Storage(e.to_string()))?,
                false,
            ),
        };

        let url = cfg.esplora_url.unwrap_or_else(|| default_esplora(cfg.network).to_string());
        let client = esplora_client::Builder::new(&url).build_async().map_err(err)?;

        Ok(Self {
            id: cfg.id,
            name: cfg.name,
            network: cfg.network,
            custody: cfg.custody,
            signer: cfg.signer,
            client,
            inner: Mutex::new(Inner { wallet, conn, scanned }),
        })
    }

    /// Public descriptors, for backup / exporting to a coordinator.
    pub async fn descriptors(&self) -> DescriptorPair {
        let inner = self.inner.lock().await;
        DescriptorPair {
            external: inner.wallet.public_descriptor(KeychainKind::External).to_string(),
            internal: inner.wallet.public_descriptor(KeychainKind::Internal).to_string(),
        }
    }

    async fn fee_rate(&self, priority: FeePriority) -> Result<FeeRate> {
        if let FeePriority::Custom(sat_vb) = priority {
            return FeeRate::from_sat_per_vb(sat_vb).ok_or_else(|| err("fee rate too high"));
        }
        let target = match priority {
            FeePriority::Fast => 1,
            FeePriority::Normal => 6,
            FeePriority::Slow => 144,
            FeePriority::Custom(_) => unreachable!(),
        };
        let estimates = self.client.get_fee_estimates().await.map_err(err)?;
        let sat_vb = esplora_client::convert_fee_rate(target, estimates).unwrap_or(2.0).max(1.0);
        // Round up to the nearest 0.25 sat/vB expressed as sat/kwu.
        Ok(FeeRate::from_sat_per_kwu(((sat_vb as f64) * 250.0).ceil() as u64))
    }

    /// Current fee rates in sat/vB for (next block, ~1 hour, no rush), from
    /// this account's chain server.
    pub async fn fee_rates(&self) -> Result<(f32, f32, f32)> {
        let est = self.client.get_fee_estimates().await.map_err(err)?;
        let at = |t| esplora_client::convert_fee_rate(t, est.clone()).unwrap_or(1.0).max(1.0);
        Ok((at(1), at(6), at(144)))
    }

    fn signatures_needed(&self) -> u8 {
        match &self.custody {
            Custody::Hot => 0,
            Custody::Hardware { .. } | Custody::WatchOnly => 1,
            Custody::Multisig { threshold, local_signers, .. } => threshold.saturating_sub(*local_signers),
            Custody::Mint { .. } => 1,
        }
    }

    fn sign_locally(&self, psbt: &mut Psbt) -> Result<()> {
        if let Some(xprv) = &self.signer {
            let secp = bdk_wallet::bitcoin::secp256k1::Secp256k1::new();
            // Partial success is normal for multisig: we only sign our inputs' keys.
            let _ = psbt.sign(xprv, &secp);
        }
        Ok(())
    }

    fn psbt_from_b64(s: &str) -> Result<Psbt> {
        Psbt::from_str(s.trim()).map_err(|e| err(format!("invalid PSBT: {e}")))
    }
}

#[async_trait]
impl Rail for OnchainAccount {
    fn id(&self) -> &AccountId {
        &self.id
    }

    fn kind(&self) -> RailKind {
        RailKind::Onchain
    }

    async fn info(&self) -> Result<AccountInfo> {
        let inner = self.inner.lock().await;
        let b = inner.wallet.balance();
        // Pending outgoing = unconfirmed txs where we're net senders.
        let pending_out: u64 = inner
            .wallet
            .transactions()
            .filter(|t| matches!(t.chain_position, ChainPosition::Unconfirmed { .. }))
            .map(|t| {
                let (sent, received) = inner.wallet.sent_and_received(&t.tx_node.tx);
                sent.to_sat().saturating_sub(received.to_sat())
            })
            .sum();
        Ok(AccountInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            rail: RailKind::Onchain,
            custody: self.custody.clone(),
            balance: Balance {
                confirmed: (b.confirmed + b.trusted_pending).to_sat(),
                pending_in: (b.untrusted_pending + b.immature).to_sat(),
                pending_out,
            },
            receives: vec![Method::BitcoinAddress],
            pays: if matches!(self.custody, Custody::WatchOnly) { vec![] } else { vec![Method::BitcoinAddress] },
        })
    }

    async fn sync(&self) -> Result<()> {
        let (request_full, request_sync) = {
            let inner = self.inner.lock().await;
            if inner.scanned {
                (None, Some(inner.wallet.start_sync_with_revealed_spks().build()))
            } else {
                (Some(inner.wallet.start_full_scan().build()), None)
            }
        };

        if let Some(req) = request_full {
            let update = self.client.full_scan(req, STOP_GAP, PARALLEL_REQUESTS).await.map_err(err)?;
            let mut inner = self.inner.lock().await;
            inner.wallet.apply_update(update).map_err(err)?;
            inner.scanned = true;
            let Inner { wallet, conn, .. } = &mut *inner;
            wallet.persist(conn).map_err(|e| Error::Storage(e.to_string()))?;
        }
        if let Some(req) = request_sync {
            let update = self.client.sync(req, PARALLEL_REQUESTS).await.map_err(err)?;
            let mut inner = self.inner.lock().await;
            inner.wallet.apply_update(update).map_err(err)?;
            let Inner { wallet, conn, .. } = &mut *inner;
            wallet.persist(conn).map_err(|e| Error::Storage(e.to_string()))?;
        }
        Ok(())
    }

    async fn receive(&self, req: ReceiveRequest) -> Result<ReceiveTarget> {
        if req.method != Method::BitcoinAddress {
            return Err(Error::Unsupported(format!("receive via {:?} on-chain", req.method)));
        }
        let mut inner = self.inner.lock().await;
        let Inner { wallet, conn, .. } = &mut *inner;
        // Reuse the first unused address so repeated taps don't burn the gap limit.
        let info = wallet.next_unused_address(KeychainKind::External);
        wallet.persist(conn).map_err(|e| Error::Storage(e.to_string()))?;

        let mut uri = format!("bitcoin:{}", info.address.to_string().to_uppercase());
        if let Some(sats) = req.amount_sat {
            uri.push_str(&format!("?amount={}", Amount::from_sat(sats).to_btc()));
        }
        Ok(ReceiveTarget {
            method: Method::BitcoinAddress,
            request: if req.amount_sat.is_some() { uri } else { info.address.to_string() },
            amount_sat: req.amount_sat,
            fee_sat: None,
            expires_at: None,
            watch_id: Some(info.address.to_string()),
        })
    }

    async fn prepare_send(&self, req: SendRequest) -> Result<SendQuote> {
        let PaymentRequest::BitcoinAddress { address, amount_sat: req_amount, .. } = &req.destination else {
            return Err(Error::Unsupported("pay that from an on-chain account".into()));
        };
        if matches!(self.custody, Custody::WatchOnly) {
            return Err(Error::Unsupported("spend from a watch-only account".into()));
        }
        let address = Address::from_str(address)
            .map_err(err)?
            .require_network(self.network.to_bitcoin())
            .map_err(|_| Error::Network("that address is for a different network".into()))?;
        let fee_rate = self.fee_rate(req.fee).await?;

        let mut inner = self.inner.lock().await;
        let Inner { wallet, conn, .. } = &mut *inner;
        let available = wallet.balance().trusted_spendable().to_sat();

        let mut psbt = {
            let mut b = wallet.build_tx();
            b.fee_rate(fee_rate);
            if req.send_max {
                b.drain_wallet().drain_to(address.script_pubkey());
            } else {
                let amount = req.amount_sat.or(*req_amount).ok_or(Error::AmountRequired)?;
                b.add_recipient(address.script_pubkey(), Amount::from_sat(amount));
            }
            b.finish().map_err(|e| match e {
                bdk_wallet::error::CreateTxError::CoinSelection(_) => Error::InsufficientFunds {
                    needed: req.amount_sat.or(*req_amount).unwrap_or(available),
                    available,
                },
                other => err(other),
            })?
        };
        // Persist so the change address we just used isn't handed out again.
        wallet.persist(conn).map_err(|e| Error::Storage(e.to_string()))?;

        let fee_sat = psbt.fee().map_err(err)?.to_sat();
        let spk = address.script_pubkey();
        let amount_sat: u64 =
            psbt.unsigned_tx.output.iter().filter(|o| o.script_pubkey == spk).map(|o| o.value.to_sat()).sum();

        let needed = self.signatures_needed();
        let needs_signature = if needed > 0 {
            // Multisig: add our own signature first so the PSBT shown to the
            // user only needs the remaining cosigners.
            self.sign_locally(&mut psbt)?;
            Some(UnsignedPsbt { psbt_base64: psbt.to_string(), signatures_needed: needed })
        } else {
            None
        };

        Ok(SendQuote {
            account: self.id.clone(),
            method: Method::BitcoinAddress,
            amount_sat,
            fee_sat,
            handle: psbt.to_string(),
            needs_signature,
        })
    }

    async fn send(&self, quote: SendQuote, signed_psbt: Option<String>) -> Result<Payment> {
        let mut psbt = Self::psbt_from_b64(&quote.handle)?;
        if let Some(signed) = signed_psbt {
            let other = Self::psbt_from_b64(&signed)?;
            if other.unsigned_tx.compute_txid() != psbt.unsigned_tx.compute_txid() {
                return Err(err("the signed PSBT is for a different transaction"));
            }
            psbt.combine(other).map_err(err)?;
        }
        self.sign_locally(&mut psbt)?;

        let tx = {
            let inner = self.inner.lock().await;
            let finalized = inner.wallet.finalize_psbt(&mut psbt, SignOptions::default()).map_err(err)?;
            if !finalized {
                return Err(Error::Unsupported("broadcast yet – signatures are still missing".into()));
            }
            psbt.extract_tx().map_err(err)?
        };

        self.client.broadcast(&tx).await.map_err(err)?;

        let mut inner = self.inner.lock().await;
        let Inner { wallet, conn, .. } = &mut *inner;
        wallet.apply_unconfirmed_txs([(tx.clone(), now())]);
        wallet.persist(conn).map_err(|e| Error::Storage(e.to_string()))?;

        Ok(Payment {
            id: tx.compute_txid().to_string(),
            account: self.id.clone(),
            direction: Direction::Outgoing,
            amount_sat: quote.amount_sat,
            fee_sat: quote.fee_sat,
            status: PaymentStatus::Pending,
            method: Method::BitcoinAddress,
            timestamp: now(),
            description: None,
            token: None,
        })
    }

    async fn payments(&self, limit: usize) -> Result<Vec<Payment>> {
        let inner = self.inner.lock().await;
        let w = &inner.wallet;
        let mut out: Vec<Payment> = w
            .transactions()
            .map(|t| {
                let tx = &t.tx_node.tx;
                let (sent, received) = w.sent_and_received(tx);
                let fee = w.calculate_fee(tx).map(|a| a.to_sat()).unwrap_or(0);
                let (sent, received) = (sent.to_sat(), received.to_sat());
                let (direction, amount) = if sent > received {
                    (Direction::Outgoing, sent - received - fee.min(sent - received))
                } else {
                    (Direction::Incoming, received - sent)
                };
                let (status, timestamp) = match &t.chain_position {
                    ChainPosition::Confirmed { anchor, .. } => (PaymentStatus::Complete, anchor.confirmation_time),
                    ChainPosition::Unconfirmed { first_seen, last_seen } => {
                        (PaymentStatus::Pending, first_seen.or(*last_seen).unwrap_or_else(now))
                    }
                };
                Payment {
                    id: t.tx_node.txid.to_string(),
                    account: self.id.clone(),
                    direction,
                    amount_sat: amount,
                    fee_sat: if direction == Direction::Outgoing { fee } else { 0 },
                    status,
                    method: Method::BitcoinAddress,
                    timestamp,
                    description: None,
                    token: None,
                }
            })
            .collect();
        out.sort_by_key(|p| std::cmp::Reverse(p.timestamp));
        out.truncate(limit);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessions_core::keystore::Seed;

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn hot(dir: &std::path::Path) -> OnchainAccount {
        let master = Seed::from_phrase(ABANDON).unwrap().master_xpriv(Network::Bitcoin).unwrap();
        OnchainAccount::open(OnchainConfig {
            id: "savings".into(),
            name: "Savings".into(),
            network: Network::Bitcoin,
            db_path: dir.join("savings.db"),
            esplora_url: None,
            descriptors: descriptors::hot_single_sig(&master, Network::Bitcoin, 0).unwrap(),
            custody: Custody::Hot,
            signer: Some(master),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn first_address_matches_bip84_vector_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        let r = a.receive(ReceiveRequest { method: Method::BitcoinAddress, amount_sat: None, description: None }).await.unwrap();
        // BIP84 test vector: m/84'/0'/0'/0/0 for "abandon … about".
        assert_eq!(r.request, "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu");
        drop(a);

        // Reopen from disk: same wallet, unused address is reused.
        let a = hot(dir.path());
        let r2 = a.receive(ReceiveRequest { method: Method::BitcoinAddress, amount_sat: Some(12_345), description: None }).await.unwrap();
        assert_eq!(r2.request, "bitcoin:BC1QCR8TE4KR609GCAWUTMRZA0J4XV80JY8Z306FYU?amount=0.00012345");
        assert_eq!(a.info().await.unwrap().balance, Balance::default());
    }

    #[tokio::test]
    async fn rejects_other_methods_and_networks() {
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        assert!(a.receive(ReceiveRequest { method: Method::Bolt11, amount_sat: None, description: None }).await.is_err());
        let q = a
            .prepare_send(SendRequest {
                destination: PaymentRequest::BitcoinAddress {
                    address: "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx".into(),
                    amount_sat: Some(1000),
                    label: None,
                },
                amount_sat: None,
                send_max: false,
                fee: FeePriority::Custom(2),
                note: None,
            })
            .await;
        assert!(matches!(q, Err(Error::Network(_))));
    }

    /// Give a wallet a fake unconfirmed coin paying its first receive address.
    async fn fund(a: &OnchainAccount, sats: u64) {
        use bdk_wallet::bitcoin::hashes::Hash;
        use bdk_wallet::bitcoin::{absolute, transaction, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};
        let mut inner = a.inner.lock().await;
        let spk = inner.wallet.peek_address(KeychainKind::External, 0).script_pubkey();
        let tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                // Any non-null outpoint: a null one would make this a coinbase.
                previous_output: OutPoint { txid: Txid::from_byte_array([7; 32]), vout: 0 },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut { value: Amount::from_sat(sats), script_pubkey: spk }],
        };
        inner.wallet.apply_unconfirmed_txs([(tx, now())]);
    }

    fn send_req(sats: u64) -> SendRequest {
        SendRequest {
            destination: PaymentRequest::BitcoinAddress {
                address: "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq".into(),
                amount_sat: None,
                label: None,
            },
            amount_sat: Some(sats),
            send_max: false,
            fee: FeePriority::Custom(3),
            note: None,
        }
    }

    async fn finalizes(a: &OnchainAccount, psbt_b64: &str) -> bool {
        let mut psbt = OnchainAccount::psbt_from_b64(psbt_b64).unwrap();
        a.sign_locally(&mut psbt).unwrap();
        a.inner.lock().await.wallet.finalize_psbt(&mut psbt, SignOptions::default()).unwrap()
    }

    #[tokio::test]
    async fn hot_wallet_signs_and_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        fund(&a, 100_000).await;
        let q = a.prepare_send(send_req(40_000)).await.unwrap();
        assert_eq!(q.amount_sat, 40_000);
        assert!(q.fee_sat > 0 && q.fee_sat < 2_000, "fee {}", q.fee_sat);
        assert!(q.needs_signature.is_none());
        assert!(finalizes(&a, &q.handle).await);
    }

    #[tokio::test]
    async fn insufficient_funds_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        fund(&a, 10_000).await;
        assert!(matches!(a.prepare_send(send_req(50_000)).await, Err(Error::InsufficientFunds { .. })));
    }

    #[tokio::test]
    async fn two_of_two_multisig_needs_and_accepts_cosigner() {
        let dir = tempfile::tempdir().unwrap();
        let ours = Seed::from_phrase(ABANDON).unwrap().master_xpriv(Network::Bitcoin).unwrap();
        let theirs = Seed::generate(12).unwrap().master_xpriv(Network::Bitcoin).unwrap();
        let keys = [
            descriptors::multisig_cosigner_key(&ours, Network::Bitcoin, 0).unwrap(),
            descriptors::multisig_cosigner_key(&theirs, Network::Bitcoin, 0).unwrap(),
        ];
        let pair = descriptors::multisig(2, &keys).unwrap();
        let open = |name: &str, signer: Xpriv| {
            OnchainAccount::open(OnchainConfig {
                id: name.into(),
                name: name.into(),
                network: Network::Bitcoin,
                db_path: dir.path().join(format!("{name}.db")),
                esplora_url: None,
                descriptors: pair.clone(),
                custody: Custody::Multisig { threshold: 2, total: 2, local_signers: 1 },
                signer: Some(signer),
            })
            .unwrap()
        };
        let a = open("vault", ours);
        let cosigner = open("cosigner", theirs);
        fund(&a, 200_000).await;

        let q = a.prepare_send(send_req(50_000)).await.unwrap();
        let unsigned = q.needs_signature.clone().expect("needs cosigner");
        assert_eq!(unsigned.signatures_needed, 1);
        // Our signature alone isn't enough.
        assert!(!finalizes(&a, &q.handle).await);

        // The cosigner (e.g. a hardware wallet) signs the PSBT we exported.
        let mut theirs_psbt = OnchainAccount::psbt_from_b64(&unsigned.psbt_base64).unwrap();
        cosigner.sign_locally(&mut theirs_psbt).unwrap();

        let mut combined = OnchainAccount::psbt_from_b64(&q.handle).unwrap();
        combined.combine(theirs_psbt).unwrap();
        assert!(finalizes(&a, &combined.to_string()).await);
    }

    #[test]
    fn signatures_needed_by_custody() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = hot(dir.path());
        assert_eq!(a.signatures_needed(), 0);
        a.custody = Custody::Multisig { threshold: 2, total: 3, local_signers: 1 };
        assert_eq!(a.signatures_needed(), 1);
        a.custody = Custody::Hardware { device: "Coldcard Q".into(), fingerprint: "73c5da0a".into() };
        assert_eq!(a.signatures_needed(), 1);
    }
}

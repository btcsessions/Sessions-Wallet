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

mod bump;
pub mod descriptors;
pub mod payjoin;
pub mod privacy;
pub mod trezor;

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
pub use payjoin::{PayjoinEndpoints, PjVersion, DEFAULT_DIRECTORY, DEFAULT_RELAYS};

const RAIL: &str = "on-chain";
/// File header for the wallet database. Never change once shipped.
const DB_MAGIC: &[u8] = b"sessions-wallet-onchain-v1";

type Db = Store<ChangeSet>;
const STOP_GAP: usize = 25;
const PARALLEL_REQUESTS: usize = 4;

fn encode_handle(psbt: &Psbt, payjoin_uri: Option<&str>) -> String {
    match payjoin_uri {
        Some(uri) => format!("{psbt}\n{uri}"),
        None => psbt.to_string(),
    }
}

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
    /// Offer payjoin (BIP77) when receiving. Off by default; only honoured
    /// for `Custody::Hot` and when `payjoin_dir` is set.
    pub payjoin_receive: bool,
    /// Where payjoin receive sessions are stored.
    pub payjoin_dir: Option<PathBuf>,
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
    pj: payjoin::PayjoinState,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
        let (mut conn, _) = Db::load_or_create(DB_MAGIC, &cfg.db_path)
            .map_err(|e| Error::Storage(e.to_string()))?;
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
                Wallet::create(ext, int)
                    .network(net)
                    .create_wallet(&mut conn)
                    .map_err(|e| Error::Storage(e.to_string()))?,
                false,
            ),
        };

        let url = cfg
            .esplora_url
            .unwrap_or_else(|| default_esplora(cfg.network).to_string());
        let client = esplora_client::Builder::new(&url)
            .build_async()
            .map_err(err)?;

        let pj = payjoin::PayjoinState::new(&cfg.custody, cfg.payjoin_receive, cfg.payjoin_dir);
        Ok(Self {
            pj,
            id: cfg.id,
            name: cfg.name,
            network: cfg.network,
            custody: cfg.custody,
            signer: cfg.signer,
            client,
            inner: Mutex::new(Inner {
                wallet,
                conn,
                scanned,
            }),
        })
    }

    /// Turn payjoin receiving on or off without reopening the account.
    /// Sessions already in progress are still finished by `payjoin_tick`.
    /// Returns whether it's now on (always off for non-hot accounts).
    pub fn set_payjoin_receive(&self, on: bool) -> bool {
        let on = on && self.pj.receive_dir.is_some();
        self.pj
            .receive_on
            .store(on, std::sync::atomic::Ordering::SeqCst);
        on
    }

    /// Public descriptors, for backup / exporting to a coordinator.
    pub async fn descriptors(&self) -> DescriptorPair {
        let inner = self.inner.lock().await;
        DescriptorPair {
            external: inner
                .wallet
                .public_descriptor(KeychainKind::External)
                .to_string(),
            internal: inner
                .wallet
                .public_descriptor(KeychainKind::Internal)
                .to_string(),
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
        let sat_vb = esplora_client::convert_fee_rate(target, estimates)
            .unwrap_or(2.0)
            .max(1.0);
        // Round up to the nearest 0.25 sat/vB expressed as sat/kwu.
        Ok(FeeRate::from_sat_per_kwu(
            ((sat_vb as f64) * 250.0).ceil() as u64
        ))
    }

    /// Current fee rates in sat/vB for (next block, ~1 hour, no rush), from
    /// this account's chain server.
    pub async fn fee_rates(&self) -> Result<(f32, f32, f32)> {
        let est = self.client.get_fee_estimates().await.map_err(err)?;
        let at = |t| {
            esplora_client::convert_fee_rate(t, est.clone())
                .unwrap_or(1.0)
                .max(1.0)
        };
        Ok((at(1), at(6), at(144)))
    }

    fn signatures_needed(&self) -> u8 {
        match &self.custody {
            Custody::Hot => 0,
            Custody::Hardware { .. } | Custody::WatchOnly => 1,
            Custody::Multisig {
                threshold,
                local_signers,
                ..
            } => threshold.saturating_sub(*local_signers),
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

    /// The quote handle: the PSBT, plus the payjoin URI when we'll try payjoin.
    fn decode_handle(handle: &str) -> Result<(Psbt, Option<String>)> {
        match handle.split_once('\n') {
            Some((psbt, uri)) => Ok((Self::psbt_from_b64(psbt)?, Some(uri.to_string()))),
            None => Ok((Self::psbt_from_b64(handle)?, None)),
        }
    }

    fn psbt_from_b64(s: &str) -> Result<Psbt> {
        Psbt::from_str(s.trim()).map_err(|e| err(format!("invalid PSBT: {e}")))
    }

    /// Wrap a built PSBT as a quote. Accounts that need outside signatures
    /// get `needs_signature`, with our own signature already added.
    fn quote(
        &self,
        mut psbt: Psbt,
        amount_sat: u64,
        notes: Vec<String>,
        payjoin_uri: Option<&str>,
    ) -> Result<SendQuote> {
        let fee_sat = psbt.fee().map_err(err)?.to_sat();
        let needed = self.signatures_needed();
        let needs_signature = if needed > 0 {
            // Multisig: add our own signature first so the PSBT shown to the
            // user only needs the remaining cosigners.
            self.sign_locally(&mut psbt)?;
            Some(UnsignedPsbt {
                psbt_base64: psbt.to_string(),
                signatures_needed: needed,
            })
        } else {
            None
        };
        Ok(SendQuote {
            account: self.id.clone(),
            method: Method::BitcoinAddress,
            amount_sat,
            fee_sat,
            handle: encode_handle(&psbt, payjoin_uri),
            needs_signature,
            private: false,
            payjoin: payjoin_uri.is_some(),
            notes,
        })
    }

    /// Merge outside signatures, add ours and finalize. Returns the
    /// finalized PSBT and the same PSBT just before finalizing.
    async fn sign_and_finalize(
        &self,
        mut psbt: Psbt,
        signed_psbt: Option<String>,
    ) -> Result<(Psbt, Psbt)> {
        if let Some(signed) = signed_psbt {
            let other = Self::psbt_from_b64(&signed)?;
            if other.unsigned_tx.compute_txid() != psbt.unsigned_tx.compute_txid() {
                return Err(err("the signed PSBT is for a different transaction"));
            }
            psbt.combine(other).map_err(err)?;
        }
        self.sign_locally(&mut psbt)?;
        let unfinalized = psbt.clone();
        let inner = self.inner.lock().await;
        let finalized = inner
            .wallet
            .finalize_psbt(&mut psbt, SignOptions::default())
            .map_err(err)?;
        if !finalized {
            return Err(Error::Unsupported(
                "broadcast yet – signatures are still missing".into(),
            ));
        }
        Ok((psbt, unfinalized))
    }
}

/// Add a transaction we just broadcast to the wallet. Anything it replaces
/// (an RBF speed-up or cancel) drops out of the history and balance.
fn record_tx(wallet: &mut Wallet, tx: bdk_wallet::bitcoin::Transaction) {
    let txid = tx.compute_txid();
    let spends: std::collections::HashSet<_> = tx.input.iter().map(|i| i.previous_output).collect();
    let mut latest = 0;
    let replaced: Vec<_> = wallet
        .transactions()
        .filter(|t| t.tx_node.txid != txid)
        .filter(|t| {
            t.tx_node
                .tx
                .input
                .iter()
                .any(|i| spends.contains(&i.previous_output))
        })
        .filter_map(|t| match t.chain_position {
            ChainPosition::Unconfirmed {
                first_seen,
                last_seen,
            } => {
                latest = latest.max(last_seen.or(first_seen).unwrap_or(0));
                Some(t.tx_node.txid)
            }
            ChainPosition::Confirmed { .. } => None,
        })
        .collect();
    // Newer than whatever it replaces, so it wins BDK's conflict resolution
    // even within the same second.
    let seen = now().max(latest + 1);
    wallet.apply_evicted_txs(replaced.into_iter().map(|t| (t, seen)));
    wallet.apply_unconfirmed_txs([(tx, seen)]);
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
            pays: if matches!(self.custody, Custody::WatchOnly) {
                vec![]
            } else {
                vec![Method::BitcoinAddress]
            },
        })
    }

    async fn sync(&self) -> Result<()> {
        let (request_full, request_sync) = {
            let inner = self.inner.lock().await;
            if inner.scanned {
                (
                    None,
                    Some(inner.wallet.start_sync_with_revealed_spks().build()),
                )
            } else {
                (Some(inner.wallet.start_full_scan().build()), None)
            }
        };

        if let Some(req) = request_full {
            let update = self
                .client
                .full_scan(req, STOP_GAP, PARALLEL_REQUESTS)
                .await
                .map_err(err)?;
            let mut inner = self.inner.lock().await;
            inner.wallet.apply_update(update).map_err(err)?;
            inner.scanned = true;
            let Inner { wallet, conn, .. } = &mut *inner;
            wallet
                .persist(conn)
                .map_err(|e| Error::Storage(e.to_string()))?;
        }
        if let Some(req) = request_sync {
            let update = self
                .client
                .sync(req, PARALLEL_REQUESTS)
                .await
                .map_err(err)?;
            let mut inner = self.inner.lock().await;
            inner.wallet.apply_update(update).map_err(err)?;
            let Inner { wallet, conn, .. } = &mut *inner;
            wallet
                .persist(conn)
                .map_err(|e| Error::Storage(e.to_string()))?;
        }
        Ok(())
    }

    async fn receive(&self, req: ReceiveRequest) -> Result<ReceiveTarget> {
        if req.method != Method::BitcoinAddress {
            return Err(Error::Unsupported(format!(
                "receive via {:?} on-chain",
                req.method
            )));
        }
        let info = {
            let mut inner = self.inner.lock().await;
            let Inner { wallet, conn, .. } = &mut *inner;
            // Reuse the first unused address so repeated taps don't burn the gap limit.
            let info = wallet.next_unused_address(KeychainKind::External);
            wallet
                .persist(conn)
                .map_err(|e| Error::Storage(e.to_string()))?;
            info
        };

        // Payjoin receive (opt-in, hot only). Falls back to a plain request.
        if let Some(pj_uri) = self
            .payjoin_receive_uri(&info.address, req.amount_sat)
            .await
        {
            return Ok(ReceiveTarget {
                method: Method::BitcoinAddress,
                request: pj_uri,
                amount_sat: req.amount_sat,
                fee_sat: None,
                expires_at: None,
                watch_id: Some(info.address.to_string()),
            });
        }

        let mut uri = format!("bitcoin:{}", info.address.to_string().to_uppercase());
        if let Some(sats) = req.amount_sat {
            uri.push_str(&format!("?amount={}", Amount::from_sat(sats).to_btc()));
        }
        Ok(ReceiveTarget {
            method: Method::BitcoinAddress,
            request: if req.amount_sat.is_some() {
                uri
            } else {
                info.address.to_string()
            },
            amount_sat: req.amount_sat,
            fee_sat: None,
            expires_at: None,
            watch_id: Some(info.address.to_string()),
        })
    }

    async fn prepare_send(&self, req: SendRequest) -> Result<SendQuote> {
        let PaymentRequest::BitcoinAddress {
            address,
            amount_sat: req_amount,
            payjoin: payjoin_uri,
            ..
        } = &req.destination
        else {
            return Err(Error::Unsupported(
                "pay that from an on-chain account".into(),
            ));
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
        // Unconfirmed incoming coins are spendable too (0-conf).
        let bal = wallet.balance();
        let available = (bal.trusted_spendable() + bal.untrusted_pending).to_sat();

        let private = req.style == SpendStyle::Private;
        let mut notes = Vec::new();
        let psbt = if private {
            if req.send_max {
                return Err(Error::Invalid("Private mode can't send the whole balance, since the decoy needs a matching amount. Use Efficient".into()));
            }
            let amount = req
                .amount_sat
                .or(*req_amount)
                .ok_or(Error::AmountRequired)?;
            let utxos = privacy::spendable(wallet);
            let preferred = privacy::settled(&utxos);
            let input_weight = privacy::input_weight(wallet)?;
            let fresh = [
                wallet
                    .reveal_next_address(KeychainKind::Internal)
                    .script_pubkey(),
                wallet
                    .reveal_next_address(KeychainKind::Internal)
                    .script_pubkey(),
                wallet
                    .reveal_next_address(KeychainKind::Internal)
                    .script_pubkey(),
            ];
            let dest = address.script_pubkey();
            // Confirmed coins and our own change first; fall back to also
            // using unconfirmed incoming coins.
            let plan = match privacy::plan(
                &preferred,
                input_weight,
                &dest,
                amount,
                fee_rate,
                fresh.clone(),
            ) {
                Ok(p) => p,
                Err(_) => privacy::plan(&utxos, input_weight, &dest, amount, fee_rate, fresh)?,
            };
            notes.extend(plan.notes);
            let mut b = wallet.build_tx();
            b.add_utxos(&plan.inputs).map_err(err)?;
            b.manually_selected_only();
            for (spk, value) in plan.outputs {
                b.add_recipient(spk, Amount::from_sat(value));
            }
            b.fee_absolute(Amount::from_sat(plan.fee));
            b.finish().map_err(err)?
        } else {
            let amount = if req.send_max {
                None
            } else {
                Some(
                    req.amount_sat
                        .or(*req_amount)
                        .ok_or(Error::AmountRequired)?,
                )
            };
            // Unconfirmed incoming coins (not our own change) are held back
            // on the first try, so they're only used when needed.
            let unsettled: Vec<_> = wallet
                .list_unspent()
                .filter(|u| {
                    !u.chain_position.is_confirmed() && u.keychain == KeychainKind::External
                })
                .map(|u| u.outpoint)
                .collect();
            let tiers: &[bool] = match (amount, unsettled.is_empty()) {
                (Some(_), false) => &[true, false],
                _ => &[false],
            };
            let mut result = None;
            for &hold_back in tiers {
                let mut b = wallet.build_tx();
                b.fee_rate(fee_rate);
                if hold_back {
                    b.unspendable(unsettled.clone());
                }
                match amount {
                    None => {
                        b.drain_wallet().drain_to(address.script_pubkey());
                    }
                    Some(sats) => {
                        b.add_recipient(address.script_pubkey(), Amount::from_sat(sats));
                    }
                }
                result = Some(b.finish());
                if matches!(result, Some(Ok(_))) {
                    break;
                }
            }
            result.expect("at least one attempt").map_err(|e| match e {
                bdk_wallet::error::CreateTxError::CoinSelection(_) => Error::InsufficientFunds {
                    needed: amount.unwrap_or(available),
                    available,
                },
                other => err(other),
            })?
        };

        // Payjoin (sender): hot wallets only, and not combined with private
        // mode, since the receiver rebuilds the transaction anyway.
        let payjoin = payjoin_uri.is_some()
            && matches!(self.custody, Custody::Hot)
            && !private
            && !req.send_max;
        if payjoin {
            notes.push("The recipient supports payjoin. The wallet will try it first and send this transaction if they don't respond.".into());
        }

        // Persist so the change address we just used isn't handed out again.
        wallet
            .persist(conn)
            .map_err(|e| Error::Storage(e.to_string()))?;

        let spk = address.script_pubkey();
        let amount_sat: u64 = psbt
            .unsigned_tx
            .output
            .iter()
            .filter(|o| o.script_pubkey == spk)
            .map(|o| o.value.to_sat())
            .sum();

        let mut quote = self.quote(
            psbt,
            amount_sat,
            notes,
            payjoin_uri.as_deref().filter(|_| payjoin),
        )?;
        quote.private = private;
        Ok(quote)
    }

    async fn send(&self, quote: SendQuote, signed_psbt: Option<String>) -> Result<Payment> {
        let (psbt, payjoin_uri) = Self::decode_handle(&quote.handle)?;
        let payjoin_uri =
            payjoin_uri.filter(|_| signed_psbt.is_none() && matches!(self.custody, Custody::Hot));
        let (psbt, unfinalized) = self.sign_and_finalize(psbt, signed_psbt).await?;
        let original = psbt.clone().extract_tx().map_err(err)?;

        // Payjoin first; any problem means the Original goes out instead.
        let (tx, used_payjoin, extra_fee) = match &payjoin_uri {
            Some(uri) => {
                let choice = self
                    .payjoin_or_original(&psbt, &unfinalized, uri, self.pj.http.as_ref())
                    .await;
                if choice.payjoin {
                    match self.broadcast_idempotent(&choice.tx).await {
                        Ok(()) => (choice.tx, true, choice.extra_fee_sat),
                        Err(e) => {
                            tracing::info!("payjoin broadcast failed ({e}); sending the original");
                            self.broadcast_idempotent(&original).await?;
                            (original, false, 0)
                        }
                    }
                } else {
                    self.broadcast_idempotent(&original).await?;
                    (original, false, 0)
                }
            }
            None => {
                self.client.broadcast(&original).await.map_err(err)?;
                (original, false, 0)
            }
        };
        if payjoin_uri.is_some() {
            tracing::info!(
                "on-chain send {} {}",
                tx.compute_txid(),
                if used_payjoin {
                    "used payjoin"
                } else {
                    "fell back to a normal transaction"
                }
            );
        }

        let mut inner = self.inner.lock().await;
        let Inner { wallet, conn, .. } = &mut *inner;
        record_tx(wallet, tx.clone());
        wallet
            .persist(conn)
            .map_err(|e| Error::Storage(e.to_string()))?;

        Ok(Payment {
            id: tx.compute_txid().to_string(),
            account: self.id.clone(),
            direction: Direction::Outgoing,
            amount_sat: quote.amount_sat,
            fee_sat: quote.fee_sat + extra_fee,
            status: PaymentStatus::Pending,
            method: Method::BitcoinAddress,
            timestamp: now(),
            description: match (payjoin_uri.is_some(), used_payjoin) {
                (_, true) => Some(payjoin::DESCRIPTION_PAYJOIN.into()),
                (true, false) => Some(payjoin::DESCRIPTION_FALLBACK.into()),
                _ => None,
            },
            token: None,
        })
    }

    async fn tx_details(&self, txid: &str) -> Result<TxDetails> {
        self.details(txid).await
    }

    async fn prepare_bump(
        &self,
        txid: &str,
        action: BumpAction,
        fee: FeePriority,
    ) -> Result<SendQuote> {
        self.bump(txid, action, fee).await
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
                    (
                        Direction::Outgoing,
                        sent - received - fee.min(sent - received),
                    )
                } else {
                    (Direction::Incoming, received - sent)
                };
                let (status, timestamp) = match &t.chain_position {
                    ChainPosition::Confirmed { anchor, .. } => {
                        (PaymentStatus::Complete, anchor.confirmation_time)
                    }
                    ChainPosition::Unconfirmed {
                        first_seen,
                        last_seen,
                    } => (
                        PaymentStatus::Pending,
                        first_seen.or(*last_seen).unwrap_or_else(now),
                    ),
                };
                Payment {
                    id: t.tx_node.txid.to_string(),
                    account: self.id.clone(),
                    direction,
                    amount_sat: amount,
                    fee_sat: if direction == Direction::Outgoing {
                        fee
                    } else {
                        0
                    },
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
        let master = Seed::from_phrase(ABANDON)
            .unwrap()
            .master_xpriv(Network::Bitcoin)
            .unwrap();
        OnchainAccount::open(OnchainConfig {
            id: "savings".into(),
            name: "Savings".into(),
            network: Network::Bitcoin,
            db_path: dir.join("savings.db"),
            esplora_url: None,
            descriptors: descriptors::hot_single_sig(&master, Network::Bitcoin, 0).unwrap(),
            custody: Custody::Hot,
            signer: Some(master),
            payjoin_receive: false,
            payjoin_dir: None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn first_address_matches_bip84_vector_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        let r = a
            .receive(ReceiveRequest {
                method: Method::BitcoinAddress,
                amount_sat: None,
                description: None,
            })
            .await
            .unwrap();
        // BIP84 test vector: m/84'/0'/0'/0/0 for "abandon … about".
        assert_eq!(r.request, "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu");
        drop(a);

        // Reopen from disk: same wallet, unused address is reused.
        let a = hot(dir.path());
        let r2 = a
            .receive(ReceiveRequest {
                method: Method::BitcoinAddress,
                amount_sat: Some(12_345),
                description: None,
            })
            .await
            .unwrap();
        assert_eq!(
            r2.request,
            "bitcoin:BC1QCR8TE4KR609GCAWUTMRZA0J4XV80JY8Z306FYU?amount=0.00012345"
        );
        assert_eq!(a.info().await.unwrap().balance, Balance::default());
    }

    #[tokio::test]
    async fn rejects_other_methods_and_networks() {
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        assert!(a
            .receive(ReceiveRequest {
                method: Method::Bolt11,
                amount_sat: None,
                description: None
            })
            .await
            .is_err());
        let q = a
            .prepare_send(SendRequest {
                destination: PaymentRequest::BitcoinAddress {
                    address: "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx".into(),
                    amount_sat: Some(1000),
                    label: None,
                    payjoin: None,
                },
                amount_sat: None,
                send_max: false,
                fee: FeePriority::Custom(2),
                note: None,
                style: Default::default(),
            })
            .await;
        assert!(matches!(q, Err(Error::Network(_))));
    }

    /// Give a wallet a fake unconfirmed coin paying its first receive address.
    async fn fund(a: &OnchainAccount, sats: u64) {
        use bdk_wallet::bitcoin::hashes::Hash;
        use bdk_wallet::bitcoin::{
            absolute, transaction, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid,
            Witness,
        };
        let mut inner = a.inner.lock().await;
        let spk = inner
            .wallet
            .peek_address(KeychainKind::External, 0)
            .script_pubkey();
        let tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                // Any non-null outpoint: a null one would make this a coinbase.
                previous_output: OutPoint {
                    txid: Txid::from_byte_array([7; 32]),
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
        };
        inner.wallet.apply_unconfirmed_txs([(tx, now())]);
    }

    fn send_req(sats: u64) -> SendRequest {
        SendRequest {
            destination: PaymentRequest::BitcoinAddress {
                address: "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq".into(),
                amount_sat: None,
                label: None,
                payjoin: None,
            },
            amount_sat: Some(sats),
            send_max: false,
            fee: FeePriority::Custom(3),
            note: None,
            style: Default::default(),
        }
    }

    async fn finalizes(a: &OnchainAccount, psbt_b64: &str) -> bool {
        let mut psbt = OnchainAccount::psbt_from_b64(psbt_b64).unwrap();
        a.sign_locally(&mut psbt).unwrap();
        a.inner
            .lock()
            .await
            .wallet
            .finalize_psbt(&mut psbt, SignOptions::default())
            .unwrap()
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
        assert!(matches!(
            a.prepare_send(send_req(50_000)).await,
            Err(Error::InsufficientFunds { .. })
        ));
    }

    #[tokio::test]
    async fn two_of_two_multisig_needs_and_accepts_cosigner() {
        let dir = tempfile::tempdir().unwrap();
        let ours = Seed::from_phrase(ABANDON)
            .unwrap()
            .master_xpriv(Network::Bitcoin)
            .unwrap();
        let theirs = Seed::generate(12)
            .unwrap()
            .master_xpriv(Network::Bitcoin)
            .unwrap();
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
                custody: Custody::Multisig {
                    threshold: 2,
                    total: 2,
                    local_signers: 1,
                },
                signer: Some(signer),
                payjoin_receive: false,
                payjoin_dir: None,
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

    /// Fund receive address `index` with one coin of `sats`.
    async fn fund_at(a: &OnchainAccount, index: u32, sats: u64, salt: u8) {
        use bdk_wallet::bitcoin::hashes::Hash;
        use bdk_wallet::bitcoin::{
            absolute, transaction, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid,
            Witness,
        };
        let mut inner = a.inner.lock().await;
        let spk = inner
            .wallet
            .peek_address(KeychainKind::External, index)
            .script_pubkey();
        let tx = Transaction {
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
        };
        inner.wallet.apply_unconfirmed_txs([(tx, now())]);
    }

    fn private_req(sats: u64, to: &str) -> SendRequest {
        let mut r = send_req(sats);
        r.style = SpendStyle::Private;
        if let PaymentRequest::BitcoinAddress { address, .. } = &mut r.destination {
            *address = to.into();
        }
        r
    }

    const DEST: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";

    #[tokio::test]
    async fn private_send_spends_unconfirmed_incoming_coins() {
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        for i in 0..4 {
            fund_at(&a, i, 40_000, 10 + i as u8).await;
        }
        // Incoming coins that haven't confirmed yet can be spent (0-conf).
        let q = a.prepare_send(private_req(30_000, DEST)).await.unwrap();
        assert!(q.private);
        assert!(finalizes(&a, &q.handle).await);
    }

    #[tokio::test]
    async fn private_plan_shape() {
        use bdk_wallet::bitcoin::{Address, FeeRate};
        // Plan directly over synthetic coins: four coins at four addresses.
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        for i in 0..4 {
            fund_at(&a, i, 40_000, 20 + i as u8).await;
        }
        let mut inner = a.inner.lock().await;
        let utxos: Vec<_> = inner.wallet.list_unspent().collect();
        assert_eq!(utxos.len(), 4);
        let weight = privacy::input_weight(&inner.wallet).unwrap();
        let fresh = [
            inner
                .wallet
                .reveal_next_address(KeychainKind::Internal)
                .script_pubkey(),
            inner
                .wallet
                .reveal_next_address(KeychainKind::Internal)
                .script_pubkey(),
            inner
                .wallet
                .reveal_next_address(KeychainKind::Internal)
                .script_pubkey(),
        ];
        let dest = Address::from_str(DEST)
            .unwrap()
            .assume_checked()
            .script_pubkey();
        let rate = FeeRate::from_sat_per_vb(3).unwrap();
        let plan = privacy::plan(&utxos, weight, &dest, 30_000, rate, fresh.clone()).unwrap();

        // Exactly two outputs of the payment amount: the payment and the decoy.
        let equal: Vec<_> = plan.outputs.iter().filter(|(_, v)| *v == 30_000).collect();
        assert_eq!(equal.len(), 2);
        assert!(equal.iter().any(|(s, _)| *s == dest) && equal.iter().any(|(s, _)| *s == fresh[0]));
        // Two coins per side are needed (40k each side covers 30k + fee share).
        assert_eq!(plan.inputs.len(), 2);
        // Value is conserved.
        let out: u64 = plan.outputs.iter().map(|(_, v)| v).sum();
        assert_eq!(out + plan.fee, 80_000);
        // Fee is close to 3 sat/vB for 2 P2WPKH inputs + 4 outputs (~273 vB).
        assert!((700..=900).contains(&plan.fee), "fee {}", plan.fee);
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);

        // A taproot recipient gets a "weaker cover" note.
        let tr =
            Address::from_str("bc1p5d7rjq7g6rdk2yhzks9smlaqtedr4dekq08ge8ztwac72sfr9rusxg3297")
                .unwrap()
                .assume_checked()
                .script_pubkey();
        let plan = privacy::plan(&utxos, weight, &tr, 30_000, rate, fresh.clone()).unwrap();
        assert!(plan
            .notes
            .iter()
            .any(|n| n.contains("different address type")));

        // Too big for two separate sides.
        // 80k per side plus fees is more than four 40k coins can cover.
        assert!(privacy::plan(&utxos, weight, &dest, 80_000, rate, fresh).is_err());
    }

    #[tokio::test]
    async fn private_send_builds_and_signs() {
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        // Coins on our own change keychain count as spendable before confirming.
        {
            use bdk_wallet::bitcoin::hashes::Hash;
            use bdk_wallet::bitcoin::{
                absolute, transaction, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut,
                Txid, Witness,
            };
            let mut inner = a.inner.lock().await;
            for i in 0..3u8 {
                let spk = inner
                    .wallet
                    .reveal_next_address(KeychainKind::Internal)
                    .script_pubkey();
                let tx = Transaction {
                    version: transaction::Version::TWO,
                    lock_time: absolute::LockTime::ZERO,
                    input: vec![TxIn {
                        previous_output: OutPoint {
                            txid: Txid::from_byte_array([40 + i; 32]),
                            vout: 0,
                        },
                        script_sig: ScriptBuf::new(),
                        sequence: Sequence::MAX,
                        witness: Witness::new(),
                    }],
                    output: vec![TxOut {
                        value: Amount::from_sat(50_000),
                        script_pubkey: spk,
                    }],
                };
                inner.wallet.apply_unconfirmed_txs([(tx, now())]);
            }
        }
        let q = a.prepare_send(private_req(20_000, DEST)).await.unwrap();
        assert!(q.private);
        let (psbt, _) = OnchainAccount::decode_handle(&q.handle).unwrap();
        let equal = psbt
            .unsigned_tx
            .output
            .iter()
            .filter(|o| o.value.to_sat() == 20_000)
            .count();
        assert_eq!(equal, 2);
        assert!(psbt.unsigned_tx.input.len() >= 2);
        assert!(finalizes(&a, &q.handle).await);

        // Sending everything privately isn't possible.
        let mut all = private_req(0, DEST);
        all.send_max = true;
        assert!(a.prepare_send(all).await.is_err());
    }

    #[tokio::test]
    async fn trezor_request_and_signed_tx_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        fund(&a, 100_000).await;
        let q = a.prepare_send(send_req(40_000)).await.unwrap();
        let (unsigned, _) = OnchainAccount::decode_handle(&q.handle).unwrap();
        let b64 = unsigned.to_string();

        let req = trezor::sign_request(&b64, Network::Bitcoin).unwrap();
        assert_eq!(req["coin"], "btc");
        let input = &req["inputs"][0];
        assert_eq!(input["script_type"], "SPENDWITNESS");
        // m/84'/0'/0'/0/0
        assert_eq!(
            input["address_n"],
            serde_json::json!([0x8000_0054u32, 0x8000_0000u32, 0x8000_0000u32, 0, 0])
        );
        assert_eq!(input["amount"], "100000");
        assert_eq!(req["refTxs"].as_array().unwrap().len(), 1);
        let outs = req["outputs"].as_array().unwrap();
        assert!(outs.iter().any(
            |o| o["address"] == "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq"
                && o["amount"] == "40000"
        ));
        assert!(outs
            .iter()
            .any(|o| o["script_type"] == "PAYTOWITNESS" && o["address_n"][3] == 1));

        // Stand in for the device: sign the same transaction elsewhere.
        let mut signed = unsigned.clone();
        a.sign_locally(&mut signed).unwrap();
        assert!(a
            .inner
            .lock()
            .await
            .wallet
            .finalize_psbt(&mut signed, SignOptions::default())
            .unwrap());
        let raw = trezor::extract_hex(signed).unwrap();

        let merged = trezor::merge_signed_tx(&b64, &raw).unwrap();
        let mut merged = OnchainAccount::psbt_from_b64(&merged).unwrap();
        assert!(a
            .inner
            .lock()
            .await
            .wallet
            .finalize_psbt(&mut merged, SignOptions::default())
            .unwrap());
        assert_eq!(trezor::extract_hex(merged).unwrap(), raw);

        // A different transaction is refused.
        let other = a.prepare_send(send_req(30_000)).await.unwrap();
        let (other, _) = OnchainAccount::decode_handle(&other.handle).unwrap();
        assert!(trezor::merge_signed_tx(&other.to_string(), &raw).is_err());
    }

    #[test]
    fn signatures_needed_by_custody() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = hot(dir.path());
        assert_eq!(a.signatures_needed(), 0);
        a.custody = Custody::Multisig {
            threshold: 2,
            total: 3,
            local_signers: 1,
        };
        assert_eq!(a.signatures_needed(), 1);
        a.custody = Custody::Hardware {
            device: "Coldcard Q".into(),
            fingerprint: "73c5da0a".into(),
        };
        assert_eq!(a.signatures_needed(), 1);
    }

    // --- 0-conf, RBF, speed up and cancel ----------------------------------

    use bdk_wallet::bitcoin::{Transaction, TxOut};

    /// Everything `send` does except talking to the network.
    async fn broadcast_offline(
        a: &OnchainAccount,
        q: &SendQuote,
        signed: Option<String>,
    ) -> Transaction {
        let (psbt, _) = OnchainAccount::decode_handle(&q.handle).unwrap();
        let (psbt, _) = a.sign_and_finalize(psbt, signed).await.unwrap();
        let tx = psbt.extract_tx().unwrap();
        record_tx(&mut a.inner.lock().await.wallet, tx.clone());
        tx
    }

    /// A confirmed coin of `sats` at receive index `index`.
    async fn fund_confirmed(a: &OnchainAccount, index: u32, sats: u64, salt: u8) -> Txid {
        use bdk_wallet::bitcoin::hashes::Hash;
        use bdk_wallet::bitcoin::{
            absolute, transaction, BlockHash, OutPoint, ScriptBuf, Sequence, TxIn, Witness,
        };
        use bdk_wallet::chain::{BlockId, ConfirmationBlockTime, TxUpdate};
        let mut inner = a.inner.lock().await;
        let spk = inner
            .wallet
            .peek_address(KeychainKind::External, index)
            .script_pubkey();
        let tx = Transaction {
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
        };
        let txid = tx.compute_txid();
        let block = BlockId {
            height: 100,
            hash: BlockHash::from_byte_array([salt; 32]),
        };
        let cp = inner.wallet.latest_checkpoint().insert(block);
        let mut tu = TxUpdate::default();
        tu.txs = vec![std::sync::Arc::new(tx)];
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
        txid
    }

    use bdk_wallet::bitcoin::Txid;

    fn dest_spk() -> bdk_wallet::bitcoin::ScriptBuf {
        Address::from_str(DEST)
            .unwrap()
            .assume_checked()
            .script_pubkey()
    }

    fn all_signal_rbf(tx: &Transaction) -> bool {
        tx.input
            .iter()
            .all(|i| i.sequence.to_consensus_u32() <= 0xFFFF_FFFD)
    }

    fn fee_of(a_wallet: &Wallet, tx: &Transaction) -> u64 {
        a_wallet.calculate_fee(tx).unwrap().to_sat()
    }

    /// Send 40k to DEST at 3 sat/vB from a hot wallet holding one unconfirmed 100k coin.
    async fn sent_payment(a: &OnchainAccount) -> Transaction {
        fund(a, 100_000).await;
        let q = a.prepare_send(send_req(40_000)).await.unwrap();
        broadcast_offline(a, &q, None).await
    }

    #[tokio::test]
    async fn zero_conf_incoming_is_spendable_and_confirmed_preferred() {
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        fund(&a, 100_000).await;
        let b = a.info().await.unwrap().balance;
        assert_eq!((b.confirmed, b.pending_in), (0, 100_000));
        // The only coin is unconfirmed and from someone else: still spendable.
        let q = a.prepare_send(send_req(90_000)).await.unwrap();
        assert!(finalizes(&a, &q.handle).await);

        // With a confirmed coin that covers it, that one is used first.
        let confirmed = fund_confirmed(&a, 1, 60_000, 77).await;
        let q = a.prepare_send(send_req(30_000)).await.unwrap();
        let (psbt, _) = OnchainAccount::decode_handle(&q.handle).unwrap();
        let ins: Vec<_> = psbt
            .unsigned_tx
            .input
            .iter()
            .map(|i| i.previous_output.txid)
            .collect();
        assert_eq!(ins, vec![confirmed]);
        // More than the confirmed coin: the unconfirmed one joins in.
        let q = a.prepare_send(send_req(120_000)).await.unwrap();
        let (psbt, _) = OnchainAccount::decode_handle(&q.handle).unwrap();
        assert_eq!(psbt.unsigned_tx.input.len(), 2);
        // Not enough even with everything.
        assert!(matches!(
            a.prepare_send(send_req(200_000)).await,
            Err(Error::InsufficientFunds {
                available: 160_000,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn every_send_signals_rbf() {
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        for i in 0..4 {
            fund_at(&a, i, 40_000, 50 + i as u8).await;
        }
        let normal = a.prepare_send(send_req(30_000)).await.unwrap();
        let private = a.prepare_send(private_req(20_000, DEST)).await.unwrap();
        let mut max = send_req(0);
        max.send_max = true;
        max.amount_sat = None;
        let max = a.prepare_send(max).await.unwrap();
        for q in [normal, private, max] {
            let (psbt, _) = OnchainAccount::decode_handle(&q.handle).unwrap();
            assert!(all_signal_rbf(&psbt.unsigned_tx), "{:?}", psbt.unsigned_tx);
            let tx = broadcast_offline(&a, &q, None).await;
            assert!(all_signal_rbf(&tx));
            // Undo, so the next quote can use the same coins.
            a.inner
                .lock()
                .await
                .wallet
                .apply_evicted_txs([(tx.compute_txid(), now() + 1_000)]);
        }
    }

    #[tokio::test]
    async fn speed_up_replaces_with_higher_fee_and_same_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        let tx1 = sent_payment(&a).await;
        let txid1 = tx1.compute_txid().to_string();

        let d = a.tx_details(&txid1).await.unwrap();
        assert_eq!(d.confirmations, 0);
        assert!(d.can_speed_up && d.can_cancel);
        assert_eq!(d.speed_up_kind, Some(SpeedUpKind::Rbf));
        assert_eq!(d.vsize, tx1.vsize() as u64);
        let fee1 = d.fee_sat.unwrap();
        assert!(d.fee_rate.unwrap() >= 3.0);

        let q = a
            .prepare_bump(&txid1, BumpAction::SpeedUp, FeePriority::Custom(10))
            .await
            .unwrap();
        assert_eq!(q.amount_sat, 0);
        assert!(q.fee_sat > fee1 && q.needs_signature.is_none() && !q.payjoin);
        assert!(!q.notes.is_empty());
        let tx2 = broadcast_offline(&a, &q, None).await;
        assert!(all_signal_rbf(&tx2));
        assert_eq!(bump::shared_inputs(&tx1, &tx2), tx1.input.len());
        // Same payment output, only the change shrank.
        let pay = |t: &Transaction| {
            t.output
                .iter()
                .filter(|o| o.script_pubkey == dest_spk())
                .map(|o| o.value.to_sat())
                .collect::<Vec<_>>()
        };
        assert_eq!(pay(&tx2), vec![40_000]);
        assert_eq!(tx2.output.len(), tx1.output.len());
        let guard = a.inner.lock().await;
        let w = &guard.wallet;
        let fee2 = fee_of(w, &tx2);
        assert_eq!(fee2, q.fee_sat);
        // BIP125: higher rate, and absolute fee covers the old fee plus 1 sat/vB.
        assert!(fee2 >= fee1 + tx2.vsize() as u64);
        assert!(
            fee2 as f64 / tx2.vsize() as f64 >= 10.0,
            "fee {fee2}, {} vB",
            tx2.vsize()
        );
        drop(guard);

        // The replaced one is gone from history; the replacement is pending.
        let history = a.payments(10).await.unwrap();
        let ids: Vec<_> = history.iter().map(|p| p.id.clone()).collect();
        assert!(!ids.contains(&txid1), "{ids:?}");
        assert!(ids.contains(&tx2.compute_txid().to_string()));
        assert!(a.tx_details(&txid1).await.is_err());
        let b = a.info().await.unwrap().balance;
        assert_eq!(b.pending_out, 40_000 + fee2);

        // Too low a requested rate still yields a valid replacement.
        let txid2 = tx2.compute_txid().to_string();
        let q = a
            .prepare_bump(&txid2, BumpAction::SpeedUp, FeePriority::Custom(1))
            .await
            .unwrap();
        assert!(q.fee_sat >= fee2 + tx2.vsize() as u64);
    }

    #[tokio::test]
    async fn cancel_pays_everything_back_to_a_change_address() {
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        let tx1 = sent_payment(&a).await;
        let txid1 = tx1.compute_txid().to_string();
        let fee1 = fee_of(&a.inner.lock().await.wallet, &tx1);

        // Even a "slow" request is raised to what BIP125 needs.
        let q = a
            .prepare_bump(&txid1, BumpAction::Cancel, FeePriority::Custom(1))
            .await
            .unwrap();
        assert_eq!(q.amount_sat, 0);
        let tx2 = broadcast_offline(&a, &q, None).await;
        assert_eq!(tx2.input.len(), tx1.input.len());
        assert_eq!(bump::shared_inputs(&tx1, &tx2), tx1.input.len());
        assert_eq!(tx2.output.len(), 1);
        let guard = a.inner.lock().await;
        let w = &guard.wallet;
        assert!(matches!(
            w.derivation_of_spk(tx2.output[0].script_pubkey.clone()),
            Some((KeychainKind::Internal, _))
        ));
        let fee2 = fee_of(w, &tx2);
        assert_eq!(fee2, q.fee_sat);
        assert_eq!(tx2.output[0].value.to_sat(), 100_000 - fee2);
        assert!(fee2 >= fee1 + tx2.vsize() as u64);
        let rate = |f: u64, t: &Transaction| f as f64 / t.vsize() as f64;
        assert!(rate(fee2, &tx2) >= rate(fee1, &tx1) + 1.0);
        drop(guard);

        let ids: Vec<_> = a
            .payments(10)
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids.len(), 2, "{ids:?}"); // the funding coin and the cancel
        assert!(!ids.contains(&txid1));
        // Everything but the fee is ours again.
        let b = a.info().await.unwrap().balance;
        assert_eq!(b.confirmed + b.pending_in, 100_000 - fee2);
    }

    #[tokio::test]
    async fn cpfp_child_meets_the_package_rate() {
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        fund(&a, 50_000).await;
        // Tell the wallet what the sender's input was worth: a 200 sat fee.
        let parent = {
            let mut inner = a.inner.lock().await;
            let t = inner
                .wallet
                .transactions()
                .next()
                .unwrap()
                .tx_node
                .tx
                .clone();
            inner.wallet.insert_txout(
                t.input[0].previous_output,
                TxOut {
                    value: Amount::from_sat(50_200),
                    script_pubkey: dest_spk(),
                },
            );
            t
        };
        let pid = parent.compute_txid().to_string();
        let d = a.tx_details(&pid).await.unwrap();
        assert!(d.can_speed_up && !d.can_cancel);
        assert_eq!(d.speed_up_kind, Some(SpeedUpKind::Cpfp));
        assert_eq!(d.fee_sat, Some(200));
        assert!(matches!(
            a.prepare_bump(&pid, BumpAction::Cancel, FeePriority::Custom(5))
                .await,
            Err(Error::Invalid(_))
        ));

        let q = a
            .prepare_bump(&pid, BumpAction::SpeedUp, FeePriority::Custom(20))
            .await
            .unwrap();
        assert_eq!(q.amount_sat, 0);
        let child = broadcast_offline(&a, &q, None).await;
        assert_eq!(child.input.len(), 1);
        assert_eq!(child.input[0].previous_output.txid, parent.compute_txid());
        assert_eq!(child.output.len(), 1);
        assert!(all_signal_rbf(&child));
        let child_fee = 50_000 - child.output[0].value.to_sat();
        assert_eq!(child_fee, q.fee_sat);
        let package = (200 + child_fee) as f64 / (parent.vsize() + child.vsize()) as f64;
        assert!(package >= 20.0, "package rate {package}");
        assert!(package < 21.0, "package rate {package}");
        {
            let guard = a.inner.lock().await;
            let w = &guard.wallet;
            assert!(matches!(
                w.derivation_of_spk(child.output[0].script_pubkey.clone()),
                Some((KeychainKind::Internal, _))
            ));
        }
        // The parent's output is now spent, so it can't be sped up again;
        // the child itself can (it's ours and replaceable).
        assert!(!a.tx_details(&pid).await.unwrap().can_speed_up);
        let cd = a
            .tx_details(&child.compute_txid().to_string())
            .await
            .unwrap();
        assert_eq!(cd.speed_up_kind, Some(SpeedUpKind::Rbf));

        // Already fast enough.
        assert!(matches!(
            a.prepare_bump(&pid, BumpAction::SpeedUp, FeePriority::Custom(1))
                .await,
            Err(Error::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn cpfp_errors_when_the_output_is_too_small() {
        let dir = tempfile::tempdir().unwrap();
        let a = hot(dir.path());
        fund(&a, 2_000).await;
        let pid = a
            .inner
            .lock()
            .await
            .wallet
            .transactions()
            .next()
            .unwrap()
            .tx_node
            .txid;
        let pid = pid.to_string();
        // The sender's fee is unknown: assumed zero, with a note.
        let d = a.tx_details(&pid).await.unwrap();
        assert_eq!((d.fee_sat, d.fee_rate), (None, None));
        let q = a
            .prepare_bump(&pid, BumpAction::SpeedUp, FeePriority::Custom(2))
            .await
            .unwrap();
        assert!(q.notes.iter().any(|n| n.contains("isn't known")));
        let e = a
            .prepare_bump(&pid, BumpAction::SpeedUp, FeePriority::Custom(50))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("aren't enough"), "{e}");
    }

    #[tokio::test]
    async fn confirmed_and_watch_only_transactions_cant_be_bumped() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = hot(dir.path());
        let txid = fund_confirmed(&a, 0, 30_000, 88).await.to_string();
        let d = a.tx_details(&txid).await.unwrap();
        assert_eq!(d.confirmations, 1);
        assert_eq!(d.block_height, Some(100));
        assert_eq!(d.block_time, Some(1_700_000_000));
        assert!(!d.can_speed_up && !d.can_cancel && d.speed_up_kind.is_none());
        assert!(a
            .prepare_bump(&txid, BumpAction::SpeedUp, FeePriority::Custom(5))
            .await
            .is_err());
        assert!(a.tx_details("nope").await.is_err());

        fund(&a, 10_000).await;
        let pending = a.payments(10).await.unwrap();
        let pending = &pending
            .iter()
            .find(|p| p.status == PaymentStatus::Pending)
            .unwrap()
            .id;
        a.custody = Custody::WatchOnly;
        let d = a.tx_details(pending).await.unwrap();
        assert!(!d.can_speed_up && !d.can_cancel);
        assert!(matches!(
            a.prepare_bump(pending, BumpAction::SpeedUp, FeePriority::Custom(5))
                .await,
            Err(Error::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn hardware_bumps_need_the_device_signature() {
        let dir = tempfile::tempdir().unwrap();
        let master = Seed::from_phrase(ABANDON)
            .unwrap()
            .master_xpriv(Network::Bitcoin)
            .unwrap();
        let a = OnchainAccount::open(OnchainConfig {
            id: "coldcard".into(),
            name: "Coldcard".into(),
            network: Network::Bitcoin,
            db_path: dir.path().join("coldcard.db"),
            esplora_url: None,
            descriptors: descriptors::hot_single_sig(&master, Network::Bitcoin, 0).unwrap(),
            custody: Custody::Hardware {
                device: "Coldcard Q".into(),
                fingerprint: "73c5da0a".into(),
            },
            signer: None,
            payjoin_receive: false,
            payjoin_dir: None,
        })
        .unwrap();
        // The "device" signs whatever PSBT it's shown.
        let device_sign = |b64: &str| {
            let mut p = OnchainAccount::psbt_from_b64(b64).unwrap();
            let secp = bdk_wallet::bitcoin::secp256k1::Secp256k1::new();
            p.sign(&master, &secp).unwrap();
            p.to_string()
        };
        fund(&a, 100_000).await;
        let q = a.prepare_send(send_req(40_000)).await.unwrap();
        let unsigned = q.needs_signature.clone().unwrap();
        let tx1 = broadcast_offline(&a, &q, Some(device_sign(&unsigned.psbt_base64))).await;
        let txid1 = tx1.compute_txid().to_string();

        for action in [BumpAction::SpeedUp, BumpAction::Cancel] {
            let q = a
                .prepare_bump(&txid1, action, FeePriority::Custom(8))
                .await
                .unwrap();
            let unsigned = q.needs_signature.clone().expect("needs the device");
            assert_eq!(unsigned.signatures_needed, 1);
            assert_eq!(q.amount_sat, 0);
            // Without the device signature it can't go out.
            let (psbt, _) = OnchainAccount::decode_handle(&q.handle).unwrap();
            assert!(a.sign_and_finalize(psbt.clone(), None).await.is_err());
            let (_, _) = a
                .sign_and_finalize(psbt, Some(device_sign(&unsigned.psbt_base64)))
                .await
                .unwrap();
        }
    }
}

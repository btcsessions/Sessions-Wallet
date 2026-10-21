//! Boltz swap provider: lets Sessions Wallet pay to, and receive from, Liquid
//! without the user ever holding L-BTC.
//!
//! * Pay a Liquid address from Lightning: reverse swap (LN → L-BTC).
//! * Pay a Liquid address from on-chain BTC: chain swap (BTC → L-BTC).
//! * Receive from a Liquid sender into Lightning: submarine swap (L-BTC → LN).
//!
//! See `README.md` for trust assumptions and the recovery story.

mod esplora;
mod fees;
mod keys;
mod state;
mod store;

use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use boltz_client::boltz::{
    BoltzApiClientV2, CreateChainRequest, CreateChainResponse, CreateReverseRequest, CreateReverseResponse,
    CreateSubmarineRequest, CreateSubmarineResponse, Side, BOLTZ_MAINNET_URL_V2, BOLTZ_REGTEST,
};
use boltz_client::elements;
use boltz_client::fees::Fee;
use boltz_client::lightning_invoice::Currency;
use boltz_client::network::esplora::{
    EsploraBitcoinClient, EsploraLiquidClient, DEFAULT_LIQUID_MAINNET_NODE, DEFAULT_LIQUID_REGTEST_NODE,
    DEFAULT_MAINNET_NODE, DEFAULT_REGTEST_NODE,
};
use boltz_client::network::{BitcoinChain, Chain, LiquidChain, Network as BoltzNetwork};
use boltz_client::swaps::{BtcLikeTransaction, ChainClient, SwapScript, SwapTransactionParams, TransactionOptions};
use boltz_client::{Bolt11Invoice, PublicKey};
use serde::de::DeserializeOwned;
use sessions_core::rail::{BridgedPayment, SwapStatus};
use sessions_core::{Error, Method, Network, PaymentRequest, ReceiveTarget, Result, Sats, SwapProvider};
use tokio::sync::Mutex;

use crate::esplora::{Broadcast, Esplora, Spend};
use crate::keys::SwapKeys;
use crate::state::Action;
pub use crate::store::{Phase, SwapKind, SwapRecord};
use crate::store::Store;

const NAME: &str = "boltz";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long we keep watching an expired swap's lockup address for a late payment.
const LATE_PAYMENT_GRACE_SECS: u64 = 7 * 24 * 3600;
/// Re-sign an unconfirmed BTC refund at the current fee rate after this long.
const REFUND_BUMP_AFTER_SECS: u64 = 6 * 3600;
/// Shortest invoice validity we'll wrap in a Liquid receive address.
const MIN_RECEIVE_WINDOW_SECS: u64 = 10 * 60;

pub struct BoltzConfig {
    pub network: Network,
    pub data_dir: PathBuf,
    /// BIP85 child phrase reserved for swaps (`keystore::child::SWAPS`).
    pub mnemonic: String,
    /// Liquid Esplora base URL, e.g. `https://blockstream.info/liquid/api`.
    pub liquid_esplora: Option<String>,
    /// Bitcoin Esplora base URL, e.g. `https://blockstream.info/api`.
    pub bitcoin_esplora: Option<String>,
}

impl std::fmt::Debug for BoltzConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoltzConfig")
            .field("network", &self.network)
            .field("data_dir", &self.data_dir)
            .field("mnemonic", &"<redacted>")
            .field("liquid_esplora", &self.liquid_esplora)
            .field("bitcoin_esplora", &self.bitcoin_esplora)
            .finish()
    }
}

pub struct BoltzSwaps {
    network: Network,
    lchain: LiquidChain,
    bchain: BitcoinChain,
    data_dir: PathBuf,
    keys: SwapKeys,
    api: BoltzApiClientV2,
    chain: ChainClient,
    liquid: Esplora,
    bitcoin: Esplora,
    store: Mutex<Store>,
    /// Serialises `tick` and manual refunds so a swap is driven by one task at a time.
    driver: Mutex<()>,
    /// Swap index reconciled with Boltz's restore index in this process.
    index_synced: Mutex<bool>,
}

fn rail(e: impl std::fmt::Display) -> Error {
    Error::rail(NAME, e)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn boltz_response<T: DeserializeOwned>(r: &SwapRecord) -> Result<T> {
    serde_json::from_value(r.boltz_response.clone()).map_err(|e| Error::Storage(format!("swap {}: {e}", r.id)))
}

fn liquid_outpoint(op: &elements::OutPoint) -> String {
    format!("{}:{}", op.txid, op.vout)
}

impl BoltzSwaps {
    pub fn new(cfg: BoltzConfig) -> Result<Self> {
        let (bnet, lchain, bchain, api_url, l_default, b_default) = match cfg.network {
            Network::Bitcoin => (
                BoltzNetwork::Mainnet,
                LiquidChain::Liquid,
                BitcoinChain::Bitcoin,
                BOLTZ_MAINNET_URL_V2,
                DEFAULT_LIQUID_MAINNET_NODE,
                DEFAULT_MAINNET_NODE,
            ),
            Network::Regtest => (
                BoltzNetwork::Regtest,
                LiquidChain::LiquidRegtest,
                BitcoinChain::BitcoinRegtest,
                BOLTZ_REGTEST,
                DEFAULT_LIQUID_REGTEST_NODE,
                DEFAULT_REGTEST_NODE,
            ),
            other => {
                return Err(Error::Network(format!("Boltz swaps are only available on mainnet and regtest, not {other:?}")))
            }
        };
        let keys = SwapKeys::new(&cfg.mnemonic, bnet)?;
        let liquid_url = cfg.liquid_esplora.as_deref().unwrap_or(l_default).trim_end_matches('/').to_string();
        let bitcoin_url = cfg.bitcoin_esplora.as_deref().unwrap_or(b_default).trim_end_matches('/').to_string();
        let chain = ChainClient::new()
            .with_bitcoin(EsploraBitcoinClient::new(bchain, &bitcoin_url, HTTP_TIMEOUT.as_secs()))
            .with_liquid(EsploraLiquidClient::new(lchain, &liquid_url, HTTP_TIMEOUT.as_secs()));
        let store = Store::load(&cfg.data_dir, cfg.network)?;
        Ok(Self {
            network: cfg.network,
            lchain,
            bchain,
            keys,
            api: BoltzApiClientV2::new(api_url.to_string(), Some(HTTP_TIMEOUT)),
            chain,
            liquid: Esplora::new(&liquid_url, HTTP_TIMEOUT),
            bitcoin: Esplora::new(&bitcoin_url, HTTP_TIMEOUT),
            store: Mutex::new(store),
            data_dir: cfg.data_dir,
            driver: Mutex::new(()),
            index_synced: Mutex::new(false),
        })
    }

    /// Every swap this provider knows about.
    pub async fn swaps(&self) -> Vec<SwapRecord> {
        self.store.lock().await.swaps.clone()
    }

    /// Return the L-BTC of a failed incoming (submarine) swap to a Liquid
    /// address, e.g. the original sender's. Only for swaps in
    /// [`Phase::NeedsRescue`]. Returns the refund txid.
    pub async fn refund_to_liquid(&self, swap_id: &str, address: &str) -> Result<String> {
        let _driver = self.driver.lock().await;
        let mut r = self.store.lock().await.get(swap_id).cloned().ok_or_else(|| rail(format!("unknown swap {swap_id}")))?;
        if r.kind != SwapKind::Submarine || r.phase != Phase::NeedsRescue {
            return Err(Error::Unsupported(format!("refund swap {swap_id} in state {}", r.phase.as_str())));
        }
        self.parse_liquid(address)?;
        r.refund_address = Some(address.to_string());
        let res = self.refund_liquid(&mut r).await;
        r.last_error = res.as_ref().err().map(|e| e.to_string());
        r.updated_at = now();
        self.put(&r).await?;
        res?;
        r.refund_txids.last().cloned().ok_or_else(|| rail("refund was not built"))
    }

    // ---------- storage ----------

    /// Replace (or add) a record and write the journal. Memory only changes
    /// once the file is safely on disk.
    async fn put(&self, r: &SwapRecord) -> Result<()> {
        let mut s = self.store.lock().await;
        let mut next = s.clone();
        match next.get_mut(&r.id) {
            Some(x) => *x = r.clone(),
            None => next.swaps.push(r.clone()),
        }
        next.save(&self.data_dir)?;
        *s = next;
        Ok(())
    }

    /// Reserve `n` consecutive key indices (persisted before use).
    async fn alloc_indices(&self, n: u64) -> Result<u64> {
        {
            let mut synced = self.index_synced.lock().await;
            if !*synced {
                // After a restore on a new device the journal is empty; ask
                // Boltz which keys this seed already used so none is reused.
                let r = self
                    .api
                    .post_swap_restore_index(&self.keys.xpub(), Some("m".to_string()), None)
                    .await
                    .map_err(|e| rail(format!("couldn't check previously used swap keys: {e}")))?;
                let boltz_next = u64::try_from(r.index + 1).unwrap_or(0);
                let mut s = self.store.lock().await;
                if boltz_next > s.next_index {
                    let mut next = s.clone();
                    next.next_index = boltz_next;
                    next.save(&self.data_dir)?;
                    *s = next;
                }
                *synced = true;
            }
        }
        let mut s = self.store.lock().await;
        let mut next = s.clone();
        let first = next.next_index;
        next.next_index = first.checked_add(n).ok_or_else(|| rail("swap index overflow"))?;
        next.save(&self.data_dir)?;
        *s = next;
        Ok(first)
    }

    // ---------- validation helpers ----------

    fn parse_liquid(&self, address: &str) -> Result<elements::Address> {
        let params: &'static elements::AddressParams = self.lchain.into();
        let a = elements::Address::parse_with_params(address, params)
            .map_err(|e| rail(format!("not a Liquid address for {:?}: {e}", self.network)))?;
        if a.blinding_pubkey.is_none() {
            return Err(rail("only confidential Liquid addresses (lq1… / VJL…) are supported"));
        }
        Ok(a)
    }

    fn check_btc_address(&self, address: &str) -> Result<()> {
        boltz_client::bitcoin::Address::from_str(address)
            .map_err(|e| rail(format!("invalid refund address: {e}")))?
            .require_network(self.network.to_bitcoin())
            .map_err(|e| rail(format!("refund address is for the wrong network: {e}")))?;
        Ok(())
    }

    fn parse_invoice(&self, invoice: &str) -> Result<(Bolt11Invoice, Sats)> {
        let inv = Bolt11Invoice::from_str(invoice.trim()).map_err(|e| rail(format!("invalid invoice: {e}")))?;
        let want = match self.network {
            Network::Bitcoin => Currency::Bitcoin,
            _ => Currency::Regtest,
        };
        if inv.currency() != want {
            return Err(Error::Network(format!("invoice is for {:?}", inv.currency())));
        }
        let msat = inv.amount_milli_satoshis().ok_or(Error::AmountRequired)?;
        if msat % 1000 != 0 {
            return Err(rail("invoice amount must be a whole number of sats"));
        }
        Ok((inv, msat / 1000))
    }

    // ---------- pay bridges ----------

    async fn reverse_swap(&self, destination: &str, target: Sats) -> Result<BridgedPayment> {
        let pairs = self.api.get_reverse_pairs().await.map_err(rail)?;
        let pair = pairs.get_btc_to_lbtc_pair().ok_or_else(|| rail("Boltz isn't offering Lightning → Liquid swaps right now"))?;
        let claim_fee = fees::liquid_claim_fee(pair.fees.miner_fees.claim);
        let q = fees::quote_reverse(target, pair.fees.percentage, pair.fees.miner_fees.lockup, claim_fee)?;
        fees::check_limits(q.invoice_sat, pair.limits.minimal, pair.limits.maximal, "A Lightning → Liquid swap")?;

        let index = self.alloc_indices(1).await?;
        let claim_pk = self.keys.public_key(index)?;
        let preimage = self.keys.preimage(index)?;
        let resp = self
            .api
            .post_reverse_req(CreateReverseRequest {
                from: "BTC".into(),
                to: "L-BTC".into(),
                claim_public_key: claim_pk,
                invoice: None,
                invoice_amount: Some(q.invoice_sat),
                preimage_hash: Some(preimage.sha256),
                description: Some("Sessions Wallet: pay to Liquid".into()),
                description_hash: None,
                address: None,
                address_signature: None,
                referral_id: None,
                webhook: None,
            })
            .await
            .map_err(rail)?;
        // Checks the invoice's payment hash and that the lockup script/address are ours.
        resp.validate(&preimage, &claim_pk, Chain::Liquid(self.lchain)).map_err(rail)?;
        let invoice = resp.invoice.clone().ok_or_else(|| rail("Boltz returned no invoice"))?;
        let (_, inv_amount) = self.parse_invoice(&invoice)?;
        if inv_amount != q.invoice_sat {
            return Err(rail(format!("Boltz's invoice is for {inv_amount} sats, expected {}", q.invoice_sat)));
        }
        if resp.onchain_amount < q.onchain_needed_sat {
            return Err(rail(format!(
                "Boltz would lock {} sats, {} are needed to deliver {target}",
                resp.onchain_amount, q.onchain_needed_sat
            )));
        }

        let t = now();
        let record = SwapRecord {
            id: resp.id.clone(),
            kind: SwapKind::Reverse,
            key_index: index,
            refund_key_index: None,
            preimage_hash: Some(preimage.sha256.to_string()),
            created_at: t,
            target_amount_sat: target,
            pay_amount_sat: q.invoice_sat,
            claim_fee_sat: Some(claim_fee),
            destination: Some(destination.to_string()),
            refund_address: None,
            invoice: Some(invoice.clone()),
            boltz_response: serde_json::to_value(&resp).map_err(|e| Error::Storage(e.to_string()))?,
            boltz_status: None,
            phase: Phase::Pending,
            payer_locked: false,
            preimage_revealed: false,
            lockup_outpoint: None,
            claim_txids: vec![],
            claim_tx_hex: None,
            refund_outpoints: vec![],
            refund_txids: vec![],
            refund_tx_hex: None,
            refund_broadcast_at: None,
            cooperated: false,
            ended_at: None,
            last_error: None,
            updated_at: t,
        };
        self.put(&record).await?;
        Ok(BridgedPayment {
            swap_id: resp.id,
            pay: PaymentRequest::Bolt11 { invoice },
            amount_sat: q.invoice_sat,
            swap_fee_sat: q.invoice_sat - target,
        })
    }

    async fn chain_swap(&self, destination: &str, target: Sats, refund_address: Option<String>) -> Result<BridgedPayment> {
        let refund_address =
            refund_address.ok_or_else(|| rail("a BTC refund address is required for an on-chain → Liquid swap"))?;
        self.check_btc_address(&refund_address)?;
        let pairs = self.api.get_chain_pairs().await.map_err(rail)?;
        let pair = pairs.get_btc_to_lbtc_pair().ok_or_else(|| rail("Boltz isn't offering BTC → Liquid swaps right now"))?;
        let claim_fee = fees::liquid_claim_fee(pair.fees.miner_fees.user.claim);
        let q = fees::quote_chain(target, pair.fees.percentage, pair.fees.miner_fees.server, claim_fee)?;
        fees::check_limits(q.user_lock_sat, pair.limits.minimal, pair.limits.maximal, "A BTC → Liquid swap")?;

        let index = self.alloc_indices(2).await?;
        let (claim_index, refund_index) = (index, index + 1);
        let claim_pk = self.keys.public_key(claim_index)?;
        let refund_pk = self.keys.public_key(refund_index)?;
        let preimage = self.keys.preimage(claim_index)?;
        let resp = self
            .api
            .post_chain_req(CreateChainRequest {
                from: "BTC".into(),
                to: "L-BTC".into(),
                preimage_hash: preimage.sha256,
                claim_public_key: Some(claim_pk),
                refund_public_key: Some(refund_pk),
                user_lock_amount: None,
                server_lock_amount: Some(q.server_lock_sat),
                pair_hash: Some(pair.hash.clone()),
                referral_id: None,
                webhook: None,
            })
            .await
            .map_err(rail)?;
        resp.validate(&claim_pk, &refund_pk, Chain::Bitcoin(self.bchain), Chain::Liquid(self.lchain), &preimage.sha256)
            .map_err(rail)?;
        fees::check_chain_quote(&q, resp.claim_details.amount, resp.lockup_details.amount)?;
        let user_lock = resp.lockup_details.amount;

        let t = now();
        let record = SwapRecord {
            id: resp.id.clone(),
            kind: SwapKind::ChainBtcToLbtc,
            key_index: claim_index,
            refund_key_index: Some(refund_index),
            preimage_hash: Some(preimage.sha256.to_string()),
            created_at: t,
            target_amount_sat: target,
            pay_amount_sat: user_lock,
            claim_fee_sat: Some(claim_fee),
            destination: Some(destination.to_string()),
            refund_address: Some(refund_address),
            invoice: None,
            boltz_response: serde_json::to_value(&resp).map_err(|e| Error::Storage(e.to_string()))?,
            boltz_status: None,
            phase: Phase::Pending,
            payer_locked: false,
            preimage_revealed: false,
            lockup_outpoint: None,
            claim_txids: vec![],
            claim_tx_hex: None,
            refund_outpoints: vec![],
            refund_txids: vec![],
            refund_tx_hex: None,
            refund_broadcast_at: None,
            cooperated: false,
            ended_at: None,
            last_error: None,
            updated_at: t,
        };
        self.put(&record).await?;
        Ok(BridgedPayment {
            swap_id: resp.id.clone(),
            pay: PaymentRequest::BitcoinAddress {
                address: resp.lockup_details.lockup_address.clone(),
                amount_sat: Some(user_lock),
                label: Some(format!("Boltz swap {}", resp.id)),
            },
            amount_sat: user_lock,
            swap_fee_sat: user_lock - target,
        })
    }

    // ---------- tick ----------

    async fn drive(&self, r: &mut SwapRecord) -> Result<()> {
        match r.phase {
            Phase::Claiming => return self.check_claim(r).await,
            Phase::Refunding => return self.check_refund(r).await,
            Phase::NeedsRescue => return self.check_rescue(r).await,
            p if p.finished() => return Ok(()),
            _ => {}
        }
        let st = self.api.get_swap(&r.id).await.map_err(rail)?;
        if state::lockup_seen(&st.status) {
            r.payer_locked = true;
        }
        r.boltz_status = Some(st.status.clone());
        match state::plan(r.kind, &st.status, r.payer_locked) {
            Action::Wait => Ok(()),
            Action::Claim => self.claim(r).await,
            Action::Paid => {
                r.phase = Phase::Paid;
                Ok(())
            }
            Action::Cooperate => self.cooperate(r).await,
            Action::Complete => {
                r.phase = Phase::Complete;
                Ok(())
            }
            Action::Refund => self.refund_btc(r).await,
            Action::Rescue => {
                r.phase = Phase::NeedsRescue;
                Ok(())
            }
            Action::Fail => self.on_fail(r, &st.status).await,
        }
    }

    async fn on_fail(&self, r: &mut SwapRecord, status: &str) -> Result<()> {
        match r.kind {
            SwapKind::Reverse if r.preimage_revealed => {
                if status == "transaction.refunded" {
                    r.phase = Phase::Failed;
                    return Err(rail("Boltz refunded its lockup after our preimage was revealed; contact Boltz support with this swap id"));
                }
                // Boltz may already have settled our payment: we must claim.
                self.claim(r).await
            }
            SwapKind::Submarine => {
                // Expired before anyone paid. Watch a while for a late payment.
                let resp: CreateSubmarineResponse = boltz_response(r)?;
                if self.liquid.address_has_history(&resp.address).await.map_err(rail)? {
                    r.payer_locked = true;
                    r.phase = Phase::NeedsRescue;
                } else if self.grace_over(r) {
                    r.phase = Phase::Failed;
                }
                Ok(())
            }
            _ => {
                r.phase = Phase::Failed;
                Ok(())
            }
        }
    }

    fn grace_over(&self, r: &mut SwapRecord) -> bool {
        let t = now();
        let ended = *r.ended_at.get_or_insert(t);
        t.saturating_sub(ended) > LATE_PAYMENT_GRACE_SECS
    }

    /// Claim L-BTC to the destination (reverse and chain swaps).
    async fn claim(&self, r: &mut SwapRecord) -> Result<()> {
        let keys = self.keys.keypair(r.key_index)?;
        let pk = PublicKey { compressed: true, inner: keys.public_key() };
        let preimage = self.keys.preimage(r.key_index)?;
        if r.preimage_hash.as_deref() != Some(preimage.sha256.to_string().as_str()) {
            return Err(Error::Crypto(format!("swap {}: derived preimage doesn't match the stored hash", r.id)));
        }
        let lchain = Chain::Liquid(self.lchain);
        let (script, lockup_txid, options) = match r.kind {
            SwapKind::Reverse => {
                let resp: CreateReverseResponse = boltz_response(r)?;
                let script = SwapScript::reverse_from_swap_resp(lchain, &resp, pk).map_err(rail)?;
                let txid = self.api.get_reverse_tx(&r.id).await.map_err(rail)?.id;
                (script, txid, TransactionOptions::default())
            }
            SwapKind::ChainBtcToLbtc => {
                let resp: CreateChainResponse = boltz_response(r)?;
                let script = SwapScript::chain_from_swap_resp(lchain, Side::Claim, resp.claim_details.clone(), pk)
                    .map_err(rail)?;
                let refund_index = r.refund_key_index.ok_or_else(|| Error::Storage("chain swap without refund key".into()))?;
                let refund_keys = self.keys.keypair(refund_index)?;
                let refund_pk = PublicKey { compressed: true, inner: refund_keys.public_key() };
                let lockup_script =
                    SwapScript::chain_from_swap_resp(Chain::Bitcoin(self.bchain), Side::Lockup, resp.lockup_details.clone(), refund_pk)
                        .map_err(rail)?;
                let txid = self
                    .api
                    .get_chain_txs(&r.id)
                    .await
                    .map_err(rail)?
                    .server_lock
                    .ok_or_else(|| rail("Boltz hasn't locked its side yet"))?
                    .transaction
                    .id;
                (script, txid, TransactionOptions::default().with_chain_claim(refund_keys, lockup_script))
            }
            SwapKind::Submarine => return Err(Error::Unsupported("claim a submarine swap".into())),
        };

        // Only reveal the preimage once Boltz's lockup is confirmed in our own
        // view of the chain; afterwards we must finish the claim regardless.
        if !r.preimage_revealed && !self.liquid.tx_confirmed(&lockup_txid).await.map_err(rail)? {
            return Ok(());
        }
        let txid = elements::Txid::from_str(&lockup_txid).map_err(|e| rail(format!("bad lockup txid: {e}")))?;
        let liquid = self.chain.liquid_client().ok_or_else(|| rail("no Liquid client"))?;
        let lockup_tx = liquid.get_tx(txid).await.map_err(rail)?;

        r.preimage_revealed = true;
        self.put(r).await?;

        let destination = r.destination.clone().ok_or_else(|| Error::Storage("swap without destination".into()))?;
        let fee = Fee::Absolute(r.claim_fee_sat.unwrap_or_else(|| fees::liquid_claim_fee(0)));
        let params = |cooperative: bool| SwapTransactionParams {
            keys,
            output_address: destination.clone(),
            fee,
            swap_id: r.id.clone(),
            chain_client: &self.chain,
            boltz_client: &self.api,
            options: Some(
                options.clone().with_lockup_tx(BtcLikeTransaction::liquid(lockup_tx.clone())).with_cooperative(cooperative),
            ),
        };
        // Cooperative key-path claim first; the script path needs nobody's help.
        let tx = match script.construct_claim(&preimage, params(true)).await {
            Ok(tx) => tx,
            Err(e) => {
                tracing::warn!(swap = %r.id, "cooperative claim failed, using script path: {e}");
                script.construct_claim(&preimage, params(false)).await.map_err(rail)?
            }
        };
        let tx = tx.as_liquid().ok_or_else(|| rail("claim is not a Liquid transaction"))?.clone();
        let dest_spk = self.parse_liquid(&destination)?.script_pubkey();
        if !tx.output.iter().any(|o| o.script_pubkey == dest_spk) {
            return Err(rail("claim transaction doesn't pay the destination"));
        }
        let input = tx.input.first().ok_or_else(|| rail("claim transaction has no input"))?;
        let txid = tx.txid().to_string();
        let hex = elements::encode::serialize_hex(&tx);

        r.lockup_outpoint = Some(liquid_outpoint(&input.previous_output));
        r.claim_txids.push(txid.clone());
        r.claim_tx_hex = Some(hex.clone());
        r.phase = Phase::Claiming;
        self.put(r).await?;
        self.broadcast(Chain::Liquid(self.lchain), &hex, &txid).await.map_err(rail)
    }

    async fn check_claim(&self, r: &mut SwapRecord) -> Result<()> {
        let outpoint = r.lockup_outpoint.clone().ok_or_else(|| Error::Storage("claiming without lockup outpoint".into()))?;
        match self.liquid.outspend(&outpoint).await.map_err(rail)? {
            Spend::SpentBy { txid, confirmed } if r.claim_txids.contains(&txid) => {
                if confirmed {
                    r.phase = Phase::Complete;
                }
                Ok(())
            }
            Spend::SpentBy { txid, .. } => {
                r.phase = Phase::Failed;
                Err(rail(format!("the swap lockup was spent by {txid}, which isn't our claim")))
            }
            Spend::Unspent => {
                let (hex, txid) = match (&r.claim_tx_hex, r.claim_txids.last()) {
                    (Some(h), Some(t)) => (h.clone(), t.clone()),
                    _ => return self.claim(r).await,
                };
                match self.broadcast(Chain::Liquid(self.lchain), &hex, &txid).await {
                    Ok(()) => Ok(()),
                    Err(Broadcast::Rejected(e)) => {
                        tracing::warn!(swap = %r.id, "claim rejected ({e}); re-signing");
                        self.claim(r).await
                    }
                    Err(e) => Err(rail(e)),
                }
            }
        }
    }

    /// Submarine: Boltz paid our invoice and wants to sweep cooperatively.
    async fn cooperate(&self, r: &mut SwapRecord) -> Result<()> {
        if !r.cooperated {
            let keys = self.keys.keypair(r.key_index)?;
            let pk = PublicKey { compressed: true, inner: keys.public_key() };
            let resp: CreateSubmarineResponse = boltz_response(r)?;
            let invoice = r.invoice.clone().ok_or_else(|| Error::Storage("submarine swap without invoice".into()))?;
            let script = SwapScript::submarine_from_swap_resp(Chain::Liquid(self.lchain), &resp, pk).map_err(rail)?;
            // Checks Boltz's preimage against our invoice before signing.
            script.submarine_cooperative_claim(&r.id, &keys, &invoice, &self.api).await.map_err(rail)?;
            r.cooperated = true;
        }
        r.phase = Phase::Paid;
        Ok(())
    }

    /// Chain swap failed: return our BTC to the stored refund address.
    async fn refund_btc(&self, r: &mut SwapRecord) -> Result<()> {
        if r.preimage_revealed {
            // Boltz can take our BTC now, so we must take its L-BTC.
            return self.claim(r).await;
        }
        let resp: CreateChainResponse = boltz_response(r)?;
        if !self.bitcoin.address_has_history(&resp.lockup_details.lockup_address).await.map_err(rail)? {
            // Nothing locked. Keep watching a while in case a payment lands late.
            if self.grace_over(r) {
                r.phase = Phase::Failed;
            }
            return Ok(());
        }
        let refund_address = r.refund_address.clone().ok_or_else(|| Error::Storage("chain swap without refund address".into()))?;
        let refund_index = r.refund_key_index.ok_or_else(|| Error::Storage("chain swap without refund key".into()))?;
        let keys = self.keys.keypair(refund_index)?;
        let pk = PublicKey { compressed: true, inner: keys.public_key() };
        let script = SwapScript::chain_from_swap_resp(Chain::Bitcoin(self.bchain), Side::Lockup, resp.lockup_details.clone(), pk)
            .map_err(rail)?;
        let rate = self.bitcoin.fee_rate(3).await.map_err(rail)?.max(1.0);
        let params = |cooperative: bool| SwapTransactionParams {
            keys,
            output_address: refund_address.clone(),
            fee: Fee::Relative(rate),
            swap_id: r.id.clone(),
            chain_client: &self.chain,
            boltz_client: &self.api,
            options: Some(TransactionOptions::default().with_cooperative(cooperative)),
        };
        let tx = match script.construct_refund(params(true)).await {
            Ok(tx) => tx,
            Err(e) => {
                let tip = self.bitcoin.tip_height().await.map_err(rail)?;
                let timeout = resp.lockup_details.timeout_block_height;
                if tip < timeout {
                    return Err(rail(format!("Boltz didn't co-sign the refund ({e}); it can be refunded alone after block {timeout}")));
                }
                script.construct_refund(params(false)).await.map_err(rail)?
            }
        };
        let tx = tx.as_bitcoin().ok_or_else(|| rail("refund is not a Bitcoin transaction"))?.clone();
        let spk = boltz_client::bitcoin::Address::from_str(&refund_address)
            .map_err(rail)?
            .require_network(self.network.to_bitcoin())
            .map_err(rail)?
            .script_pubkey();
        if tx.output.first().map(|o| &o.script_pubkey) != Some(&spk) {
            return Err(rail("refund transaction doesn't pay the refund address"));
        }
        let txid = tx.compute_txid().to_string();
        let hex = boltz_client::bitcoin::consensus::encode::serialize_hex(&tx);
        r.refund_outpoints = tx.input.iter().map(|i| i.previous_output.to_string()).collect();
        self.record_refund(r, txid.clone(), hex.clone()).await?;
        self.broadcast(Chain::Bitcoin(self.bchain), &hex, &txid).await.map_err(rail)
    }

    /// Submarine swap failed: return the sender's L-BTC to `r.refund_address`.
    async fn refund_liquid(&self, r: &mut SwapRecord) -> Result<()> {
        let refund_address = r.refund_address.clone().ok_or_else(|| rail("no Liquid refund address set"))?;
        let dest_spk = self.parse_liquid(&refund_address)?.script_pubkey();
        let keys = self.keys.keypair(r.key_index)?;
        let pk = PublicKey { compressed: true, inner: keys.public_key() };
        let resp: CreateSubmarineResponse = boltz_response(r)?;
        let script = SwapScript::submarine_from_swap_resp(Chain::Liquid(self.lchain), &resp, pk).map_err(rail)?;
        let params = |cooperative: bool| SwapTransactionParams {
            keys,
            output_address: refund_address.clone(),
            fee: Fee::Relative(2.0 * fees::LIQUID_MIN_FEE_RATE),
            swap_id: r.id.clone(),
            chain_client: &self.chain,
            boltz_client: &self.api,
            options: Some(TransactionOptions::default().with_cooperative(cooperative)),
        };
        let tx = match script.construct_refund(params(true)).await {
            Ok(tx) => tx,
            Err(e) => {
                let tip = self.liquid.tip_height().await.map_err(rail)?;
                if u64::from(tip) < resp.timeout_block_height {
                    return Err(rail(format!(
                        "Boltz didn't co-sign the refund ({e}); it can be refunded alone after Liquid block {}",
                        resp.timeout_block_height
                    )));
                }
                script.construct_refund(params(false)).await.map_err(rail)?
            }
        };
        let tx = tx.as_liquid().ok_or_else(|| rail("refund is not a Liquid transaction"))?.clone();
        if !tx.output.iter().any(|o| o.script_pubkey == dest_spk) {
            return Err(rail("refund transaction doesn't pay the refund address"));
        }
        let txid = tx.txid().to_string();
        let hex = elements::encode::serialize_hex(&tx);
        r.refund_outpoints = tx.input.iter().map(|i| liquid_outpoint(&i.previous_output)).collect();
        self.record_refund(r, txid.clone(), hex.clone()).await?;
        self.broadcast(Chain::Liquid(self.lchain), &hex, &txid).await.map_err(rail)
    }

    async fn record_refund(&self, r: &mut SwapRecord, txid: String, hex: String) -> Result<()> {
        r.refund_txids.push(txid);
        r.refund_tx_hex = Some(hex);
        r.refund_broadcast_at = Some(now());
        r.phase = Phase::Refunding;
        self.put(r).await
    }

    async fn check_refund(&self, r: &mut SwapRecord) -> Result<()> {
        let (esplora, chain) = match r.kind {
            SwapKind::Submarine => (&self.liquid, Chain::Liquid(self.lchain)),
            _ => (&self.bitcoin, Chain::Bitcoin(self.bchain)),
        };
        let mut all_confirmed = true;
        let mut unspent = false;
        for op in r.refund_outpoints.clone() {
            match esplora.outspend(&op).await.map_err(rail)? {
                Spend::SpentBy { txid, confirmed } if r.refund_txids.contains(&txid) => all_confirmed &= confirmed,
                Spend::SpentBy { txid, .. } => {
                    r.phase = Phase::Failed;
                    return Err(rail(format!("the swap lockup {op} was spent by {txid}, which isn't our refund")));
                }
                Spend::Unspent => {
                    unspent = true;
                    all_confirmed = false;
                }
            }
        }
        if all_confirmed && !r.refund_outpoints.is_empty() {
            r.phase = Phase::Refunded;
            return Ok(());
        }
        let stale = r.refund_broadcast_at.is_some_and(|t| now().saturating_sub(t) > REFUND_BUMP_AFTER_SECS);
        let rebuild = if unspent {
            match (r.refund_tx_hex.clone(), r.refund_txids.last().cloned()) {
                (Some(hex), Some(txid)) => match self.broadcast(chain, &hex, &txid).await {
                    Ok(()) => false,
                    Err(Broadcast::Rejected(e)) => {
                        tracing::warn!(swap = %r.id, "refund rejected ({e}); re-signing");
                        true
                    }
                    Err(e) => return Err(rail(e)),
                },
                _ => true,
            }
        } else {
            // In the mempool but not confirming: re-sign at today's fee rate (BTC only).
            stale && r.kind == SwapKind::ChainBtcToLbtc
        };
        if !rebuild {
            return Ok(());
        }
        match r.kind {
            SwapKind::Submarine => self.refund_liquid(r).await,
            _ => self.refund_btc(r).await,
        }
    }

    /// Incoming swap that needs a manual refund: notice if it was rescued elsewhere.
    async fn check_rescue(&self, r: &mut SwapRecord) -> Result<()> {
        let resp: CreateSubmarineResponse = boltz_response(r)?;
        let empty = self.liquid.address_utxos_empty(&resp.address).await.map_err(rail)?;
        if empty && self.liquid.address_has_history(&resp.address).await.map_err(rail)? {
            // Funds left the lockup: either Boltz paid late and swept it, or it
            // was refunded elsewhere (e.g. with Boltz's rescue tool).
            let status = self.api.get_swap(&r.id).await.map_err(rail)?.status;
            r.boltz_status = Some(status.clone());
            r.phase = if status == "transaction.claimed" { Phase::Complete } else { Phase::Refunded };
        }
        Ok(())
    }

    async fn broadcast(&self, chain: Chain, hex: &str, txid: &str) -> std::result::Result<(), Broadcast> {
        let esplora = match chain {
            Chain::Liquid(_) => &self.liquid,
            Chain::Bitcoin(_) => &self.bitcoin,
        };
        match esplora.broadcast(hex, txid).await {
            Err(Broadcast::Transport(e)) => {
                // Our explorer is unreachable: hand it to Boltz instead.
                tracing::warn!("esplora broadcast failed ({e}); broadcasting through Boltz");
                self.api.broadcast_tx(chain, &hex.to_string()).await.map(|_| ()).map_err(|e| Broadcast::Transport(e.to_string()))
            }
            other => other,
        }
    }
}

#[async_trait]
impl SwapProvider for BoltzSwaps {
    fn name(&self) -> &str {
        NAME
    }

    fn pay_bridges(&self) -> Vec<(Method, Method)> {
        vec![(Method::LiquidAddress, Method::Bolt11), (Method::LiquidAddress, Method::BitcoinAddress)]
    }

    fn receive_bridges(&self) -> Vec<(Method, Method)> {
        vec![(Method::LiquidAddress, Method::Bolt11)]
    }

    async fn bridge_payment(
        &self,
        target: PaymentRequest,
        amount_sat: Sats,
        payer_method: Method,
        refund_address: Option<String>,
    ) -> Result<BridgedPayment> {
        let PaymentRequest::LiquidAddress { address, .. } = &target else {
            return Err(Error::Unsupported(format!("swap to {:?}", target.method())));
        };
        if amount_sat == 0 {
            return Err(Error::AmountRequired);
        }
        self.parse_liquid(address)?;
        match payer_method {
            Method::Bolt11 => self.reverse_swap(address, amount_sat).await,
            Method::BitcoinAddress => self.chain_swap(address, amount_sat, refund_address).await,
            other => Err(Error::Unsupported(format!("pay a Liquid address with {other:?}"))),
        }
    }

    async fn bridge_receive(&self, ours: ReceiveTarget, outside: Method) -> Result<ReceiveTarget> {
        if outside != Method::LiquidAddress || ours.method != Method::Bolt11 {
            return Err(Error::Unsupported(format!("receive {outside:?} into {:?}", ours.method)));
        }
        let raw = ours.request.trim();
        let invoice = if raw.len() > 10 && raw[..10].eq_ignore_ascii_case("lightning:") { &raw[10..] } else { raw };
        let invoice = invoice.to_ascii_lowercase();
        let (inv, amount) = self.parse_invoice(&invoice)?;
        let t = now();
        // Leave Boltz a few minutes to see the lockup and pay before the invoice dies.
        let inv_expiry = inv.expires_at().map(|d| d.as_secs()).unwrap_or(t).saturating_sub(5 * 60);
        if inv_expiry < t + MIN_RECEIVE_WINDOW_SECS {
            return Err(rail("the Lightning invoice expires too soon to receive over Liquid"));
        }

        let pairs = self.api.get_submarine_pairs().await.map_err(rail)?;
        let pair = pairs.get_lbtc_to_btc_pair().ok_or_else(|| rail("Boltz isn't offering Liquid → Lightning swaps right now"))?;
        fees::check_limits(amount, pair.limits.minimal, pair.limits.maximal, "A Liquid → Lightning swap")?;
        let max_fee = fees::submarine_max_fee(amount, pair.fees.percentage, pair.fees.miner_fees)?;

        let index = self.alloc_indices(1).await?;
        let refund_pk = self.keys.public_key(index)?;
        let resp = self
            .api
            .post_swap_req(&CreateSubmarineRequest {
                from: "L-BTC".into(),
                to: "BTC".into(),
                invoice: invoice.clone(),
                refund_public_key: refund_pk,
                pair_hash: Some(pair.hash.clone()),
                referral_id: None,
                webhook: None,
            })
            .await
            .map_err(rail)?;
        // Checks the script pays to our invoice's hash, refunds to our key, and matches the address.
        resp.validate(&invoice, &refund_pk, Chain::Liquid(self.lchain)).map_err(rail)?;
        if resp.expected_amount <= amount || resp.expected_amount - amount > max_fee {
            return Err(rail(format!(
                "Boltz asked for {} sats to pay a {amount} sat invoice (max fee {max_fee})",
                resp.expected_amount
            )));
        }
        self.parse_liquid(&resp.address)?;
        // Stop showing the address well before Boltz's timeout (Liquid: ~1 block/minute).
        let tip = self.liquid.tip_height().await.map_err(rail)?;
        let blocks_left = resp.timeout_block_height.saturating_sub(u64::from(tip));
        let expires_at = inv_expiry.min(t + blocks_left * 60 / 2);
        if expires_at < t + MIN_RECEIVE_WINDOW_SECS {
            return Err(rail("Boltz's swap window is too short"));
        }

        let record = SwapRecord {
            id: resp.id.clone(),
            kind: SwapKind::Submarine,
            key_index: index,
            refund_key_index: None,
            preimage_hash: None,
            created_at: t,
            target_amount_sat: amount,
            pay_amount_sat: resp.expected_amount,
            claim_fee_sat: None,
            destination: None,
            refund_address: None,
            invoice: Some(invoice),
            boltz_response: serde_json::to_value(&resp).map_err(|e| Error::Storage(e.to_string()))?,
            boltz_status: None,
            phase: Phase::Pending,
            payer_locked: false,
            preimage_revealed: false,
            lockup_outpoint: None,
            claim_txids: vec![],
            claim_tx_hex: None,
            refund_outpoints: vec![],
            refund_txids: vec![],
            refund_tx_hex: None,
            refund_broadcast_at: None,
            cooperated: false,
            ended_at: None,
            last_error: None,
            updated_at: t,
        };
        self.put(&record).await?;
        Ok(ReceiveTarget {
            method: Method::LiquidAddress,
            request: fees::liquid_bip21(&resp.address, resp.expected_amount, &self.lchain.bitcoin().to_string()),
            amount_sat: Some(resp.expected_amount),
            fee_sat: Some(resp.expected_amount - amount),
            expires_at: Some(expires_at),
            // Settlement arrives as a Lightning payment to our own invoice, so
            // keep the account's watch id; the swap is tracked by `tick`.
            watch_id: ours.watch_id.clone(),
        })
    }

    async fn tick(&self) -> Result<Vec<SwapStatus>> {
        let _driver = self.driver.lock().await;
        let active: Vec<SwapRecord> =
            self.store.lock().await.swaps.iter().filter(|s| !s.phase.finished()).cloned().collect();
        let mut out = Vec::with_capacity(active.len());
        for mut r in active {
            // One swap failing (network, Boltz, a bad record) never blocks the
            // others. Anything broadcast was journaled first inside `drive`.
            let res = self.drive(&mut r).await;
            if let Err(e) = &res {
                tracing::warn!(swap = %r.id, "swap step failed: {e}");
            }
            r.last_error = res.err().map(|e| e.to_string());
            r.updated_at = now();
            self.put(&r).await?;
            out.push(SwapStatus { swap_id: r.id.clone(), state: r.phase.as_str().to_string(), finished: r.phase.finished() });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHILD: &str = "velvet engage shaft effort clarify annual protect client only surround sock gain";

    fn cfg(network: Network, dir: &std::path::Path) -> BoltzConfig {
        BoltzConfig {
            network,
            data_dir: dir.to_path_buf(),
            mnemonic: CHILD.into(),
            liquid_esplora: None,
            bitcoin_esplora: None,
        }
    }

    #[test]
    fn networks() {
        let dir = tempfile::tempdir().unwrap();
        assert!(BoltzSwaps::new(cfg(Network::Bitcoin, dir.path())).is_ok());
        Store::new(Network::Bitcoin).save(dir.path()).unwrap();
        assert!(BoltzSwaps::new(cfg(Network::Regtest, dir.path())).is_err(), "journal is bound to mainnet");
        let dir = tempfile::tempdir().unwrap();
        assert!(BoltzSwaps::new(cfg(Network::Regtest, dir.path())).is_ok());
        for n in [Network::Testnet, Network::Signet] {
            assert!(matches!(BoltzSwaps::new(cfg(n, dir.path())), Err(Error::Network(_))));
        }
    }

    #[test]
    fn config_debug_hides_phrase() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!format!("{:?}", cfg(Network::Bitcoin, dir.path())).contains("velvet"));
    }

    #[test]
    fn bridges() {
        let dir = tempfile::tempdir().unwrap();
        let b = BoltzSwaps::new(cfg(Network::Bitcoin, dir.path())).unwrap();
        assert_eq!(b.name(), "boltz");
        assert!(b.pay_bridges().contains(&(Method::LiquidAddress, Method::Bolt11)));
        assert!(b.pay_bridges().contains(&(Method::LiquidAddress, Method::BitcoinAddress)));
        assert_eq!(b.receive_bridges(), vec![(Method::LiquidAddress, Method::Bolt11)]);
    }

    #[tokio::test]
    async fn rejects_bad_requests_before_any_network_call() {
        let dir = tempfile::tempdir().unwrap();
        let b = BoltzSwaps::new(cfg(Network::Bitcoin, dir.path())).unwrap();
        let liquid = |a: &str| PaymentRequest::LiquidAddress { address: a.into(), amount_sat: None };
        // Not a Liquid target.
        let r = b.bridge_payment(PaymentRequest::Bolt11 { invoice: "lnbc1".into() }, 1_000, Method::Bolt11, None).await;
        assert!(matches!(r, Err(Error::Unsupported(_))));
        // Garbage address.
        assert!(b.bridge_payment(liquid("lq1nope"), 1_000, Method::Bolt11, None).await.is_err());
        // Zero amount.
        assert!(matches!(b.bridge_payment(liquid("lq1nope"), 0, Method::Bolt11, None).await, Err(Error::AmountRequired)));
        // Wrong receive combination.
        let ours = ReceiveTarget {
            method: Method::BitcoinAddress,
            request: "bc1q".into(),
            amount_sat: None,
            fee_sat: None,
            expires_at: None,
            watch_id: None,
        };
        assert!(matches!(b.bridge_receive(ours, Method::LiquidAddress).await, Err(Error::Unsupported(_))));
        // Nothing was journaled.
        assert!(b.swaps().await.is_empty());
        assert!(!Store::path(dir.path()).exists());
    }

    #[test]
    fn liquid_address_rules() {
        let dir = tempfile::tempdir().unwrap();
        let b = BoltzSwaps::new(cfg(Network::Bitcoin, dir.path())).unwrap();
        let secp = boltz_client::elements::secp256k1_zkp::Secp256k1::new();
        let k = b.keys.keypair(0).unwrap();
        let pk = boltz_client::elements::bitcoin::PublicKey::new(k.public_key());
        let blinder = boltz_client::elements::secp256k1_zkp::PublicKey::from_secret_key(
            &secp,
            &boltz_client::elements::secp256k1_zkp::SecretKey::from_slice(&[3u8; 32]).unwrap(),
        );
        let conf = elements::Address::p2wpkh(&pk, Some(blinder), &elements::AddressParams::LIQUID).to_string();
        let unconf = elements::Address::p2wpkh(&pk, None, &elements::AddressParams::LIQUID).to_string();
        let regtest = elements::Address::p2wpkh(&pk, Some(blinder), &elements::AddressParams::ELEMENTS).to_string();
        assert!(conf.starts_with("lq1"));
        assert!(b.parse_liquid(&conf).is_ok());
        assert!(b.parse_liquid(&unconf).unwrap_err().to_string().contains("confidential"));
        assert!(b.parse_liquid(&regtest).is_err());
    }

    #[test]
    fn btc_refund_address_network() {
        let dir = tempfile::tempdir().unwrap();
        let b = BoltzSwaps::new(cfg(Network::Bitcoin, dir.path())).unwrap();
        assert!(b.check_btc_address("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq").is_ok());
        assert!(b.check_btc_address("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080").is_err());
        assert!(b.check_btc_address("nope").is_err());
    }
}

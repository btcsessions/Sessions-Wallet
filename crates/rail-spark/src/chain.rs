//! The Breez SDK's chain backend over Electrum.
//!
//! By default the SDK reads the chain from mempool.space's REST API. On some
//! phones HTTP requests to it time out while raw TLS (Electrum) works, and
//! then the SDK never sees on-chain deposits. [`ElectrumChainService`] gives
//! the SDK the same answers from the app's shared Electrum connection
//! ([`rail_onchain::electrum_chain::ElectrumChain`]): deposit detection,
//! claims, refunds and the fee rates claims are capped by all go through it.

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use breez_sdk_spark::{
    BitcoinChainService, ChainServiceError, Outspend, RecommendedFees, TxStatus, Utxo,
};
use rail_onchain::electrum_chain::{
    AddressOutput, ElectrumChain, FeeRates, QueryError, Spend, TxState,
};
use rail_onchain::Txid;

/// Where the Spending account's SDK reads the chain.
#[derive(Debug, Clone)]
pub enum SparkChain {
    /// The SDK's built-in default (mempool.space on mainnet).
    Default,
    /// The app's Electrum servers.
    Electrum(ElectrumChain),
    /// An Esplora REST API. `mempool`: a mempool server (it has mempool's
    /// `fees/recommended`), not plain Esplora.
    Esplora { url: String, mempool: bool },
}

/// [`BitcoinChainService`] over an [`ElectrumChain`].
pub struct ElectrumChainService {
    chain: ElectrumChain,
}

impl ElectrumChainService {
    pub fn new(chain: ElectrumChain) -> Self {
        ElectrumChainService { chain }
    }

    pub fn into_arc(self) -> Arc<dyn BitcoinChainService> {
        Arc::new(self)
    }
}

fn chain_err(e: QueryError) -> ChainServiceError {
    match e {
        QueryError::InvalidAddress(m) => ChainServiceError::InvalidAddress(m),
        QueryError::NotFound(m) => ChainServiceError::NotFound(m),
        QueryError::Connectivity(m) => ChainServiceError::ServiceConnectivity(m),
        QueryError::Other(m) => ChainServiceError::Generic(m),
    }
}

fn txid(s: &str) -> Result<Txid, ChainServiceError> {
    Txid::from_str(s.trim())
        .map_err(|e| ChainServiceError::Generic(format!("invalid txid {s}: {e}")))
}

fn status(s: TxState) -> TxStatus {
    TxStatus {
        confirmed: s.confirmed(),
        block_height: s.height,
        block_time: s.time.map(u64::from),
    }
}

fn utxo(o: AddressOutput) -> Utxo {
    Utxo {
        txid: o.txid.to_string(),
        vout: o.vout,
        value: o.value,
        status: status(o.state),
    }
}

/// mempool.space's field names for Electrum's estimates.
pub fn recommended(f: FeeRates) -> RecommendedFees {
    // Deposit claims pay what the server recommends: cap it (see
    // `rail_onchain::chain::MAX_SERVER_FEE_RATE`).
    let cap = |r: u64| r.min(rail_onchain::chain::MAX_SERVER_FEE_RATE as u64);
    RecommendedFees {
        fastest_fee: cap(f.fastest),
        half_hour_fee: cap(f.half_hour),
        hour_fee: cap(f.hour),
        economy_fee: cap(f.economy),
        minimum_fee: cap(f.minimum),
    }
}

#[async_trait]
impl BitcoinChainService for ElectrumChainService {
    async fn get_address_utxos(&self, address: String) -> Result<Vec<Utxo>, ChainServiceError> {
        Ok(self
            .chain
            .address_utxos(&address)
            .await
            .map_err(chain_err)?
            .into_iter()
            .map(utxo)
            .collect())
    }

    async fn get_address_txos(&self, address: String) -> Result<Vec<Utxo>, ChainServiceError> {
        Ok(self
            .chain
            .address_txos(&address)
            .await
            .map_err(chain_err)?
            .into_iter()
            .map(utxo)
            .collect())
    }

    async fn get_transaction_status(&self, txid_s: String) -> Result<TxStatus, ChainServiceError> {
        let id = txid(&txid_s)?;
        Ok(status(self.chain.tx_state(id).await.map_err(chain_err)?))
    }

    async fn tip_height(&self) -> Result<u32, ChainServiceError> {
        self.chain.tip_height().await.map_err(chain_err)
    }

    async fn get_transaction_hex(&self, txid_s: String) -> Result<String, ChainServiceError> {
        let id = txid(&txid_s)?;
        self.chain.tx_hex(id).await.map_err(chain_err)
    }

    async fn get_outspend(&self, txid_s: String, vout: u32) -> Result<Outspend, ChainServiceError> {
        let id = txid(&txid_s)?;
        Ok(
            match self.chain.outspend(id, vout).await.map_err(chain_err)? {
                Spend::Unspent => Outspend::Unspent,
                Spend::Spent { txid, vin, state } => Outspend::Spent {
                    txid: txid.to_string(),
                    vin,
                    status: status(state),
                },
            },
        )
    }

    async fn broadcast_transaction(&self, tx: String) -> Result<(), ChainServiceError> {
        self.chain
            .broadcast_hex(&tx)
            .await
            .map(|_| ())
            .map_err(chain_err)
    }

    async fn recommended_fees(&self) -> Result<RecommendedFees, ChainServiceError> {
        Ok(recommended(
            self.chain.fee_rates().await.map_err(chain_err)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rail_onchain::electrum_chain::fake::{tx, FakeSource};
    use std::collections::HashMap;

    const DEPOSIT: &str = "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu";
    const EXCHANGE: &str = "bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g";
    const REFUND_TO: &str = "bc1qp59yckz4ae5c4efgw2s5wfyvrz0ala7rgvuz8z";

    fn spk(a: &str) -> bitcoin::ScriptBuf {
        bitcoin::Address::from_str(a)
            .unwrap()
            .assume_checked()
            .script_pubkey()
    }

    fn outpoint(n: u8) -> bitcoin::OutPoint {
        bitcoin::OutPoint {
            txid: Txid::from_str(&format!("{n:02x}").repeat(32)).unwrap(),
            vout: 0,
        }
    }

    fn service(src: &Arc<FakeSource>) -> Arc<dyn BitcoinChainService> {
        let chain = ElectrumChain::new(src.clone(), sessions_core::Network::Bitcoin);
        ElectrumChainService::new(chain).into_arc()
    }

    #[tokio::test]
    async fn fee_rates_in_mempool_space_shape() {
        // BTC/kvB per target: 1, 3, 6, 25, 1008 blocks.
        let fees: HashMap<usize, f64> = [
            (1, 0.00012),
            (3, 0.00008),
            (6, 0.000041),
            (25, -1.0),
            (1008, 0.00001),
        ]
        .into();
        let src = Arc::new(FakeSource {
            fees,
            relay: Some(0.00001),
            ..Default::default()
        });
        let f = service(&src).recommended_fees().await.unwrap();
        assert_eq!(
            f,
            RecommendedFees {
                fastest_fee: 12,
                half_hour_fee: 8,
                hour_fee: 5, // 4.1 rounds up
                economy_fee: 1,
                minimum_fee: 1,
            }
        );
        // What Breez's NetworkRecommended { leeway: 1 } caps a claim at, per
        // vbyte: fastest + 1.
        assert_eq!(f.fastest_fee + 1, 13);
    }

    #[tokio::test]
    async fn deposit_lookups_status_and_outspend() {
        let src = Arc::new(FakeSource {
            tip: 900_012,
            ..Default::default()
        });
        let (deposit, exchange) = (spk(DEPOSIT), spk(EXCHANGE));
        let funding = src.add(
            tx(
                &[outpoint(1)],
                &[(&exchange, 1_000_000), (&deposit, 60_000)],
            ),
            900_010,
        );
        let svc = service(&src);

        // The deposit address's unspent output, confirmed.
        let utxos = svc.get_address_utxos(DEPOSIT.into()).await.unwrap();
        assert_eq!(utxos.len(), 1);
        let u = &utxos[0];
        assert_eq!(
            (u.txid.as_str(), u.vout, u.value),
            (&*funding.to_string(), 1, 60_000)
        );
        assert!(u.status.confirmed);
        assert_eq!(u.status.block_height, Some(900_010));
        assert!(u.status.block_time.is_some());

        // Status and tip give Breez the confirmation count: 3 here.
        let st = svc
            .get_transaction_status(funding.to_string())
            .await
            .unwrap();
        let tip = svc.tip_height().await.unwrap();
        assert_eq!(tip - st.block_height.unwrap() + 1, 3);

        // The raw transaction rebinds to its txid (Breez checks this).
        let hex = svc.get_transaction_hex(funding.to_string()).await.unwrap();
        let back: bitcoin::Transaction = bitcoin::consensus::encode::deserialize_hex(&hex).unwrap();
        assert_eq!(back.compute_txid(), funding);

        // Unknown transactions are NotFound (an answer), not connectivity.
        let unknown = "ab".repeat(32);
        assert!(matches!(
            svc.get_transaction_status(unknown.clone()).await,
            Err(ChainServiceError::NotFound(_))
        ));
        assert!(matches!(
            svc.get_outspend(unknown, 0).await,
            Err(ChainServiceError::NotFound(_))
        ));
        assert!(matches!(
            svc.get_transaction_status("nope".into()).await,
            Err(ChainServiceError::Generic(_))
        ));
        assert!(matches!(
            svc.get_address_utxos("tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx".into())
                .await,
            Err(ChainServiceError::InvalidAddress(_))
        ));

        // Outspend: unspent, then spent by a refund in the mempool, then
        // that refund confirmed.
        assert_eq!(
            svc.get_outspend(funding.to_string(), 1).await.unwrap(),
            Outspend::Unspent
        );
        let refund = tx(
            &[bitcoin::OutPoint {
                txid: funding,
                vout: 1,
            }],
            &[(&spk(REFUND_TO), 59_000)],
        );
        let refund_hex = bitcoin::consensus::encode::serialize_hex(&refund);
        svc.broadcast_transaction(refund_hex.clone()).await.unwrap();
        let refund_id = refund.compute_txid();
        assert_eq!(
            svc.get_outspend(funding.to_string(), 1).await.unwrap(),
            Outspend::Spent {
                txid: refund_id.to_string(),
                vin: 0,
                status: TxStatus {
                    confirmed: false,
                    block_height: None,
                    block_time: None
                }
            }
        );
        // A rebroadcast reads as "already on the network" to Breez.
        let again = svc.broadcast_transaction(refund_hex).await.unwrap_err();
        assert!(again.to_string().to_lowercase().contains("txn-already"));
        src.add(refund, 900_013);
        match svc.get_outspend(funding.to_string(), 1).await.unwrap() {
            Outspend::Spent { status, .. } => {
                assert!(status.confirmed);
                assert_eq!(status.block_height, Some(900_013));
            }
            o => panic!("expected spent, got {o:?}"),
        }
        let st = svc
            .get_transaction_status(refund_id.to_string())
            .await
            .unwrap();
        assert_eq!(st.block_height, Some(900_013));

        // Every output ever paid to the address, spent or not.
        let txos = svc.get_address_txos(DEPOSIT.into()).await.unwrap();
        assert_eq!(txos.len(), 1);
        assert_eq!(txos[0].txid, funding.to_string());
        assert!(svc
            .get_address_utxos(DEPOSIT.into())
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn unreachable_server_is_connectivity() {
        let src = Arc::new(FakeSource {
            offline: true,
            ..Default::default()
        });
        let svc = service(&src);
        assert!(matches!(
            svc.get_address_utxos(DEPOSIT.into()).await,
            Err(ChainServiceError::ServiceConnectivity(_))
        ));
        assert!(matches!(
            svc.recommended_fees().await,
            Err(ChainServiceError::ServiceConnectivity(_))
        ));
    }

    /// Live check (manual), through the SDK's trait:
    /// `cargo test -p rail-spark live_chain_service -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_chain_service() {
        let servers =
            rail_onchain::electrum::default_servers(sessions_core::Network::Bitcoin).unwrap();
        let chain = ElectrumChain::for_servers(&servers, sessions_core::Network::Bitcoin).unwrap();
        let svc = ElectrumChainService::new(chain).into_arc();
        println!("tip {}", svc.tip_height().await.unwrap());
        println!("fees {:?}", svc.recommended_fees().await.unwrap());
        // The first bitcoin transaction (block 170).
        let t170 = "f4184fc596403b9d638783cf57adfe4c75c605f6356fbc91338530e9831e9e16";
        let st = svc.get_transaction_status(t170.into()).await.unwrap();
        println!("{t170}: {st:?}");
        assert_eq!(st.block_height, Some(170));
        println!(
            "outspend 1: {:?}",
            svc.get_outspend(t170.into(), 1).await.unwrap()
        );
        let address = DEPOSIT;
        println!(
            "{address}: {} utxos",
            svc.get_address_utxos(address.into()).await.unwrap().len()
        );
    }
}

//! The few Esplora calls `boltz-client` doesn't make for us: spend tracking,
//! tip height, fee estimates, and a broadcast that actually checks the
//! response (the library's Liquid broadcast returns the body even on HTTP
//! errors).

use std::collections::HashMap;
use std::time::Duration;

use boltz_client::reqwest;
use serde::Deserialize;

#[derive(Clone)]
pub struct Esplora {
    http: reqwest::Client,
    pub base: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spend {
    Unspent,
    SpentBy { txid: String, confirmed: bool },
}

#[derive(Deserialize)]
struct OutspendResp {
    spent: bool,
    txid: Option<String>,
    status: Option<StatusResp>,
}

#[derive(Deserialize)]
struct StatusResp {
    confirmed: bool,
}

impl Esplora {
    pub fn new(base: &str, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_default();
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
        }
    }

    async fn get_text(&self, path: &str) -> Result<String, String> {
        let url = format!("{}{path}", self.base);
        let r = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("GET {url}: {e}"))?;
        let status = r.status();
        let body = r.text().await.map_err(|e| format!("GET {url}: {e}"))?;
        if !status.is_success() {
            return Err(format!("GET {url}: {status} {body}"));
        }
        Ok(body)
    }

    /// Who (if anyone) spends `outpoint` (`txid:vout`).
    pub async fn outspend(&self, outpoint: &str) -> Result<Spend, String> {
        let (txid, vout) = outpoint
            .split_once(':')
            .ok_or_else(|| format!("bad outpoint {outpoint}"))?;
        let body = self
            .get_text(&format!("/tx/{txid}/outspend/{vout}"))
            .await?;
        let r: OutspendResp =
            serde_json::from_str(&body).map_err(|e| format!("outspend: {e}: {body}"))?;
        Ok(match (r.spent, r.txid) {
            (true, Some(txid)) => Spend::SpentBy {
                txid,
                confirmed: r.status.is_some_and(|s| s.confirmed),
            },
            (true, None) => return Err(format!("outspend without txid: {body}")),
            (false, _) => Spend::Unspent,
        })
    }

    pub async fn tx_confirmed(&self, txid: &str) -> Result<bool, String> {
        let body = self.get_text(&format!("/tx/{txid}/status")).await?;
        let s: StatusResp =
            serde_json::from_str(&body).map_err(|e| format!("tx status: {e}: {body}"))?;
        Ok(s.confirmed)
    }

    pub async fn tip_height(&self) -> Result<u32, String> {
        let body = self.get_text("/blocks/tip/height").await?;
        body.trim()
            .parse()
            .map_err(|e| format!("tip height {body}: {e}"))
    }

    /// Fee rate (sat/vB) for confirmation within `target` blocks.
    pub async fn fee_rate(&self, target: u16) -> Result<f64, String> {
        let body = self.get_text("/fee-estimates").await?;
        let map: HashMap<String, f64> =
            serde_json::from_str(&body).map_err(|e| format!("fee-estimates: {e}"))?;
        // Closest estimate at or below the target (faster), else any.
        let mut best: Option<(u16, f64)> = None;
        for (k, v) in map {
            let Ok(k) = k.parse::<u16>() else { continue };
            if k <= target && best.is_none_or(|(bk, _)| k > bk) {
                best = Some((k, v));
            }
        }
        best.map(|(_, v)| v)
            .filter(|v| v.is_finite() && *v > 0.0)
            .ok_or_else(|| "no fee estimate".to_string())
    }

    /// Whether `address` currently holds no outputs (mempool included).
    pub async fn address_utxos_empty(&self, address: &str) -> Result<bool, String> {
        let body = self.get_text(&format!("/address/{address}/utxo")).await?;
        let utxos: Vec<serde_json::Value> =
            serde_json::from_str(&body).map_err(|e| format!("address utxo: {e}"))?;
        Ok(utxos.is_empty())
    }

    /// Whether any transaction ever touched `address`.
    pub async fn address_has_history(&self, address: &str) -> Result<bool, String> {
        let body = self.get_text(&format!("/address/{address}/txs")).await?;
        let txs: Vec<serde_json::Value> =
            serde_json::from_str(&body).map_err(|e| format!("address txs: {e}"))?;
        Ok(!txs.is_empty())
    }

    /// Broadcast and check the node answered with the expected txid. A
    /// transaction the node already has counts as success.
    pub async fn broadcast(&self, tx_hex: &str, txid: &str) -> Result<(), Broadcast> {
        let url = format!("{}/tx", self.base);
        let r = self
            .http
            .post(&url)
            .body(tx_hex.to_string())
            .send()
            .await
            .map_err(|e| Broadcast::Transport(format!("POST {url}: {e}")))?;
        let status = r.status();
        let body = r.text().await.unwrap_or_default();
        let body = body.trim();
        if status.is_success() && body == txid {
            return Ok(());
        }
        if status.is_success() {
            return Err(Broadcast::Transport(format!(
                "unexpected broadcast reply: {body}"
            )));
        }
        if already_known(body) {
            return Ok(());
        }
        if status.is_client_error() {
            Err(Broadcast::Rejected(format!("{status}: {body}")))
        } else {
            Err(Broadcast::Transport(format!("{status}: {body}")))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Broadcast {
    /// The node refused the transaction (conflict, fee, non-final…).
    Rejected(String),
    /// We couldn't reach the node; try again later.
    Transport(String),
}

impl std::fmt::Display for Broadcast {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Broadcast::Rejected(m) => write!(f, "broadcast rejected: {m}"),
            Broadcast::Transport(m) => write!(f, "broadcast failed: {m}"),
        }
    }
}

fn already_known(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    [
        "already in block chain",
        "already in utxo set",
        "txn-already-in-mempool",
        "txn-already-known",
        "already have transaction",
    ]
    .iter()
    .any(|s| b.contains(s))
}

#[cfg(test)]
mod tests {
    #[test]
    fn already_known_messages() {
        assert!(super::already_known(
            r#"sendrawtransaction RPC error: {"code":-27,"message":"Transaction already in block chain"}"#
        ));
        assert!(super::already_known(
            r#"sendrawtransaction RPC error: {"code":-26,"message":"txn-already-in-mempool"}"#
        ));
        assert!(!super::already_known(
            r#"sendrawtransaction RPC error: {"code":-26,"message":"min relay fee not met"}"#
        ));
        assert!(!super::already_known("bad-txns-inputs-missingorspent"));
    }
}

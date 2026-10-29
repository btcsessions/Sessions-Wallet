//! Your own node: test a server before saving it, and whether the wallet
//! can reach its chain server right now.

use rail_onchain::electrum::{self, ServerUrl};
use rail_onchain::ChainServer;
use serde::Serialize;
use sessions_core::{Error, Result};
use tauri::State;

use crate::state::AppState;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionTest {
    pub ok: bool,
    /// The address in full (`tcp://host:50001`): what to save. When the
    /// address was typed without `ssl://`/`tcp://` and the guess was wrong,
    /// this is the one that worked.
    pub url: Option<String>,
    pub height: Option<usize>,
    pub encrypted: bool,
    pub millis: u64,
    pub error: Option<String>,
}

/// Try an Electrum server (as typed in Settings) without saving it.
#[tauri::command]
pub async fn test_connection(url: String) -> Result<ConnectionTest> {
    let first = ServerUrl::parse(&url)?;
    // A scheme (or Electrum's :s / :t) says which transport; otherwise the
    // other one is worth a try too.
    let t = url.trim();
    let explicit = t.contains("://") || t.ends_with(":s") || t.ends_with(":t");
    let mut tries = vec![first.clone()];
    if !explicit {
        tries.push(first.other_transport());
    }
    tokio::task::spawn_blocking(move || {
        let mut first_error = None;
        for server in tries {
            let started = std::time::Instant::now();
            match electrum::probe(&server) {
                Ok(height) => {
                    return ConnectionTest {
                        ok: true,
                        url: Some(server.to_string()),
                        height: Some(height),
                        encrypted: server.tls,
                        millis: started.elapsed().as_millis() as u64,
                        error: None,
                    }
                }
                Err(e) => {
                    first_error.get_or_insert_with(|| electrum::explain(&server, &e));
                }
            }
        }
        ConnectionTest {
            ok: false,
            url: None,
            height: None,
            encrypted: false,
            millis: 0,
            error: first_error,
        }
    })
    .await
    .map_err(|e| Error::Network(e.to_string()))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainStatus {
    /// Your own server (not the public defaults).
    pub custom: bool,
    /// What's being used: your server, or "public servers".
    pub server: String,
    pub connected: bool,
    pub height: Option<usize>,
    pub error: Option<String>,
}

/// Can the wallet reach its chain server right now?
#[tauri::command]
pub async fn chain_status(state: State<'_, AppState>) -> Result<ChainStatus> {
    let config = {
        let guard = state.inner.lock().await;
        guard.as_ref().ok_or(Error::Locked)?.config.clone()
    };
    let custom = config.electrum_url.is_some() || config.esplora_url.is_some();
    match config.chain_server()? {
        ChainServer::Electrum(servers) => {
            let label = match (custom, servers.first()) {
                (true, Some(s)) => s.to_string(),
                _ => "public servers".into(),
            };
            let pool = electrum::pool_for(&servers)?;
            let first = servers.first().cloned();
            let r = tokio::task::spawn_blocking(move || pool.conn().tip_height())
                .await
                .map_err(|e| Error::Network(e.to_string()))?;
            Ok(match r {
                Ok(h) => ChainStatus {
                    custom,
                    server: label,
                    connected: true,
                    height: Some(h),
                    error: None,
                },
                Err(e) => ChainStatus {
                    custom,
                    server: label,
                    connected: false,
                    height: None,
                    error: Some(match (custom, first) {
                        (true, Some(s)) => electrum::explain(&s, &e),
                        _ => format!("Couldn't reach the bitcoin network: {e}"),
                    }),
                },
            })
        }
        ChainServer::Esplora(url) => {
            let tip = format!("{}/blocks/tip/height", url.trim_end_matches('/'));
            let r = async {
                let resp = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()?
                    .get(&tip)
                    .send()
                    .await?
                    .error_for_status()?;
                resp.text().await
            }
            .await;
            Ok(match r.map(|t| t.trim().parse::<usize>().ok()) {
                Ok(Some(h)) => ChainStatus {
                    custom,
                    server: url,
                    connected: true,
                    height: Some(h),
                    error: None,
                },
                Ok(None) => ChainStatus {
                    custom,
                    server: url.clone(),
                    connected: false,
                    height: None,
                    error: Some(format!("{url} answered, but not like an Esplora server")),
                },
                Err(e) => ChainStatus {
                    custom,
                    server: url.clone(),
                    connected: false,
                    height: None,
                    error: Some(format!("Couldn't reach {url}: {e}")),
                },
            })
        }
    }
}

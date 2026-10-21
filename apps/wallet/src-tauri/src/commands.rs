//! Tauri commands. Names and argument names match `src/api/tauri.ts`.
//! Every command returns `Result<_, sessions_core::Error>`, which serialises
//! to a plain message string for the UI.

use std::sync::Arc;

use rail_onchain::descriptors;
use serde::{Deserialize, Serialize};
use sessions_core::keystore::Seed;
use sessions_core::modules::Layout;
use sessions_core::planner::Plan;
use sessions_core::wallet::{Execution, Prepared};
use sessions_core::{
    parse, AccountId, AccountInfo, Error, FeePriority, Method, Payment, PaymentRequest, ReceiveTarget, Result, Sats, Wallet,
};
use tauri::State;

use crate::config::AccountSpec;
use crate::state::AppState;

async fn wallet(state: &AppState) -> Result<Arc<Wallet>> {
    Ok(state.inner.lock().await.as_ref().ok_or(Error::Locked)?.wallet.clone())
}

#[tauri::command]
pub async fn status(state: State<'_, AppState>) -> Result<&'static str> {
    Ok(if state.inner.lock().await.is_some() {
        "unlocked"
    } else if state.has_vault() {
        "locked"
    } else {
        "empty"
    })
}

#[tauri::command]
pub async fn create_wallet(state: State<'_, AppState>, passcode: String, words: usize) -> Result<String> {
    check_passcode(&passcode)?;
    let seed = Seed::generate(if words == 24 { 24 } else { 12 })?;
    let phrase = seed.phrase().to_string();
    state.create(seed, &passcode).await?;
    // Shown once, during onboarding, so the user can write it down.
    Ok(phrase)
}

#[tauri::command]
pub async fn restore_wallet(state: State<'_, AppState>, phrase: String, passcode: String) -> Result<()> {
    check_passcode(&passcode)?;
    let seed = Seed::from_phrase(&phrase)?;
    state.create(seed, &passcode).await
}

#[tauri::command]
pub async fn unlock(state: State<'_, AppState>, passcode: String) -> Result<()> {
    let seed = state.open_vault(&passcode).await?;
    state.unlock_with(seed).await
}

#[tauri::command]
pub async fn lock(state: State<'_, AppState>) -> Result<()> {
    state.lock().await;
    Ok(())
}

#[tauri::command]
pub async fn reveal_phrase(state: State<'_, AppState>, passcode: String) -> Result<String> {
    // Always re-check the passcode, even when unlocked.
    let seed = state.open_vault(&passcode).await?;
    Ok(seed.phrase().to_string())
}

fn check_passcode(p: &str) -> Result<()> {
    if p.len() < 6 {
        return Err(Error::Invalid("Use a passcode of at least 6 digits".into()));
    }
    Ok(())
}

#[tauri::command]
pub async fn accounts(state: State<'_, AppState>) -> Result<Vec<AccountInfo>> {
    wallet(&state).await?.accounts().await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewAccount {
    kind: String,
    name: String,
    threshold: Option<u8>,
    cosigners: Option<Vec<String>>,
    descriptor: Option<String>,
    device: Option<String>,
    mint_url: Option<String>,
}

#[tauri::command]
pub async fn add_account(state: State<'_, AppState>, spec: NewAccount) -> Result<AccountInfo> {
    let spec = {
        let mut guard = state.inner.lock().await;
        let u = guard.as_mut().ok_or(Error::Locked)?;
        let network = u.config.network;
        let name = spec.name.trim().to_string();
        match spec.kind.as_str() {
            "multisig" => {
                let local_account = u.config.next_multisig_account;
                let master = u.seed.master_xpriv(network)?;
                let mut keys = vec![descriptors::multisig_cosigner_key(&master, network, local_account)?];
                keys.extend(spec.cosigners.unwrap_or_default().into_iter().map(|k| k.trim().to_string()));
                let threshold = spec.threshold.unwrap_or(2);
                let descriptors = descriptors::multisig(threshold, &keys)?;
                u.config.next_multisig_account += 1;
                AccountSpec::Multisig {
                    id: u.config.fresh_id("vault"),
                    name,
                    threshold,
                    total: keys.len() as u8,
                    descriptors,
                    local_account,
                }
            }
            "hardware" | "watchOnly" => {
                let d = descriptors::import(spec.descriptor.as_deref().unwrap_or(""), network)?;
                let fingerprint = descriptors::fingerprints(&d.external)?.into_iter().next().unwrap_or_default();
                if spec.kind == "hardware" {
                    AccountSpec::Hardware {
                        id: u.config.fresh_id("hw"),
                        name,
                        device: spec.device.unwrap_or_else(|| "Hardware wallet".into()),
                        fingerprint,
                        descriptors: d,
                    }
                } else {
                    AccountSpec::WatchOnly { id: u.config.fresh_id("watch"), name, descriptors: d }
                }
            }
            "ecash" => {
                let mint_url = spec.mint_url.unwrap_or_default();
                if !mint_url.starts_with("https://") {
                    return Err(Error::Invalid("Mint addresses must start with https://".into()));
                }
                AccountSpec::Ecash { id: u.config.fresh_id("ecash"), name, mint_url }
            }
            other => return Err(Error::Unsupported(format!("add a {other} account"))),
        }
    };
    let id = state.add_account(spec).await?;
    wallet(&state).await?.account(&id).await?.info().await
}

#[tauri::command]
pub async fn cosigner_key(state: State<'_, AppState>) -> Result<String> {
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(Error::Locked)?;
    let master = u.seed.master_xpriv(u.config.network)?;
    descriptors::multisig_cosigner_key(&master, u.config.network, u.config.next_multisig_account)
}

#[tauri::command]
pub async fn sync(state: State<'_, AppState>) -> Result<()> {
    let results = wallet(&state).await?.sync_all().await;
    // Report the first failure but don't hide progress made on other accounts.
    match results.into_iter().find_map(|(id, r)| r.err().map(|e| (id, e))) {
        Some((id, e)) => Err(Error::Rail { rail: id.0, message: e.to_string() }),
        None => Ok(()),
    }
}

#[tauri::command]
pub async fn activity(state: State<'_, AppState>, limit: usize) -> Result<Vec<Payment>> {
    wallet(&state).await?.activity(limit.min(500)).await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Parsed {
    request: PaymentRequest,
    amount_sat: Option<Sats>,
}

#[tauri::command]
pub async fn parse(input: String) -> Result<Parsed> {
    let request = parse::parse(&input)?;
    Ok(Parsed { amount_sat: request.amount_sat(), request })
}

#[tauri::command]
pub async fn plan_payment(state: State<'_, AppState>, input: String, amount_sat: Option<Sats>, from: Option<AccountId>) -> Result<Plan> {
    wallet(&state).await?.plan_payment(&input, amount_sat, from).await
}

#[tauri::command]
pub async fn plan_move(state: State<'_, AppState>, from: AccountId, to: AccountId, amount_sat: Sats) -> Result<Plan> {
    wallet(&state).await?.plan_move(&from, &to, amount_sat).await
}

#[tauri::command]
pub async fn prepare(state: State<'_, AppState>, plan: Plan, fee: FeePriority) -> Result<Prepared> {
    wallet(&state).await?.prepare(plan, fee).await
}

#[tauri::command]
pub async fn confirm(state: State<'_, AppState>, prepared: Prepared, fee: FeePriority) -> Result<Execution> {
    wallet(&state).await?.confirm(prepared, fee).await
}

#[tauri::command]
pub async fn resume(state: State<'_, AppState>, paused: Execution, signed_psbt: String, fee: FeePriority) -> Result<Execution> {
    wallet(&state).await?.resume(paused, signed_psbt, fee).await
}

#[tauri::command]
pub async fn receive(
    state: State<'_, AppState>,
    into: AccountId,
    method: Method,
    amount_sat: Option<Sats>,
    description: Option<String>,
) -> Result<ReceiveTarget> {
    wallet(&state).await?.receive(&into, method, amount_sat, description).await
}

#[tauri::command]
pub async fn redeem(state: State<'_, AppState>, token: String, into: Option<AccountId>) -> Result<Payment> {
    wallet(&state).await?.redeem(&token, into).await
}

#[tauri::command]
pub async fn create_token(state: State<'_, AppState>, from: AccountId, amount_sat: Sats, memo: Option<String>) -> Result<Payment> {
    wallet(&state).await?.create_token(&from, amount_sat, memo).await
}

#[tauri::command]
pub async fn layout(state: State<'_, AppState>) -> Result<Layout> {
    Ok(state.inner.lock().await.as_ref().ok_or(Error::Locked)?.config.layout.clone())
}

#[tauri::command]
pub async fn save_layout(state: State<'_, AppState>, layout: Layout) -> Result<()> {
    let mut guard = state.inner.lock().await;
    let u = guard.as_mut().ok_or(Error::Locked)?;
    u.config.layout = layout;
    u.config.save(&state.paths.config())
}

#[derive(Serialize)]
pub struct FeeRates {
    fast: f32,
    normal: f32,
    slow: f32,
}

#[tauri::command]
pub async fn fee_rates(state: State<'_, AppState>) -> Result<FeeRates> {
    let account = {
        let guard = state.inner.lock().await;
        let u = guard.as_ref().ok_or(Error::Locked)?;
        u.onchain.values().next().cloned().ok_or(Error::NoRoute)?
    };
    let (fast, normal, slow) = account.fee_rates().await?;
    Ok(FeeRates { fast, normal, slow })
}

/// Spot price from mempool.space (or the user's own mempool instance).
#[tauri::command]
pub async fn btc_price(state: State<'_, AppState>, currency: String) -> Result<Option<f64>> {
    let base = {
        let guard = state.inner.lock().await;
        let u = guard.as_ref().ok_or(Error::Locked)?;
        u.config.esplora_url.clone().unwrap_or_else(|| "https://mempool.space/api".into())
    };
    let url = format!("{}/v1/prices", base.trim_end_matches('/'));
    let res = reqwest::Client::new()
        .get(url)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .and_then(|r| r.error_for_status());
    let Ok(res) = res else { return Ok(None) };
    let json: serde_json::Value = res.json().await.unwrap_or_default();
    Ok(json.get(currency.to_uppercase()).and_then(|v| v.as_f64()).filter(|p| *p > 0.0))
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    esplora_url: Option<String>,
    /// Never sent back to the UI; only whether one is set.
    #[serde(default, skip_serializing)]
    breez_api_key: Option<String>,
    #[serde(default)]
    has_breez_key: bool,
}

#[tauri::command]
pub async fn settings(state: State<'_, AppState>) -> Result<Settings> {
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(Error::Locked)?;
    Ok(Settings { esplora_url: u.config.esplora_url.clone(), breez_api_key: None, has_breez_key: u.config.breez_api_key.is_some() })
}

/// Saved now, applied the next time the wallet unlocks.
#[tauri::command]
pub async fn save_settings(state: State<'_, AppState>, settings: Settings) -> Result<()> {
    let mut guard = state.inner.lock().await;
    let u = guard.as_mut().ok_or(Error::Locked)?;
    u.config.esplora_url = match settings.esplora_url.map(|s| s.trim().trim_end_matches('/').to_string()) {
        Some(url) if url.is_empty() => None,
        Some(url) if url.starts_with("https://") || url.starts_with("http://") => Some(url),
        Some(_) => return Err(Error::Invalid("Server addresses start with https:// or http://".into())),
        None => None,
    };
    if let Some(key) = settings.breez_api_key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty()) {
        u.config.breez_api_key = Some(key);
    }
    u.config.save(&state.paths.config())
}

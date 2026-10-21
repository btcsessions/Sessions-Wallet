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
    parse, AccountId, AccountInfo, Error, FeePriority, Method, Payment, PaymentRequest,
    ReceiveTarget, Result, Sats, Wallet,
};
use tauri::State;

use crate::config::AccountSpec;
use crate::state::AppState;

async fn wallet(state: &AppState) -> Result<Arc<Wallet>> {
    Ok(state
        .inner
        .lock()
        .await
        .as_ref()
        .ok_or(Error::Locked)?
        .wallet
        .clone())
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
pub async fn create_wallet(
    state: State<'_, AppState>,
    passcode: String,
    words: usize,
) -> Result<String> {
    check_passcode(&passcode)?;
    let seed = Seed::generate(if words == 24 { 24 } else { 12 })?;
    let phrase = seed.phrase().to_string();
    state.create(seed, &passcode).await?;
    // Shown once, during onboarding, so the user can write it down.
    Ok(phrase)
}

#[tauri::command]
pub async fn restore_wallet(
    state: State<'_, AppState>,
    phrase: String,
    passcode: String,
) -> Result<()> {
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
                let mut keys = vec![descriptors::multisig_cosigner_key(
                    &master,
                    network,
                    local_account,
                )?];
                keys.extend(
                    spec.cosigners
                        .unwrap_or_default()
                        .into_iter()
                        .map(|k| k.trim().to_string()),
                );
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
                let fingerprint = descriptors::fingerprints(&d.external)?
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                if spec.kind == "hardware" {
                    AccountSpec::Hardware {
                        id: u.config.fresh_id("hw"),
                        name,
                        device: spec.device.unwrap_or_else(|| "Hardware wallet".into()),
                        fingerprint,
                        descriptors: d,
                    }
                } else {
                    AccountSpec::WatchOnly {
                        id: u.config.fresh_id("watch"),
                        name,
                        descriptors: d,
                    }
                }
            }
            "savings" => {
                // Hot on-chain wallet (BIP84). Opt-in only.
                let used: Vec<u32> = u
                    .config
                    .accounts
                    .iter()
                    .filter_map(|a| match a {
                        AccountSpec::Savings { account_index, .. } => Some(*account_index),
                        _ => None,
                    })
                    .collect();
                let account_index = (0..).find(|i| !used.contains(i)).unwrap_or(0);
                let id = if account_index == 0
                    && !u.config.accounts.iter().any(|a| a.id().0 == "savings")
                {
                    AccountId("savings".into())
                } else {
                    u.config.fresh_id("savings")
                };
                AccountSpec::Savings {
                    id,
                    name,
                    account_index,
                    payjoin_receive: false,
                }
            }
            "ecash" => {
                let mint_url = spec.mint_url.unwrap_or_default();
                if !mint_url.starts_with("https://") {
                    return Err(Error::Invalid(
                        "Mint addresses must start with https://".into(),
                    ));
                }
                AccountSpec::Ecash {
                    id: u.config.fresh_id("ecash"),
                    name,
                    mint_url,
                }
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
    // Report the first real failure but don't hide progress made on other
    // accounts. A slow server isn't a failure: it finishes next time.
    match results
        .into_iter()
        .filter_map(|(id, r)| r.err().map(|e| (id, e)))
        .find(|(_, e)| !e.to_string().contains("sync took longer than"))
    {
        Some((id, e)) => Err(Error::Rail {
            rail: id.0,
            message: e.to_string(),
        }),
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
    Ok(Parsed {
        amount_sat: request.amount_sat(),
        request,
    })
}

#[tauri::command]
pub async fn plan_payment(
    state: State<'_, AppState>,
    input: String,
    amount_sat: Option<Sats>,
    from: Option<AccountId>,
) -> Result<Plan> {
    wallet(&state)
        .await?
        .plan_payment(&input, amount_sat, from)
        .await
}

#[tauri::command]
pub async fn plan_move(
    state: State<'_, AppState>,
    from: AccountId,
    to: AccountId,
    amount_sat: Sats,
) -> Result<Plan> {
    wallet(&state)
        .await?
        .plan_move(&from, &to, amount_sat)
        .await
}

#[tauri::command]
pub async fn prepare(
    state: State<'_, AppState>,
    plan: Plan,
    fee: FeePriority,
    style: Option<sessions_core::SpendStyle>,
) -> Result<Prepared> {
    wallet(&state)
        .await?
        .prepare_styled(plan, fee, style.unwrap_or_default())
        .await
}

#[tauri::command]
pub async fn confirm(
    state: State<'_, AppState>,
    prepared: Prepared,
    fee: FeePriority,
) -> Result<Execution> {
    wallet(&state).await?.confirm(prepared, fee).await
}

#[tauri::command]
pub async fn resume(
    state: State<'_, AppState>,
    paused: Execution,
    signed_psbt: String,
    fee: FeePriority,
) -> Result<Execution> {
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
    wallet(&state)
        .await?
        .receive(&into, method, amount_sat, description)
        .await
}

#[tauri::command]
pub async fn redeem(
    state: State<'_, AppState>,
    token: String,
    into: Option<AccountId>,
) -> Result<Payment> {
    wallet(&state).await?.redeem(&token, into).await
}

#[tauri::command]
pub async fn create_token(
    state: State<'_, AppState>,
    from: AccountId,
    amount_sat: Sats,
    memo: Option<String>,
) -> Result<Payment> {
    wallet(&state)
        .await?
        .create_token(&from, amount_sat, memo)
        .await
}

#[tauri::command]
pub async fn layout(state: State<'_, AppState>) -> Result<Layout> {
    Ok(state
        .inner
        .lock()
        .await
        .as_ref()
        .ok_or(Error::Locked)?
        .config
        .layout
        .clone())
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
    let (account, base) = {
        let guard = state.inner.lock().await;
        let u = guard.as_ref().ok_or(Error::Locked)?;
        let base = u
            .config
            .esplora_url
            .clone()
            .unwrap_or_else(|| rail_onchain::default_esplora(u.config.network).to_string());
        (u.onchain.values().next().cloned(), base)
    };
    if let Some(account) = account {
        let (fast, normal, slow) = account.fee_rates().await?;
        return Ok(FeeRates { fast, normal, slow });
    }
    // No on-chain account: ask the chain server directly (Esplora fee-estimates,
    // keyed by confirmation target in blocks).
    let est: std::collections::HashMap<String, f32> = reqwest::Client::new()
        .get(format!("{}/fee-estimates", base.trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| Error::rail("fees", e))?
        .json()
        .await
        .map_err(|e| Error::rail("fees", e))?;
    let at = |target: u32| {
        est.iter()
            .filter_map(|(k, v)| {
                k.parse::<u32>()
                    .ok()
                    .filter(|t| *t <= target)
                    .map(|t| (t, *v))
            })
            .max_by_key(|(t, _)| *t)
            .map(|(_, v)| v.max(1.0))
            .unwrap_or(1.0)
    };
    Ok(FeeRates {
        fast: at(1),
        normal: at(6),
        slow: at(144),
    })
}

/// Spot price from mempool.space (or the user's own mempool instance).
#[tauri::command]
pub async fn btc_price(state: State<'_, AppState>, currency: String) -> Result<Option<f64>> {
    let base = {
        let guard = state.inner.lock().await;
        let u = guard.as_ref().ok_or(Error::Locked)?;
        u.config
            .esplora_url
            .clone()
            .unwrap_or_else(|| "https://mempool.space/api".into())
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
    Ok(json
        .get(currency.to_uppercase())
        .and_then(|v| v.as_f64())
        .filter(|p| *p > 0.0))
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
    Ok(Settings {
        esplora_url: u.config.esplora_url.clone(),
        breez_api_key: None,
        has_breez_key: u.config.breez_api_key.is_some(),
    })
}

/// Saved now, applied the next time the wallet unlocks.
#[tauri::command]
pub async fn save_settings(state: State<'_, AppState>, settings: Settings) -> Result<()> {
    let mut guard = state.inner.lock().await;
    let u = guard.as_mut().ok_or(Error::Locked)?;
    u.config.esplora_url = match settings
        .esplora_url
        .map(|s| s.trim().trim_end_matches('/').to_string())
    {
        Some(url) if url.is_empty() => None,
        Some(url) if url.starts_with("https://") || url.starts_with("http://") => Some(url),
        Some(_) => {
            return Err(Error::Invalid(
                "Server addresses start with https:// or http://".into(),
            ))
        }
        None => None,
    };
    if let Some(key) = settings
        .breez_api_key
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
    {
        u.config.breez_api_key = Some(key);
    }
    u.config.save(&state.paths.config())
}

#[derive(Serialize)]
pub struct Unavailable {
    id: AccountId,
    reason: String,
}

/// Accounts that couldn't be opened at unlock (offline, missing key…).
#[tauri::command]
pub async fn unavailable_accounts(state: State<'_, AppState>) -> Result<Vec<Unavailable>> {
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(Error::Locked)?;
    Ok(u.unavailable
        .iter()
        .map(|(id, reason)| Unavailable {
            id: id.clone(),
            reason: reason.clone(),
        })
        .collect())
}

async fn spark_account(state: &AppState, id: &AccountId) -> Result<Arc<rail_spark::SparkAccount>> {
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(Error::Locked)?;
    u.spark
        .get(id)
        .cloned()
        .ok_or_else(|| Error::Unsupported("have a Lightning address".into()))
}

#[tauri::command]
pub async fn lightning_address(
    state: State<'_, AppState>,
    account: AccountId,
) -> Result<Option<rail_spark::LightningAddress>> {
    spark_account(&state, &account)
        .await?
        .lightning_address()
        .await
}

fn check_username(username: &str) -> Result<String> {
    let u = username.trim().to_lowercase();
    let ok = (2..=32).contains(&u.len())
        && u.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' || c == '.'
        });
    if !ok {
        return Err(Error::Invalid(
            "Use 2 to 32 letters, numbers, dots, dashes or underscores".into(),
        ));
    }
    Ok(u)
}

#[tauri::command]
pub async fn lightning_address_available(
    state: State<'_, AppState>,
    account: AccountId,
    username: String,
) -> Result<bool> {
    let username = check_username(&username)?;
    spark_account(&state, &account)
        .await?
        .lightning_address_available(&username)
        .await
}

#[tauri::command]
pub async fn register_lightning_address(
    state: State<'_, AppState>,
    account: AccountId,
    username: String,
) -> Result<rail_spark::LightningAddress> {
    let username = check_username(&username)?;
    spark_account(&state, &account)
        .await?
        .register_lightning_address(&username)
        .await
}

/// Remove an account that holds nothing. Spending can't be removed.
/// Its keys still derive from the recovery phrase, so adding it back later
/// recovers any funds that arrive in the meantime.
#[tauri::command]
pub async fn remove_account(state: State<'_, AppState>, id: AccountId) -> Result<()> {
    if id.0 == "spending" {
        return Err(Error::Invalid("Spending can't be removed".into()));
    }
    let w = wallet(&state).await?;
    let info = w.account(&id).await?.info().await?;
    let b = info.balance;
    if b.confirmed + b.pending_in + b.pending_out > 0 {
        return Err(Error::Invalid(
            "Move the funds out first. Only empty accounts can be removed".into(),
        ));
    }
    w.remove_account(&id).await;
    let mut guard = state.inner.lock().await;
    let u = guard.as_mut().ok_or(Error::Locked)?;
    u.config.accounts.retain(|a| a.id() != &id);
    u.onchain.remove(&id);
    u.spark.remove(&id);
    u.config
        .layout
        .modules
        .retain(|m| !m.id.ends_with(&format!(":{}", id.0)));
    u.config.save(&state.paths.config())
}

/// Whether a hot on-chain account offers payjoin when receiving.
#[tauri::command]
pub async fn payjoin_receive(state: State<'_, AppState>, account: AccountId) -> Result<bool> {
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(Error::Locked)?;
    Ok(u.config.accounts.iter().any(
        |a| matches!(a, AccountSpec::Savings { id, payjoin_receive: true, .. } if *id == account),
    ))
}

#[tauri::command]
pub async fn set_payjoin_receive(
    state: State<'_, AppState>,
    account: AccountId,
    enabled: bool,
) -> Result<()> {
    state.set_payjoin_receive(&account, enabled).await
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DescriptorPreview {
    descriptor: String,
    fingerprint: String,
    script_type: String,
    first_address: String,
}

/// Check an export from a hardware wallet before adding it.
#[tauri::command]
pub async fn inspect_descriptor(
    state: State<'_, AppState>,
    descriptor: String,
) -> Result<DescriptorPreview> {
    let network = state.network().await?;
    let d = descriptors::import(&descriptor, network)?;
    Ok(DescriptorPreview {
        fingerprint: descriptors::fingerprints(&d.external)?
            .into_iter()
            .next()
            .unwrap_or_default(),
        script_type: descriptors::script_label(&d).into(),
        first_address: descriptors::address_at(&d, network, 0)?,
        descriptor: d.external,
    })
}

/// Trezor Connect `signTransaction` parameters for an unsigned PSBT.
#[tauri::command]
pub async fn trezor_sign_request(
    state: State<'_, AppState>,
    psbt: String,
) -> Result<serde_json::Value> {
    let network = state.network().await?;
    rail_onchain::trezor::sign_request(&psbt, network)
}

/// The PSBT with the Trezor-signed transaction folded back in.
#[tauri::command]
pub async fn trezor_merge(psbt: String, signed_tx: String) -> Result<String> {
    rail_onchain::trezor::merge_signed_tx(&psbt, &signed_tx)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetailRow {
    label: String,
    value: String,
    /// Something to copy or check: shown in monospace with a copy button.
    mono: bool,
}

fn row(label: &str, value: impl Into<String>, mono: bool) -> DetailRow {
    DetailRow {
        label: label.into(),
        value: value.into(),
        mono,
    }
}

/// The xpub(s) in a descriptor, with their origins.
fn key_rows(descriptor: &str) -> Vec<DetailRow> {
    let mut out = Vec::new();
    let mut rest = descriptor;
    while let Some(start) = rest.find('[') {
        let tail = &rest[start..];
        let end = tail.find([',', ')']).unwrap_or(tail.len());
        let key = &tail[..end];
        let (origin, xpub) = key.split_once(']').unwrap_or(("", key));
        let xpub = xpub.split('/').next().unwrap_or(xpub);
        let origin = origin.trim_start_matches('[');
        let (fp, path) = origin.split_once('/').unwrap_or((origin, ""));
        out.push(row("Fingerprint", fp, true));
        if !path.is_empty() {
            out.push(row("Derivation path", format!("m/{path}"), true));
        }
        out.push(row("Extended public key", xpub, true));
        rest = &tail[end..];
    }
    out
}

fn descriptor_rows(d: &rail_onchain::DescriptorPair) -> Vec<DetailRow> {
    let mut out = vec![row("Address type", descriptors::script_label(d), false)];
    out.extend(key_rows(&d.external));
    // One line that restores this account in Sparrow, Bitcoin Core and others.
    out.push(row(
        "Descriptor",
        d.external.replacen("/0/*", "/<0;1>/*", usize::MAX),
        true,
    ));
    out
}

/// The technical details behind an account, for the "Wallet details" view.
#[tauri::command]
pub async fn account_details(state: State<'_, AppState>, id: AccountId) -> Result<Vec<DetailRow>> {
    let (spec, network, master) = {
        let guard = state.inner.lock().await;
        let u = guard.as_ref().ok_or(Error::Locked)?;
        let spec = u
            .config
            .accounts
            .iter()
            .find(|a| a.id() == &id)
            .cloned()
            .ok_or_else(|| Error::AccountNotFound(id.0.clone()))?;
        let network = u.config.network;
        (spec, network, u.seed.master_xpriv(network)?)
    };
    let info = wallet(&state).await?.account(&id).await?.info().await?;
    let mut rows = match &spec {
        AccountSpec::Savings { account_index, .. } => {
            let mut r = vec![row(
                "Keys",
                "On this device, from your recovery phrase",
                false,
            )];
            r.extend(descriptor_rows(&descriptors::hot_single_sig(
                &master,
                network,
                *account_index,
            )?));
            r
        }
        AccountSpec::Hardware {
            device,
            descriptors: d,
            ..
        } => {
            let mut r = vec![row("Device", device.as_str(), false)];
            r.extend(descriptor_rows(d));
            r
        }
        AccountSpec::Multisig {
            threshold,
            total,
            descriptors: d,
            ..
        } => {
            let mut r = vec![row("Policy", format!("{threshold} of {total} keys"), false)];
            r.extend(descriptor_rows(d));
            r
        }
        AccountSpec::WatchOnly { descriptors: d, .. } => {
            let mut r = vec![row("Keys", "None. This account can only watch", false)];
            r.extend(descriptor_rows(d));
            r
        }
        AccountSpec::Spending { .. } => vec![
            row("Network", "Lightning and Spark", false),
            row("Keys", "On this device, from your recovery phrase", false),
        ],
        AccountSpec::Ecash { mint_url, .. } => vec![
            row("Mint", mint_url.as_str(), true),
            row(
                "Keys",
                "Ecash notes held on this device; the mint holds the bitcoin",
                false,
            ),
        ],
    };
    let methods: Vec<String> = info.receives.iter().map(|m| format!("{m:?}")).collect();
    rows.push(row("Receives with", methods.join(", "), false));
    rows.push(row("Account id", id.0.as_str(), true));
    Ok(rows)
}

/// Limits and fees for receiving `method` into `into` through a swap, before
/// anything is created. `null` when the account receives `method` natively.
#[tauri::command]
pub async fn receive_quote(
    state: State<'_, AppState>,
    into: AccountId,
    method: Method,
    amount_sat: Option<Sats>,
) -> Result<Option<sessions_core::ReceiveQuote>> {
    wallet(&state)
        .await?
        .receive_quote(&into, method, amount_sat)
        .await
}

/// Chain details and speed-up / cancel options for an on-chain transaction.
/// `null` for accounts that aren't on-chain.
#[tauri::command]
pub async fn tx_details(
    state: State<'_, AppState>,
    account: AccountId,
    txid: String,
) -> Result<Option<sessions_core::TxDetails>> {
    wallet(&state).await?.tx_details(&account, &txid).await
}

/// Price speeding up or cancelling an unconfirmed on-chain transaction.
/// Send it with `confirm_quote`, or, when it `needsSignature`, with `resume`
/// on `{ payments: [], awaitingSignature: quote, remaining: [], amountSat: 0 }`.
#[tauri::command]
pub async fn prepare_bump(
    state: State<'_, AppState>,
    account: AccountId,
    txid: String,
    action: sessions_core::BumpAction,
    fee: FeePriority,
) -> Result<sessions_core::SendQuote> {
    wallet(&state)
        .await?
        .prepare_bump(&account, &txid, action, fee)
        .await
}

/// Send a standalone quote (e.g. from `prepare_bump`) from an account that
/// signs on this device.
#[tauri::command]
pub async fn confirm_quote(
    state: State<'_, AppState>,
    quote: sessions_core::SendQuote,
) -> Result<Payment> {
    wallet(&state).await?.confirm_quote(quote).await
}

/// Sync one account now. The Receive screen calls this every few seconds
/// while a request is showing, so a payment appears as soon as possible.
#[tauri::command]
pub async fn sync_account(state: State<'_, AppState>, id: AccountId) -> Result<()> {
    let rail = wallet(&state).await?.account(&id).await?;
    match tokio::time::timeout(std::time::Duration::from_secs(15), rail.sync()).await {
        Ok(r) => r,
        // Slow server: the next poll tries again.
        Err(_) => Ok(()),
    }
}

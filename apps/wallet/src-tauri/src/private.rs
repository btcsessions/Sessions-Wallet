//! Private Lightning (Advanced): send and receive through an ecash account
//! (see `sessions_core::private`).
//!
//! * Send: [`private_pay_prepare`] prices it and keeps the plan here (the
//!   webview gets an id, not the quote); [`private_pay`] runs it.
//! * Receive: [`private_receive`] makes an invoice at the mint and records
//!   where the sats should end up; [`tick`] (every 10 s while unlocked, from
//!   the background loop) moves them on once the payer has paid.
//!   Pending receives live in `private-receives.json` so a restart doesn't
//!   strand them; if moving on fails, the sats simply stay in ecash.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sessions_core::{AccountId, Error, Method, Payment, PrivatePay, ReceiveRequest, ReceiveTarget, Result, Sats};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::config::write_atomic;
use crate::state::AppState;

/// Prepared private sends, by id (single use, dropped after 10 minutes).
static PREPARED: Mutex<Option<HashMap<String, (PrivatePay, u64)>>> = Mutex::new(None);
const PREPARED_TTL: u64 = 600;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn random_id() -> String {
    let b: [u8; 12] = rand::random();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

async fn wallet(state: &AppState) -> Result<std::sync::Arc<sessions_core::Wallet>> {
    state.check_screen()?;
    Ok(state.inner.lock().await.as_ref().ok_or(Error::Locked)?.wallet.clone())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivateQuote {
    pub id: String,
    pub amount_sat: Sats,
    /// The most it can cost (unused fee reserve stays in ecash).
    pub max_fee_sat: Sats,
    pub from: AccountId,
    pub hub: AccountId,
}

/// Price a private Lightning payment from `from` (Spending) through the
/// ecash account `hub`.
#[tauri::command]
pub async fn private_pay_prepare(
    state: State<'_, AppState>,
    input: String,
    amount_sat: Option<Sats>,
    from: AccountId,
    hub: AccountId,
) -> Result<PrivateQuote> {
    let p = wallet(&state)
        .await?
        .prepare_private_pay(&input, amount_sat, &from, &hub)
        .await?;
    let id = random_id();
    let quote = PrivateQuote {
        id: id.clone(),
        amount_sat: p.amount_sat,
        max_fee_sat: p.max_fee_sat,
        from: p.from.clone(),
        hub: p.hub.clone(),
    };
    let mut g = PREPARED.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    let t = now();
    map.retain(|_, (_, at)| t.saturating_sub(*at) < PREPARED_TTL);
    map.insert(id, (p, t));
    Ok(quote)
}

/// Run a private payment priced by [`private_pay_prepare`].
#[tauri::command]
pub async fn private_pay(state: State<'_, AppState>, id: String) -> Result<Vec<Payment>> {
    let p = {
        let mut g = PREPARED.lock().unwrap_or_else(|e| e.into_inner());
        g.get_or_insert_with(HashMap::new)
            .remove(&id)
            .map(|(p, _)| p)
            .ok_or_else(|| Error::Invalid("That payment expired. Review it again".into()))?
    };
    let r = wallet(&state).await?.pay_private(p).await;
    state.backup.touch();
    r
}

/// Limits and fees for a private receive into `into` (on-chain accounts;
/// `null` for accounts that take Lightning themselves).
#[tauri::command]
pub async fn private_receive_quote(
    state: State<'_, AppState>,
    into: AccountId,
    amount_sat: Option<Sats>,
) -> Result<Option<sessions_core::ReceiveQuote>> {
    wallet(&state).await?.private_receive_quote(&into, amount_sat).await
}

/// A private receive waiting for the payer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pending {
    hub: AccountId,
    quote: String,
    into: AccountId,
    amount_sat: Sats,
    /// Stop watching after this (Unix seconds).
    until: u64,
}

fn path(state: &AppState) -> std::path::PathBuf {
    state.paths.root.join("private-receives.json")
}

fn load(state: &AppState) -> Vec<Pending> {
    std::fs::read(path(state))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn store(state: &AppState, list: &[Pending]) {
    if let Ok(b) = serde_json::to_vec(list) {
        let _ = write_atomic(&path(state), &b);
    }
}

/// Receive privately: the payer pays the mint (`hub`), and the sats move on
/// to `into` once they arrive.
#[tauri::command]
pub async fn private_receive(
    state: State<'_, AppState>,
    into: AccountId,
    hub: AccountId,
    amount_sat: Sats,
    description: Option<String>,
) -> Result<ReceiveTarget> {
    let w = wallet(&state).await?;
    // Into an on-chain account: what's left after the mint's cut must still
    // be enough for the move on-chain.
    if let Some(q) = w.private_receive_quote(&into, Some(amount_sat)).await? {
        if amount_sat < q.min_sat {
            return Err(Error::Invalid(format!(
                "Ask for at least {} sats: after the mint's fee, moving it on-chain needs that much",
                q.min_sat
            )));
        }
    }
    let mut target = w
        .account(&hub)
        .await?
        .receive(ReceiveRequest {
            method: Method::Bolt11,
            amount_sat: Some(amount_sat),
            description,
        })
        .await?;
    // The mint's cut (a guess; the rest of it stays in ecash).
    target.fee_sat = Some(amount_sat - sessions_core::private_forward_amount(amount_sat));
    let quote = target
        .watch_id
        .clone()
        .ok_or_else(|| Error::Invalid("the mint didn't give a quote id".into()))?;
    let mut list = load(&state);
    list.push(Pending {
        hub,
        quote,
        into,
        amount_sat,
        // A day past the invoice's expiry: late payments still move on.
        until: target.expires_at.unwrap_or_else(|| now() + 3600) + 24 * 3600,
    });
    store(&state, &list);
    Ok(target)
}

/// Move on any private receives that have been paid. Called from the
/// background loop; does nothing while locked or with nothing pending.
pub async fn tick(app: &AppHandle) {
    let state = app.state::<AppState>();
    let list = load(&state);
    if list.is_empty() {
        return;
    }
    let Some(w) = state.inner.lock().await.as_ref().map(|u| u.wallet.clone()) else {
        return;
    };
    let mut keep = Vec::new();
    let mut moved = false;
    for p in list {
        match w.forward_private_receive(&p.hub, &p.quote, &p.into, p.amount_sat).await {
            Ok(Some(_)) => moved = true,
            Ok(None) if now() < p.until => keep.push(p),
            Ok(None) => {}
            Err(e) => {
                // The sats are safe in ecash; try again on the next round
                // unless it has been going on for too long.
                tracing::warn!("private receive: {e}");
                if now() < p.until {
                    keep.push(p);
                }
            }
        }
    }
    store(&state, &keep);
    if moved {
        state.backup.touch();
        let _ = app.emit("wallet-changed", ());
    }
}

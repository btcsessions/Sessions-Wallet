//! Nostr Wallet Connect: apps (Primal, Damus, Amethyst…) that may use the
//! Spending account within a budget. The protocol lives in `sessions_nwc`;
//! the Spending side in `rail_spark::nwc`. Here: the saved connections, the
//! running service, and the commands the UI uses.
//!
//! Connections are saved in the config (so they're in the encrypted backup
//! and come back on a restore). Each keeps its connection link, which holds
//! the app's secret, so the UI can show the code again.

use std::collections::BTreeSet;
use std::sync::Arc;

use rail_spark::nwc::SparkNwc;
use serde::{Deserialize, Serialize};
use sessions_core::{Error, Result};
use sessions_nwc::{
    Budget, BudgetPeriod, Connection, ConnectionOptions, ConnectionStore, Permission, ServiceConfig,
};
use tauri::State;

use crate::state::{AppState, Unlocked};

pub type Handle = sessions_nwc::ServiceHandle<SparkNwc>;

/// A saved connection and its link.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NwcEntry {
    pub connection: Connection,
    pub uri: String,
}

/// What the UI shows (no keys).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub id: String,
    pub name: String,
    pub created_at: u64,
    pub last_used_at: Option<u64>,
    pub enabled: bool,
    pub permissions: Vec<Permission>,
    pub budget_sat: Option<u64>,
    pub budget_period: BudgetPeriod,
    pub max_per_payment_sat: Option<u64>,
    /// Spent in the current budget period, fees included.
    pub spent_sat: u64,
    pub relay: String,
}

fn view(c: &Connection) -> View {
    let mut usage = c.usage;
    usage.roll(c.budget.period, c.created_at, sessions_nwc::now());
    View {
        id: c.id.clone(),
        name: c.name.clone(),
        created_at: c.created_at,
        last_used_at: c.last_used_at,
        enabled: c.enabled,
        permissions: c.permissions.iter().copied().collect(),
        budget_sat: c.budget.limit_sat,
        budget_period: c.budget.period,
        max_per_payment_sat: c.budget.max_per_payment_sat,
        spent_sat: usage.spent_msat / 1000,
        relay: c.relays.first().cloned().unwrap_or_default(),
    }
}

/// The connections as the running service knows them (freshest budget use),
/// else as saved.
fn current(u: &Unlocked) -> Vec<Connection> {
    match &u.nwc {
        Some(h) => {
            let live = h.connections();
            u.config
                .nwc
                .iter()
                .map(|e| {
                    let mut c = e.connection.clone();
                    if let Some(l) = live.iter().find(|l| l.id == c.id) {
                        merge_usage(&mut c, l);
                    }
                    c
                })
                .collect()
        }
        None => u.config.nwc.iter().map(|e| e.connection.clone()).collect(),
    }
}

/// Start, update or stop the service to match the saved connections. Runs
/// only with a Spending account and at least one active connection.
pub fn sync_service(u: &mut Unlocked, store: Arc<dyn ConnectionStore>) {
    let connections: Vec<Connection> = current(u);
    let any_active = connections.iter().any(|c| c.is_active(sessions_nwc::now()));
    let spending = u
        .forwarder
        .as_ref()
        .and_then(|id| u.spark.get(id))
        .or_else(|| u.spark.values().next())
        .cloned();
    let Some(spending) = spending.filter(|_| any_active) else {
        if u.nwc.take().is_some() {
            tracing::info!("nwc: stopped");
        }
        return;
    };
    match &u.nwc {
        Some(h) => {
            h.set_connections(connections);
        }
        None => {
            if tokio::runtime::Handle::try_current().is_err() {
                return;
            }
            tracing::info!(count = connections.len(), "nwc: starting");
            let wallet = Arc::new(SparkNwc::new(spending, u.config.network));
            u.nwc = Some(sessions_nwc::start(
                wallet,
                connections,
                store,
                ServiceConfig::default(),
            ));
        }
    }
}

/// Fold the service's report into a saved connection: only what the
/// service tracks (budget use, last use, recent payments), never the user's
/// settings, and never less spent than already saved for the period (a late
/// report from a stopped service mustn't undo a newer one).
fn merge_usage(saved: &mut Connection, from: &Connection) {
    saved.last_used_at = saved.last_used_at.max(from.last_used_at);
    saved.recent = from.recent.clone();
    let (a, b) = (&mut saved.usage, &from.usage);
    if b.period_start > a.period_start {
        *a = *b;
    } else if b.period_start == a.period_start {
        a.spent_msat = a.spent_msat.max(b.spent_msat);
    }
}

/// Save a connection's budget use, as the service reports it.
pub async fn save_usage(state: &AppState, c: &Connection) {
    let mut guard = state.inner.lock().await;
    let Some(u) = guard.as_mut() else {
        // Locked meanwhile (a payment finished after the lock): straight to
        // the config file.
        drop(guard);
        let path = state.paths.config();
        if let Ok(Some(mut config)) = crate::config::Config::load(&path) {
            if let Some(e) = config.nwc.iter_mut().find(|e| e.connection.id == c.id) {
                merge_usage(&mut e.connection, c);
                if let Err(e) = config.save(&path) {
                    tracing::warn!("nwc: couldn't save a connection: {e}");
                }
            }
        }
        return;
    };
    let Some(entry) = u.config.nwc.iter_mut().find(|e| e.connection.id == c.id) else {
        return;
    };
    merge_usage(&mut entry.connection, c);
    if let Err(e) = state.save_config(&u.config) {
        tracing::warn!("nwc: couldn't save a connection: {e}");
    }
}

fn permissions(list: &[Permission]) -> BTreeSet<Permission> {
    list.iter().copied().collect()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewRequest {
    pub name: String,
    pub budget_sat: Option<u64>,
    #[serde(default)]
    pub budget_period: BudgetPeriod,
    pub max_per_payment_sat: Option<u64>,
    pub permissions: Vec<Permission>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Created {
    pub connection: View,
    pub uri: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Patch {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    /// `Some(None)`: no limit.
    #[serde(default, with = "double_option")]
    pub budget_sat: Option<Option<u64>>,
    pub budget_period: Option<BudgetPeriod>,
}

mod double_option {
    use serde::{Deserialize, Deserializer};
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<u64>>, D::Error> {
        Ok(Some(Option::deserialize(d)?))
    }
}

fn check_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 40 {
        return Err(Error::Invalid("Name the app (up to 40 characters)".into()));
    }
    Ok(n.to_string())
}

#[tauri::command]
pub async fn nwc_list(state: State<'_, AppState>) -> Result<Vec<View>> {
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(Error::Locked)?;
    Ok(current(u).iter().map(view).collect())
}

#[tauri::command]
pub async fn nwc_create(state: State<'_, AppState>, req: NewRequest) -> Result<Created> {
    state.check_screen()?;
    let name = check_name(&req.name)?;
    if req.permissions.is_empty() {
        return Err(Error::Invalid("Allow the app at least one thing".into()));
    }
    let mut guard = state.inner.lock().await;
    let u = guard.as_mut().ok_or(Error::Locked)?;
    if u.spark.is_empty() {
        return Err(Error::Invalid("Add a Spending account first".into()));
    }
    let made = Connection::create(
        ConnectionOptions {
            name,
            relays: vec![sessions_nwc::DEFAULT_RELAY.to_string()],
            permissions: permissions(&req.permissions),
            budget: Budget {
                max_per_payment_sat: req.max_per_payment_sat,
                limit_sat: req.budget_sat,
                period: req.budget_period,
            },
            expires_at: None,
            lud16: None,
        },
        sessions_nwc::now(),
    )
    .map_err(|e| Error::Invalid(e.to_string()))?;
    let uri = made.uri.to_string();
    let v = view(&made.connection);
    u.config.nwc.push(NwcEntry {
        connection: made.connection,
        uri: uri.clone(),
    });
    state.save_config(&u.config)?;
    sync_service(u, state.nwc_store());
    Ok(Created { connection: v, uri })
}

#[tauri::command]
pub async fn nwc_update(state: State<'_, AppState>, id: String, patch: Patch) -> Result<View> {
    state.check_screen()?;
    let mut guard = state.inner.lock().await;
    let u = guard.as_mut().ok_or(Error::Locked)?;
    let live = current(u);
    let entry = u
        .config
        .nwc
        .iter_mut()
        .find(|e| e.connection.id == id)
        .ok_or_else(|| Error::Invalid("That connection is gone".into()))?;
    if let Some(c) = live.into_iter().find(|c| c.id == id) {
        entry.connection = c;
    }
    let c = &mut entry.connection;
    if let Some(n) = &patch.name {
        c.name = check_name(n)?;
    }
    if let Some(on) = patch.enabled {
        c.enabled = on;
    }
    if let Some(limit) = patch.budget_sat {
        c.budget.limit_sat = limit;
    }
    if let Some(p) = patch.budget_period {
        c.budget.period = p;
    }
    let v = view(c);
    state.save_config(&u.config)?;
    sync_service(u, state.nwc_store());
    Ok(v)
}

#[tauri::command]
pub async fn nwc_delete(state: State<'_, AppState>, id: String) -> Result<()> {
    let mut guard = state.inner.lock().await;
    let u = guard.as_mut().ok_or(Error::Locked)?;
    u.config.nwc.retain(|e| e.connection.id != id);
    state.save_config(&u.config)?;
    sync_service(u, state.nwc_store());
    Ok(())
}

#[tauri::command]
pub async fn nwc_uri(state: State<'_, AppState>, id: String) -> Result<String> {
    state.check_screen()?;
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(Error::Locked)?;
    u.config
        .nwc
        .iter()
        .find(|e| e.connection.id == id)
        .map(|e| e.uri.clone())
        .ok_or_else(|| Error::Invalid("That connection is gone".into()))
}

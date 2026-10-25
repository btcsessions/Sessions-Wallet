//! Payment alerts.
//!
//! Whenever an account reports a change, look at its newest payments and
//! pick out the incoming ones we haven't seen: tell the UI at once
//! (`wallet://payment`, [`Arrival`]) so the Receive screen can celebrate, and
//! post a system notification when the app isn't on screen.
//!
//! An on-chain payment to a Spending account's deposit address isn't a
//! payment yet (it's claimed after confirmations), so those are found from
//! the address's outputs instead.
//!
//! "Keep watching with the app closed" runs a foreground service (see
//! `tauri-plugin-alerts`) that keeps this process, and so the wallet's live
//! connections, alive.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use sessions_core::{AccountId, Direction, Method, Payment, PaymentStatus};
use tauri::{AppHandle, Manager};

use crate::state::AppState;

/// Payments a little older than the moment the wallet unlocked still count
/// (they may have arrived while it was opening).
const GRACE_SECS: u64 = 120;
/// How many of an account's newest payments to look at.
const LOOK_AT: usize = 25;

pub type Emit = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

/// What arrived, for the UI (`wallet://payment`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Arrival {
    pub account: String,
    pub account_name: String,
    /// Payment id (txid for on-chain), or `txid:vout` for a deposit.
    pub id: String,
    pub amount_sat: u64,
    pub kind: Kind,
    /// Lightning (and other instant methods) vs on-chain.
    pub onchain: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// Arrived and spendable (Lightning, ecash, a claimed deposit, or an
    /// on-chain payment first seen already confirmed).
    Received,
    /// Seen on-chain, not yet confirmed.
    Incoming,
    /// An on-chain payment we announced as incoming has confirmed.
    Confirmed,
    /// Paid to a Spending account's deposit address: it's added after
    /// confirmations.
    Deposit,
}

/// The user's alert settings, from the synced preferences
/// (`prefs.alerts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Post system notifications (default on; the OS permission decides).
    pub notify: bool,
    /// Keep watching with the app closed (default off).
    pub background: bool,
}

impl Settings {
    pub fn from_prefs(prefs: &serde_json::Value) -> Self {
        let a = prefs.get("alerts");
        let flag = |k: &str, default: bool| {
            a.and_then(|a| a.get(k))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(default)
        };
        Settings {
            notify: flag("notify", true),
            background: flag("background", false),
        }
    }
}

struct Inner {
    settings: Settings,
    /// Unix seconds the wallet unlocked.
    since: u64,
    /// Per account: payment id → last status seen.
    seen: HashMap<AccountId, HashMap<String, PaymentStatus>>,
    /// Per Spending account: deposit outpoints seen.
    deposits: HashMap<AccountId, HashSet<String>>,
    /// Accounts being checked, and those that changed again meanwhile.
    busy: HashSet<AccountId>,
    again: HashSet<AccountId>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            notify: true,
            background: false,
        }
    }
}

impl Default for Inner {
    fn default() -> Self {
        Inner {
            settings: Settings::default(),
            since: 0,
            seen: HashMap::new(),
            deposits: HashMap::new(),
            busy: HashSet::new(),
            again: HashSet::new(),
        }
    }
}

#[derive(Default)]
pub struct Alerts {
    inner: Mutex<Inner>,
    emit: OnceLock<Emit>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Alerts {
    pub fn set_emitter(&self, emit: Emit) {
        let _ = self.emit.set(emit);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Forget everything: the wallet just unlocked (or locked).
    pub fn reset(&self) {
        let mut i = self.lock();
        let settings = i.settings;
        *i = Inner {
            settings,
            since: now(),
            ..Inner::default()
        };
    }

    pub fn settings(&self) -> Settings {
        self.lock().settings
    }

    /// The user's settings (from the wallet's preferences), and the watch
    /// service to match: running only while the wallet is unlocked and the
    /// user wants it.
    pub fn apply(&self, settings: Settings, unlocked: bool) {
        self.lock().settings = settings;
        if unlocked && settings.background {
            tauri_plugin_alerts::start_watch();
        } else {
            tauri_plugin_alerts::stop_watch();
        }
    }

    /// An account changed: check it for new payments (in the background,
    /// coalesced).
    pub fn changed(app: &AppHandle, account: AccountId) {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let state = app.state::<AppState>();
            let alerts = state.alerts.clone();
            {
                let mut i = alerts.lock();
                if !i.busy.insert(account.clone()) {
                    i.again.insert(account);
                    return;
                }
            }
            loop {
                if let Err(e) = alerts.check(&state, &account).await {
                    tracing::debug!(account = %account, "alerts: check failed: {e}");
                }
                let mut i = alerts.lock();
                if !i.again.remove(&account) {
                    i.busy.remove(&account);
                    break;
                }
            }
        });
    }

    async fn check(&self, state: &AppState, id: &AccountId) -> sessions_core::Result<()> {
        let (rail, name) = {
            let guard = state.inner.lock().await;
            let Some(u) = guard.as_ref() else {
                return Ok(());
            };
            let name = u
                .config
                .accounts
                .iter()
                .find(|s| s.id() == id)
                .map(|s| s.name().to_string())
                .unwrap_or_default();
            (u.wallet.account(id).await?, name)
        };
        let payments = rail.payments(LOOK_AT).await?;
        let (found, settings) = {
            let mut i = self.lock();
            let settings = i.settings;
            let since = i.since;
            let seen = i.seen.entry(id.clone()).or_default();
            (diff(seen, since, &payments), settings)
        };
        for (p, kind) in found {
            self.announce(
                Arrival {
                    account: id.0.clone(),
                    account_name: name.clone(),
                    id: p.id.clone(),
                    amount_sat: p.amount_sat,
                    kind,
                    onchain: p.method == Method::BitcoinAddress,
                },
                settings,
            );
        }
        Ok(())
    }

    /// Outputs paid to a Spending account's deposit address (`(outpoint,
    /// amount, confirmed)`). The first call only learns what's there.
    pub fn deposits(&self, id: &AccountId, name: &str, outputs: &[(String, u64, bool)]) {
        let (fresh, settings) = {
            let mut i = self.lock();
            let settings = i.settings;
            let first = !i.deposits.contains_key(id);
            let known = i.deposits.entry(id.clone()).or_default();
            let mut fresh = Vec::new();
            for (outpoint, amount, confirmed) in outputs {
                if known.insert(outpoint.clone()) && !first && !confirmed {
                    fresh.push((outpoint.clone(), *amount));
                }
            }
            (fresh, settings)
        };
        for (outpoint, amount) in fresh {
            self.announce(
                Arrival {
                    account: id.0.clone(),
                    account_name: name.to_string(),
                    id: outpoint,
                    amount_sat: amount,
                    kind: Kind::Deposit,
                    onchain: true,
                },
                settings,
            );
        }
    }

    fn announce(&self, a: Arrival, settings: Settings) {
        tracing::info!(account = %a.account, kind = ?a.kind, "payment alert");
        if let Some(emit) = self.emit.get() {
            emit(
                "wallet://payment",
                serde_json::to_value(&a).unwrap_or_default(),
            );
        }
        if settings.notify {
            let (title, body, public) = text(&a);
            tauri_plugin_alerts::post(&tauri_plugin_alerts::Notice {
                key: &a.id,
                title: &title,
                body: &body,
                public_title: public,
            });
        }
    }
}

/// The incoming payments in `payments` worth announcing, updating `seen`.
/// New ones count from `since` (less a grace period); an on-chain payment
/// announced while pending is announced again once it confirms.
fn diff(
    seen: &mut HashMap<String, PaymentStatus>,
    since: u64,
    payments: &[Payment],
) -> Vec<(Payment, Kind)> {
    let mut out = Vec::new();
    for p in payments {
        if p.direction != Direction::Incoming {
            continue;
        }
        let before = seen.insert(p.id.clone(), p.status);
        let recent = p.timestamp == 0 || p.timestamp + GRACE_SECS >= since;
        let kind = match (before, p.status) {
            (None, PaymentStatus::Failed) | (Some(_), PaymentStatus::Failed) => continue,
            (None, _) if !recent => continue,
            (None, PaymentStatus::Pending) => Kind::Incoming,
            (None, PaymentStatus::Complete) => Kind::Received,
            (Some(PaymentStatus::Pending), PaymentStatus::Complete) => {
                if p.method == Method::BitcoinAddress {
                    Kind::Confirmed
                } else {
                    Kind::Received
                }
            }
            (Some(_), _) => continue,
        };
        out.push((p.clone(), kind));
    }
    out
}

pub fn sats(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{out} sats")
}

/// (title, body, lock-screen title).
fn text(a: &Arrival) -> (String, String, &'static str) {
    let amount = sats(a.amount_sat);
    let to = if a.account_name.is_empty() {
        "your wallet".to_string()
    } else {
        a.account_name.clone()
    };
    match a.kind {
        Kind::Received => (
            format!("Received {amount}"),
            format!("In {to}"),
            "Payment received",
        ),
        Kind::Incoming => (
            format!("Incoming {amount}"),
            format!("To {to}. Waiting for confirmation"),
            "Payment incoming",
        ),
        Kind::Confirmed => (
            format!("{amount} confirmed"),
            format!("In {to}"),
            "Payment confirmed",
        ),
        Kind::Deposit => (
            format!("Incoming {amount}"),
            format!("To {to}. Added after it confirms"),
            "Payment incoming",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pay(id: &str, dir: Direction, status: PaymentStatus, method: Method, ts: u64) -> Payment {
        Payment {
            id: id.into(),
            account: AccountId("a".into()),
            direction: dir,
            amount_sat: 1000,
            fee_sat: 0,
            status,
            method,
            timestamp: ts,
            description: None,
            token: None,
        }
    }

    #[test]
    fn new_payments_only_after_unlock() {
        let mut seen = HashMap::new();
        let since = 1_000_000;
        let old = pay("old", Direction::Incoming, PaymentStatus::Complete, Method::BitcoinAddress, 500);
        let out = pay("out", Direction::Outgoing, PaymentStatus::Complete, Method::Bolt11, since + 5);
        let ln = pay("ln", Direction::Incoming, PaymentStatus::Complete, Method::Bolt11, since + 5);
        let found = diff(&mut seen, since, &[old.clone(), out, ln.clone()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.id, "ln");
        assert_eq!(found[0].1, Kind::Received);
        // Seen once: never again.
        assert!(diff(&mut seen, since, &[old, ln]).is_empty());
    }

    #[test]
    fn onchain_incoming_then_confirmed() {
        let mut seen = HashMap::new();
        let since = 1_000_000;
        let p = pay("tx", Direction::Incoming, PaymentStatus::Pending, Method::BitcoinAddress, since + 1);
        assert_eq!(diff(&mut seen, since, &[p.clone()])[0].1, Kind::Incoming);
        assert!(diff(&mut seen, since, &[p.clone()]).is_empty());
        // Confirmed: its timestamp becomes the block time, maybe before `since`.
        let c = Payment {
            status: PaymentStatus::Complete,
            timestamp: since - 10_000,
            ..p
        };
        assert_eq!(diff(&mut seen, since, &[c.clone()])[0].1, Kind::Confirmed);
        assert!(diff(&mut seen, since, &[c]).is_empty());
    }

    #[test]
    fn grace_and_failures() {
        let mut seen = HashMap::new();
        let since = 1_000_000;
        let just_before = pay("a", Direction::Incoming, PaymentStatus::Complete, Method::Bolt11, since - 60);
        let failed = pay("b", Direction::Incoming, PaymentStatus::Failed, Method::Bolt11, since + 1);
        let found = diff(&mut seen, since, &[just_before, failed]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.id, "a");
    }

    #[test]
    fn deposits_learn_first_then_announce_unconfirmed() {
        let alerts = Alerts::default();
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        alerts.set_emitter(Arc::new(move |_, v| g.lock().unwrap().push(v)));
        let id = AccountId("s".into());
        alerts.lock().settings.notify = false;
        alerts.deposits(&id, "Spending", &[("a:0".into(), 5, true)]);
        assert!(got.lock().unwrap().is_empty());
        alerts.deposits(
            &id,
            "Spending",
            &[("a:0".into(), 5, true), ("b:1".into(), 7000, false)],
        );
        let v = got.lock().unwrap().clone();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["kind"], "deposit");
        assert_eq!(v[0]["amountSat"], 7000);
        assert_eq!(v[0]["id"], "b:1");
    }

    #[test]
    fn settings_and_text() {
        let s = Settings::from_prefs(&serde_json::json!({}));
        assert!(s.notify && !s.background);
        let s = Settings::from_prefs(&serde_json::json!({"alerts": {"notify": false, "background": true}}));
        assert!(!s.notify && s.background);
        assert_eq!(sats(1_234_567), "1,234,567 sats");
        assert_eq!(sats(999), "999 sats");
        let (t, b, p) = text(&Arrival {
            account: "x".into(),
            account_name: "Spending".into(),
            id: "i".into(),
            amount_sat: 21_000,
            kind: Kind::Received,
            onchain: false,
        });
        assert_eq!((t.as_str(), b.as_str(), p), ("Received 21,000 sats", "In Spending", "Payment received"));
    }
}

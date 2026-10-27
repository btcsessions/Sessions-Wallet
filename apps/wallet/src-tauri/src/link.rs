//! Linking devices: set up this wallet on another phone or computer by QR
//! (see `sessions_group::link` for the protocol).
//!
//! * New device: [`link_offer`] makes the QR, [`link_poll`] watches for the
//!   wallet device's hello (then shows the check code) and the seed, and
//!   [`link_finish`] creates the wallet from it with this device's passcode.
//!   The UI then calls `restore_state`, as after any restore, which brings
//!   the accounts, labels and settings over from the encrypted backup.
//! * Wallet device: [`link_scan`] reads the QR and says hello (both screens
//!   show the code); [`link_send`] asks for the passcode and sends the seed.

use std::sync::Mutex;

use serde::Serialize;
use serde_json::json;
use sessions_backup::relay::Relays;
use sessions_core::keystore::Seed;
use sessions_core::{Error, Result};
use sessions_group::link::{self, Offer, Received, Sender, KIND_LINK};
use tauri::State;
use zeroize::Zeroizing;

use crate::config::{BackupFlags, SeedOrigin};
use crate::group_join::{now, published, relays};
use crate::state::AppState;

/// New-device side, in memory only.
#[derive(Default)]
struct Incoming {
    offer: Option<Offer>,
    /// The wallet device that said hello first: the only one we listen to.
    sender: Option<(String, String, String)>,
    phrase: Option<Zeroizing<String>>,
}

/// Wallet-device side.
#[derive(Default)]
struct Outgoing {
    sender: Option<Sender>,
}

static INCOMING: Mutex<Option<Incoming>> = Mutex::new(None);
static OUTGOING: Mutex<Option<Outgoing>> = Mutex::new(None);

fn incoming<T>(f: impl FnOnce(&mut Incoming) -> T) -> T {
    let mut g = INCOMING.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Default::default))
}

fn outgoing<T>(f: impl FnOnce(&mut Outgoing) -> T) -> T {
    let mut g = OUTGOING.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Default::default))
}

/// What this device is called on the other screen.
pub fn device_name() -> &'static str {
    if cfg!(target_os = "android") {
        "Android phone"
    } else if cfg!(target_os = "ios") {
        "iPhone"
    } else if cfg!(target_os = "macos") {
        "Mac"
    } else if cfg!(target_os = "windows") {
        "Windows PC"
    } else {
        "Linux computer"
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkOffer {
    pub qr: String,
    pub expires_at: u64,
}

/// New device: a fresh QR to scan from the wallet device.
#[tauri::command]
pub async fn link_offer(state: State<'_, AppState>) -> Result<LinkOffer> {
    if state.has_vault() {
        return Err(Error::Invalid("This device already has a wallet".into()));
    }
    let offer = Offer::new(now());
    let out = LinkOffer {
        qr: offer.qr(),
        expires_at: offer.created + link::LINK_TTL_SECS,
    };
    incoming(|i| *i = Incoming { offer: Some(offer), ..Default::default() });
    Ok(out)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum LinkStatus {
    /// Waiting for the wallet device to scan.
    Waiting,
    /// Scanned: compare this code with the wallet device.
    Code { code: String, from: String },
    /// The seed arrived: choose a passcode to finish.
    Ready { from: String },
    /// The QR ran out: make a new one.
    Expired,
}

/// New device: check the relays for the hello and the seed.
#[tauri::command]
pub async fn link_poll() -> Result<LinkStatus> {
    let (me, since, expired, ready) = incoming(|i| {
        let o = i.offer.as_ref();
        (
            o.map(|o| o.key.public_hex()),
            o.map(|o| o.created.saturating_sub(60)).unwrap_or(0),
            o.is_none_or(|o| o.expired(now())),
            i.phrase.is_some().then(|| i.sender.as_ref().map(|s| s.1.clone()).unwrap_or_default()),
        )
    });
    if let Some(from) = ready {
        return Ok(LinkStatus::Ready { from });
    }
    if expired {
        return Ok(LinkStatus::Expired);
    }
    let Some(me) = me else { return Ok(LinkStatus::Expired) };
    let filter = json!({ "kinds": [KIND_LINK], "#p": [me], "since": since });
    let mut events: Vec<_> = Relays::default()
        .query(&relays(), filter)
        .await
        .into_iter()
        .filter_map(|(_, r)| r.ok())
        .flatten()
        .collect();
    events.sort_by_key(|e| e.created_at);
    events.dedup_by(|a, b| a.id == b.id);
    let t = now();
    Ok(incoming(|i| {
        let Some(offer) = i.offer.as_ref() else { return LinkStatus::Expired };
        for e in &events {
            match link::open(offer, e, t) {
                Ok(Received::Hello { sender, name, code }) => {
                    if i.sender.is_none() {
                        i.sender = Some((sender, name, code));
                    }
                }
                Ok(Received::Seed { sender, phrase }) => {
                    if i.sender.as_ref().is_some_and(|s| s.0 == sender) {
                        i.phrase = Some(phrase);
                    }
                }
                Err(_) => {}
            }
        }
        match (&i.sender, &i.phrase) {
            (Some(s), Some(_)) => LinkStatus::Ready { from: s.1.clone() },
            (Some(s), None) => LinkStatus::Code {
                code: s.2.clone(),
                from: s.1.clone(),
            },
            _ => LinkStatus::Waiting,
        }
    }))
}

/// New device: create the wallet from the seed that arrived, with this
/// device's own passcode. Then the UI calls `restore_state`.
#[tauri::command]
pub async fn link_finish(app: tauri::AppHandle, state: State<'_, AppState>, passcode: String) -> Result<()> {
    crate::commands::check_passcode(&passcode)?;
    let phrase = incoming(|i| i.phrase.clone()).ok_or_else(|| Error::Invalid("Nothing has arrived yet".into()))?;
    let seed = Seed::from_phrase(&phrase)?;
    state
        .create_with(
            seed,
            &passcode,
            BackupFlags {
                passkey: false,
                phrase: true,
                seed_origin: SeedOrigin::Phrase,
                credential_id: None,
                last_tested_at: None,
            },
            true,
        )
        .await?;
    crate::biometric::forget(&app).await;
    link_cancel();
    Ok(())
}

/// Either side: forget the link in progress.
#[tauri::command]
pub fn link_cancel() {
    *INCOMING.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *OUTGOING.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkScan {
    pub code: String,
}

/// Wallet device: the new device's QR was scanned. Says hello so both
/// screens show the check code.
#[tauri::command]
pub async fn link_scan(state: State<'_, AppState>, qr: String) -> Result<LinkScan> {
    state.check_screen()?;
    {
        let guard = state.inner.lock().await;
        guard.as_ref().ok_or(Error::Locked)?;
    }
    let sender = Sender::new(link::parse(&qr)?);
    let hello = sender.hello(device_name(), now())?;
    published(Relays::default().publish(&relays(), &[hello]).await)?;
    let code = sender.code();
    outgoing(|o| o.sender = Some(sender));
    Ok(LinkScan { code })
}

/// Wallet device: the codes match. Re-checks the passcode, then sends the
/// seed to the new device.
#[tauri::command]
pub async fn link_send(state: State<'_, AppState>, passcode: String) -> Result<()> {
    state.check_screen()?;
    let seed = state.open_vault(&passcode).await?;
    let event = outgoing(|o| o.sender.as_ref().map(|s| s.seed(&seed.phrase(), now())))
        .ok_or_else(|| Error::Invalid("Scan the other device's code first".into()))??;
    published(Relays::default().publish(&relays(), &[event]).await)?;
    outgoing(|o| o.sender = None);
    Ok(())
}

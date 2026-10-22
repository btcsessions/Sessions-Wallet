//! Bluetooth LE links to Jade.
//!
//! Jade offers a UART-style GATT service: we write requests to its RX
//! characteristic (with response; Jade requires an encrypted, bonded link)
//! and it sends replies as indications on TX. Desktop uses btleplug; Android
//! goes through `tauri-plugin-jade`.
//!
//! Pairings go stale when Jade forgets this phone (a reset, or "Reset BLE"
//! on Jade) while the phone still holds the old keys: the link then fails
//! to encrypt. On Android [`next_step`] spots that from what the plugin
//! reports and has it remove the old pairing and pair afresh, so the person
//! just confirms the number again. Jade keeps its pairings across power-off,
//! so turning it off and on needs no new pairing.

use sessions_core::Result;
use tauri::AppHandle;

use super::rpc::Link;

/// How long to look for a Jade before giving up.
const SCAN_MS: u64 = 15_000;

/// Find the nearest Jade (the paired one first), connect (pairing if the OS
/// needs to) and return the link. Emits the "connecting" status once a Jade
/// is found, and "pairing" when the phone asks to pair (Android).
pub async fn open(app: &AppHandle) -> Result<Box<dyn Link>> {
    imp::open(app).await
}

// --- what to do when connecting fails (Android) ------------------------------

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod policy {
    /// How a connect attempt failed, as the Android plugin reports it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Kind {
        /// No link came up (Jade off, out of range, or Android's status 133).
        Connect,
        /// The link went down while setting up.
        Dropped,
        /// Jade refused the phone's encryption keys.
        Auth,
        /// Android dropped its pairing with Jade mid-connect.
        BondLost,
    }

    #[derive(Debug, Clone, Copy)]
    pub struct Failure {
        pub kind: Kind,
        /// GATT status or HCI reason.
        pub status: i32,
        /// The phone was paired with this Jade before the attempt.
        pub bonded: bool,
        /// The attempt paired just now.
        pub fresh_bond: bool,
    }

    #[derive(Debug, PartialEq, Eq)]
    pub enum Next {
        /// Try the same again (Android often fails a first connect).
        Retry,
        /// Remove the stale pairing and pair afresh.
        Repair,
        GiveUp,
    }

    /// Attempts per `open`, re-pairing included.
    pub const MAX_ATTEMPTS: u32 = 3;

    /// HCI / GATT codes that mean the keys don't match: 5 authentication
    /// failure (or GATT insufficient authentication), 6 PIN or key missing,
    /// 15 insufficient encryption, 61 MIC failure, 137 GATT auth fail.
    pub fn is_key_problem(status: i32) -> bool {
        matches!(status, 5 | 6 | 15 | 61 | 137)
    }

    /// 19 Jade hung up, 22 Android hung up: right after connecting, on a
    /// paired Jade, typically because encryption with the old keys failed.
    fn hung_up(status: i32) -> bool {
        matches!(status, 19 | 22)
    }

    /// What to do after attempt number `attempt` (0-based) failed with `f`.
    /// `repaired`: a re-pair already happened during this `open`. Only
    /// positive signs of stale keys lead to re-pairing: a Jade that's simply
    /// off or far away must not lose its pairing.
    pub fn next_step(f: &Failure, attempt: u32, repaired: bool) -> Next {
        if attempt + 1 >= MAX_ATTEMPTS {
            return Next::GiveUp;
        }
        let stale_pairing = f.bonded && !f.fresh_bond && !repaired;
        match f.kind {
            Kind::Auth | Kind::BondLost if stale_pairing => Next::Repair,
            Kind::Auth | Kind::BondLost => Next::GiveUp,
            Kind::Dropped if stale_pairing && is_key_problem(f.status) => Next::Repair,
            // Hung up twice in a row right after connecting.
            Kind::Dropped if stale_pairing && hung_up(f.status) && attempt >= 1 => Next::Repair,
            Kind::Dropped | Kind::Connect if attempt == 0 => Next::Retry,
            Kind::Dropped | Kind::Connect => Next::GiveUp,
        }
    }

    /// Plain English once it's given up.
    pub fn give_up_error(f: &Failure) -> sessions_core::Error {
        let keys = matches!(f.kind, Kind::Auth | Kind::BondLost)
            || (f.kind == Kind::Dropped && is_key_problem(f.status));
        if f.fresh_bond {
            sessions_core::Error::Invalid(
                "Pairing with Jade didn't finish. Try again, and confirm the same number on Jade and your phone"
                    .into(),
            )
        } else if keys {
            super::super::forget_and_repair()
        } else {
            super::super::not_found()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn f(kind: Kind, status: i32, bonded: bool) -> Failure {
            Failure {
                kind,
                status,
                bonded,
                fresh_bond: false,
            }
        }

        #[test]
        fn re_pairs_only_on_stale_keys() {
            // Jade refused the old keys: pair again, once.
            assert_eq!(next_step(&f(Kind::Auth, 5, true), 0, false), Next::Repair);
            assert_eq!(next_step(&f(Kind::Auth, 15, true), 1, true), Next::GiveUp);
            assert_eq!(
                next_step(&f(Kind::BondLost, 0, true), 0, false),
                Next::Repair
            );
            assert_eq!(
                next_step(&f(Kind::Dropped, 6, true), 0, false),
                Next::Repair
            );
            assert_eq!(
                next_step(&f(Kind::Dropped, 61, true), 0, false),
                Next::Repair
            );
            // Hung up right after connecting: retry first, re-pair the second time.
            assert_eq!(
                next_step(&f(Kind::Dropped, 19, true), 0, false),
                Next::Retry
            );
            assert_eq!(
                next_step(&f(Kind::Dropped, 22, true), 1, false),
                Next::Repair
            );
            // Off or far away (133 generic error, 8 supervision timeout): never un-pair.
            assert_eq!(
                next_step(&f(Kind::Connect, 133, true), 0, false),
                Next::Retry
            );
            assert_eq!(
                next_step(&f(Kind::Connect, 133, true), 1, false),
                Next::GiveUp
            );
            assert_eq!(next_step(&f(Kind::Dropped, 8, true), 0, false), Next::Retry);
            assert_eq!(
                next_step(&f(Kind::Dropped, 8, true), 1, false),
                Next::GiveUp
            );
            // Never paired: nothing stale to remove.
            assert_eq!(next_step(&f(Kind::Auth, 5, false), 0, false), Next::GiveUp);
            assert_eq!(
                next_step(&f(Kind::Dropped, 6, false), 0, false),
                Next::Retry
            );
            // A pairing made moments ago isn't stale.
            let fresh = Failure {
                fresh_bond: true,
                ..f(Kind::Auth, 5, true)
            };
            assert_eq!(next_step(&fresh, 0, false), Next::GiveUp);
            // Bounded.
            assert_eq!(
                next_step(&f(Kind::Auth, 5, true), MAX_ATTEMPTS - 1, false),
                Next::GiveUp
            );
        }

        #[test]
        fn give_up_messages_are_plain() {
            let msg = |f: Failure| give_up_error(&f).to_string();
            assert!(msg(f(Kind::Auth, 5, true)).contains("Forget Jade"));
            assert!(msg(f(Kind::Connect, 133, true)).starts_with("Turn on Jade"));
            let fresh = Failure {
                fresh_bond: true,
                ..f(Kind::Dropped, 19, true)
            };
            assert!(msg(fresh).starts_with("Pairing with Jade didn't finish"));
        }
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod imp {
    use std::time::Duration;

    use async_trait::async_trait;
    use btleplug::api::{
        Central, CentralState, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
    };
    use btleplug::platform::{Adapter, Manager, Peripheral};
    use futures_util::StreamExt;
    use sessions_core::{Error, Result};
    use tauri::AppHandle;
    use tokio::sync::{mpsc, Mutex};
    use tokio::time::Instant;
    use uuid::Uuid;

    use super::super::rpc::{Fail, Link};
    use super::SCAN_MS;

    const SERVICE: Uuid = Uuid::from_u128(0x6e400001_b5a3_f393_e0a9_e50e24dcca9e);
    const RX: Uuid = Uuid::from_u128(0x6e400002_b5a3_f393_e0a9_e50e24dcca9e);
    const TX: Uuid = Uuid::from_u128(0x6e400003_b5a3_f393_e0a9_e50e24dcca9e);

    /// Per write. The OS splits anything over the MTU into a long write, as
    /// Jade's own Python client relies on (largest attribute is 512).
    const CHUNK: usize = 509;
    /// After the first Jade shows up, wait this long for a closer one.
    const SETTLE: Duration = Duration::from_millis(1500);
    const CONNECT: Duration = Duration::from_secs(20);

    pub async fn open(app: &AppHandle) -> Result<Box<dyn Link>> {
        let manager = Manager::new().await.map_err(setup_error)?;
        let adapter = manager
            .adapters()
            .await
            .map_err(setup_error)?
            .into_iter()
            .next()
            .ok_or_else(super::super::turn_on_bluetooth)?;
        if matches!(adapter.adapter_state().await, Ok(CentralState::PoweredOff)) {
            return Err(super::super::turn_on_bluetooth());
        }
        adapter
            .start_scan(ScanFilter {
                services: vec![SERVICE],
            })
            .await
            .map_err(setup_error)?;
        let found = find(&adapter).await;
        let _ = adapter.stop_scan().await;
        let peripheral = found?.ok_or_else(super::super::not_found)?;
        super::super::status(app, "connecting");
        connect(adapter, peripheral).await
    }

    /// The Jade with the strongest signal seen during this scan.
    async fn find(adapter: &Adapter) -> Result<Option<Peripheral>> {
        let start = Instant::now();
        let mut first_seen: Option<Instant> = None;
        let mut best: Option<(i16, Peripheral)> = None;
        loop {
            for p in adapter.peripherals().await.map_err(setup_error)? {
                let Ok(Some(props)) = p.properties().await else {
                    continue;
                };
                let is_jade = props.services.contains(&SERVICE)
                    || props
                        .local_name
                        .as_deref()
                        .is_some_and(|n| n.starts_with("Jade"));
                // No RSSI: remembered by the OS but not seen in this scan.
                let Some(rssi) = props.rssi else { continue };
                if is_jade && best.as_ref().is_none_or(|(r, _)| rssi > *r) {
                    best = Some((rssi, p));
                }
            }
            if best.is_some() && first_seen.get_or_insert_with(Instant::now).elapsed() >= SETTLE {
                break;
            }
            if start.elapsed() >= Duration::from_millis(SCAN_MS) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Ok(best.map(|(_, p)| p))
    }

    async fn connect(adapter: Adapter, peripheral: Peripheral) -> Result<Box<dyn Link>> {
        if !peripheral.is_connected().await.unwrap_or(false) {
            match tokio::time::timeout(CONNECT, peripheral.connect()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!("jade: connect failed: {e}");
                    return Err(super::super::not_found());
                }
                Err(_) => return Err(super::super::not_found()),
            }
        }
        let setup = async {
            peripheral.discover_services().await.map_err(setup_error)?;
            let chars = peripheral.characteristics();
            let find = |uuid| chars.iter().find(|c| c.uuid == uuid).cloned();
            let (Some(rx), Some(tx)) = (find(RX), find(TX)) else {
                return Err(Error::Invalid("That Bluetooth device isn't a Jade".into()));
            };
            let mut stream = peripheral.notifications().await.map_err(setup_error)?;
            peripheral
                .subscribe(&tx)
                .await
                .map_err(|e| match link_error(e) {
                    Fail::App(e) => e,
                    _ => super::super::lost_connection(),
                })?;
            let (sender, receiver) = mpsc::unbounded_channel();
            let task = tokio::spawn(async move {
                while let Some(n) = stream.next().await {
                    if n.uuid == TX && sender.send(n.value).is_err() {
                        break;
                    }
                }
            });
            Ok((rx, receiver, task))
        };
        match setup.await {
            Ok((rx, receiver, task)) => Ok(Box::new(BleLink {
                _adapter: adapter,
                peripheral,
                rx,
                incoming: Mutex::new(receiver),
                task,
            })),
            Err(e) => {
                let _ = peripheral.disconnect().await;
                Err(e)
            }
        }
    }

    struct BleLink {
        /// Keeps the OS session alive on some platforms.
        _adapter: Adapter,
        peripheral: Peripheral,
        rx: Characteristic,
        incoming: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
        task: tokio::task::JoinHandle<()>,
    }

    #[async_trait]
    impl Link for BleLink {
        async fn send(&self, bytes: &[u8]) -> std::result::Result<(), Fail> {
            for chunk in bytes.chunks(CHUNK) {
                self.peripheral
                    .write(&self.rx, chunk, WriteType::WithResponse)
                    .await
                    .map_err(link_error)?;
            }
            Ok(())
        }

        async fn recv(&self) -> std::result::Result<Vec<u8>, Fail> {
            let mut incoming = self.incoming.lock().await;
            loop {
                tokio::select! {
                    msg = incoming.recv() => {
                        return msg.ok_or_else(|| Fail::Link("Jade disconnected".into()));
                    }
                    // Not every platform ends the notification stream on a
                    // disconnect, so check now and then.
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {
                        if !self.peripheral.is_connected().await.unwrap_or(false) {
                            return Err(Fail::Link("Jade disconnected".into()));
                        }
                    }
                }
            }
        }

        async fn close(&self) {
            self.task.abort();
            let _ = self.peripheral.disconnect().await;
        }
    }

    impl Drop for BleLink {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn setup_error(e: btleplug::Error) -> Error {
        use btleplug::Error as E;
        match &e {
            E::PermissionDenied => super::super::allow_bluetooth(),
            E::NoAdapterAvailable => super::super::turn_on_bluetooth(),
            _ => {
                let msg = e.to_string();
                tracing::warn!("jade: bluetooth: {msg}");
                // BlueZ: adapter off ("NotReady") or bluetoothd not running.
                if msg.contains("NotReady")
                    || msg.contains("ServiceUnknown")
                    || msg.to_ascii_lowercase().contains("powered off")
                {
                    super::super::turn_on_bluetooth()
                } else {
                    Error::Invalid(format!("Bluetooth error: {msg}"))
                }
            }
        }
    }

    /// Writes fail like this when the link isn't encrypted with a bond Jade knows.
    fn is_pairing_problem(msg: &str) -> bool {
        let m = msg.to_ascii_lowercase();
        [
            "authenticat",
            "encrypt",
            "notauthorized",
            "not authorized",
            "notpermitted",
            "not permitted",
            "not paired",
            "att error: 0x05",
            "att error: 0x0f",
            "att error: 0x08",
        ]
        .iter()
        .any(|needle| m.contains(needle))
    }

    fn link_error(e: btleplug::Error) -> Fail {
        use btleplug::Error as E;
        match e {
            E::NotConnected | E::DeviceNotFound => Fail::Link(e.to_string()),
            E::PermissionDenied => Fail::App(super::super::allow_bluetooth()),
            other => {
                let msg = other.to_string();
                tracing::warn!("jade: write failed: {msg}");
                if is_pairing_problem(&msg) {
                    Fail::App(super::super::pair_first())
                } else {
                    Fail::Link(msg)
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn spots_pairing_problems() {
            assert!(is_pairing_problem(
                "org.bluez.Error.Failed: Operation failed with ATT error: 0x05"
            ));
            assert!(is_pairing_problem("org.bluez.Error.NotAuthorized"));
            assert!(is_pairing_problem("Insufficient Encryption"));
            assert!(!is_pairing_problem("Not connected"));
            assert_eq!(SERVICE.to_string(), "6e400001-b5a3-f393-e0a9-e50e24dcca9e");
            assert_eq!(TX.to_string(), "6e400003-b5a3-f393-e0a9-e50e24dcca9e");
        }
    }
}

#[cfg(target_os = "android")]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use async_trait::async_trait;
    use sessions_core::{Error, Result};
    use tauri::{AppHandle, Manager};
    use tauri_plugin_jade::{BleError, BleEvent, ConnectOutcome, JadeBle, LinkFailure};
    use tokio::sync::{mpsc, Mutex};

    use super::super::rpc::{Fail, Link};
    use super::policy::{self, Kind, Next};
    use super::SCAN_MS;

    pub async fn open(app: &AppHandle) -> Result<Box<dyn Link>> {
        let ble = app.state::<JadeBle<tauri::Wry>>();
        let mut repaired = false;
        let mut attempt = 0;
        loop {
            let (sender, receiver) = mpsc::unbounded_channel();
            let up = Arc::new(AtomicBool::new(true));
            // The plugin keeps its callback for good; let it go quiet once the
            // link (holding the strong sender) is dropped.
            let weak = sender.downgrade();
            let ui = app.clone();
            let link_up = up.clone();
            let outcome = ble
                .connect(SCAN_MS, repaired, move |e| match e {
                    BleEvent::Connecting => super::super::status(&ui, "connecting"),
                    BleEvent::Pairing => super::super::status(&ui, "pairing"),
                    BleEvent::Background(b) => {
                        tracing::debug!("jade: app in background: {b}");
                        ui.state::<super::super::JadeState>().set_foreground(!b);
                    }
                    other => {
                        if let BleEvent::Disconnected(status) = other {
                            tracing::info!("jade: link dropped (status {status})");
                            link_up.store(false, Ordering::SeqCst);
                        }
                        if let Some(s) = weak.upgrade() {
                            let _ = s.send(other);
                        }
                    }
                })
                .await
                .map_err(setup_error)?;
            let failure = match outcome {
                ConnectOutcome::Connected(c) => {
                    tracing::info!("jade: connected to {} (mtu {})", c.name, c.mtu);
                    return Ok(Box::new(PluginLink {
                        app: app.clone(),
                        incoming: Mutex::new(receiver),
                        up,
                        _sender: sender,
                    }));
                }
                ConnectOutcome::Failed(f) => failure(&f),
            };
            let next = policy::next_step(&failure, attempt, repaired);
            tracing::info!("jade: connect attempt {attempt} failed: {failure:?}; next: {next:?}");
            match next {
                Next::Retry => {}
                Next::Repair => {
                    repaired = true;
                    super::super::status(app, "pairing");
                }
                Next::GiveUp => return Err(policy::give_up_error(&failure)),
            }
            attempt += 1;
        }
    }

    fn failure(f: &LinkFailure) -> policy::Failure {
        policy::Failure {
            kind: match f.kind.as_str() {
                "connect" => Kind::Connect,
                "auth" => Kind::Auth,
                "bondLost" => Kind::BondLost,
                _ => Kind::Dropped,
            },
            status: f.status,
            bonded: f.bonded,
            fresh_bond: f.fresh_bond,
        }
    }

    fn setup_error(e: BleError) -> Error {
        tracing::warn!("jade: connect failed: {e}");
        match e {
            BleError::NoBluetooth => {
                Error::Invalid("This phone doesn't have Bluetooth LE. Use Jade's QR mode instead".into())
            }
            BleError::BluetoothOff => super::super::turn_on_bluetooth(),
            BleError::LocationOff => Error::Invalid(
                "Turn on Location: older Android versions need it to find Bluetooth devices".into(),
            ),
            BleError::PermissionDenied => super::super::allow_bluetooth(),
            BleError::NotFound | BleError::Disconnected => super::super::not_found(),
            BleError::PairingFailed => Error::Invalid(
                "Pairing with Jade didn't finish. Try again, and confirm the same number on Jade and your phone"
                    .into(),
            ),
            BleError::PairingStale => super::super::forget_and_repair(),
            BleError::Other(m) => Error::Invalid(format!("Bluetooth error: {m}")),
        }
    }

    struct PluginLink {
        app: AppHandle,
        incoming: Mutex<mpsc::UnboundedReceiver<BleEvent>>,
        /// Cleared when the plugin reports the link dropped.
        up: Arc<AtomicBool>,
        _sender: mpsc::UnboundedSender<BleEvent>,
    }

    #[async_trait]
    impl Link for PluginLink {
        async fn send(&self, bytes: &[u8]) -> std::result::Result<(), Fail> {
            let ble = self.app.state::<JadeBle<tauri::Wry>>();
            ble.write(bytes).await.map_err(|e| match e {
                BleError::PermissionDenied => Fail::App(super::super::allow_bluetooth()),
                // Includes a stale pairing: reconnecting spots it and re-pairs.
                other => {
                    tracing::info!("jade: write failed: {other}");
                    self.up.store(false, Ordering::SeqCst);
                    Fail::Link(other.to_string())
                }
            })
        }

        async fn recv(&self) -> std::result::Result<Vec<u8>, Fail> {
            let mut incoming = self.incoming.lock().await;
            loop {
                match incoming.recv().await {
                    Some(BleEvent::Data(d)) => return Ok(d),
                    Some(BleEvent::Disconnected(_)) | None => {
                        return Err(Fail::Link("Jade disconnected".into()))
                    }
                    Some(_) => continue,
                }
            }
        }

        fn is_up(&self) -> bool {
            self.up.load(Ordering::SeqCst)
        }

        async fn close(&self) {
            self.app.state::<JadeBle<tauri::Wry>>().disconnect().await;
        }
    }
}

#[cfg(target_os = "ios")]
mod imp {
    use sessions_core::{Error, Result};
    use tauri::AppHandle;

    use super::super::rpc::Link;

    pub async fn open(_app: &AppHandle) -> Result<Box<dyn Link>> {
        Err(Error::Invalid(
            "Jade over Bluetooth isn't available on iPhone yet. Use Jade's QR mode instead".into(),
        ))
    }
}

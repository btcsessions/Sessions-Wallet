//! Bluetooth LE links to Jade.
//!
//! Jade offers a UART-style GATT service: we write requests to its RX
//! characteristic (with response; Jade requires an encrypted, bonded link)
//! and it sends replies as indications on TX. Desktop uses btleplug; Android
//! goes through `tauri-plugin-jade`.

use sessions_core::Result;
use tauri::AppHandle;

use super::rpc::Link;

/// How long to look for a Jade before giving up.
const SCAN_MS: u64 = 15_000;

/// Find the nearest Jade, connect (pairing if the OS needs to) and return the
/// link. Emits the "connecting" status once a Jade is found.
pub async fn open(app: &AppHandle) -> Result<Box<dyn Link>> {
    imp::open(app).await
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
    use async_trait::async_trait;
    use sessions_core::{Error, Result};
    use tauri::{AppHandle, Manager};
    use tauri_plugin_jade::{BleError, BleEvent, JadeBle};
    use tokio::sync::{mpsc, Mutex};

    use super::super::rpc::{Fail, Link};
    use super::SCAN_MS;

    pub async fn open(app: &AppHandle) -> Result<Box<dyn Link>> {
        let ble = app.state::<JadeBle<tauri::Wry>>();
        let (sender, receiver) = mpsc::unbounded_channel();
        // The plugin keeps its callback for good; let it go quiet once the
        // link (holding the strong sender) is dropped.
        let weak = sender.downgrade();
        let ui = app.clone();
        ble.connect(SCAN_MS, move |e| match e {
            BleEvent::Connecting => super::super::status(&ui, "connecting"),
            other => {
                if let Some(s) = weak.upgrade() {
                    let _ = s.send(other);
                }
            }
        })
        .await
        .map_err(setup_error)?;
        Ok(Box::new(PluginLink {
            app: app.clone(),
            incoming: Mutex::new(receiver),
            _sender: sender,
        }))
    }

    fn setup_error(e: BleError) -> Error {
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
            BleError::Other(m) => {
                tracing::warn!("jade: bluetooth: {m}");
                Error::Invalid(format!("Bluetooth error: {m}"))
            }
        }
    }

    struct PluginLink {
        app: AppHandle,
        incoming: Mutex<mpsc::UnboundedReceiver<BleEvent>>,
        _sender: mpsc::UnboundedSender<BleEvent>,
    }

    #[async_trait]
    impl Link for PluginLink {
        async fn send(&self, bytes: &[u8]) -> std::result::Result<(), Fail> {
            let ble = self.app.state::<JadeBle<tauri::Wry>>();
            ble.write(bytes).await.map_err(|e| match e {
                BleError::PairingStale => Fail::App(super::super::forget_and_repair()),
                BleError::PermissionDenied => Fail::App(super::super::allow_bluetooth()),
                other => Fail::Link(other.to_string()),
            })
        }

        async fn recv(&self) -> std::result::Result<Vec<u8>, Fail> {
            let mut incoming = self.incoming.lock().await;
            loop {
                match incoming.recv().await {
                    Some(BleEvent::Data(d)) => return Ok(d),
                    Some(BleEvent::Connecting) => continue,
                    Some(BleEvent::Disconnected) | None => {
                        return Err(Fail::Link("Jade disconnected".into()))
                    }
                }
            }
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

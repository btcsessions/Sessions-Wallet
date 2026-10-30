//! Links to a Ledger: USB (HID) or Bluetooth LE, on a computer or a phone.
//!
//! A link exchanges whole APDUs; the framing is in [`super::frame`].
//! Computers use hidapi and btleplug; Android goes through the app's USB and
//! Bluetooth plugins (shared with BitBox02 and Jade).

use std::time::Duration;

use async_trait::async_trait;
use sessions_core::Error;

/// Vendor ID of every Ledger.
pub const VENDOR_ID: u16 = 0x2c97;

/// A Ledger's Bluetooth service and its notify / write characteristics, by
/// model: Nano X, Stax, Flex, Nano Gen5 (two sets).
pub const BLE_MODELS: &[(&str, u16)] = &[
    ("Nano X", 0x0004),
    ("Stax", 0x6004),
    ("Flex", 0x3004),
    ("Nano Gen5", 0x8004),
    ("Nano Gen5", 0x9004),
];

/// `13d63400-2c97-{model}-{part}-4c6564676572`: part 0 service, 1 notify,
/// 2 write, 3 write without response.
pub fn ble_uuid(model: u16, part: u16) -> String {
    format!("13d63400-2c97-{model:04x}-{part:04x}-4c6564676572")
}

/// How a link failed.
#[derive(Debug)]
pub enum Fail {
    /// The connection is gone (unplugged, out of range): reconnect.
    Link(String),
    /// Something to tell the person as it is.
    App(Error),
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::Link(e) => write!(f, "link: {e}"),
            Fail::App(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Usb,
    Bluetooth,
}

/// One APDU at a time, in, response (data + status word) out.
#[async_trait]
pub trait Link: Send + Sync {
    async fn exchange(&self, apdu: &[u8], wait: Duration) -> Result<Vec<u8>, Fail>;
    async fn close(&self);
    fn via(&self) -> Via;
}

pub fn not_found() -> Error {
    Error::Invalid(
        if cfg!(any(target_os = "android", target_os = "ios")) {
            "Plug in your Ledger, or turn it on with Bluetooth enabled and keep it close. Unlock it, then try again"
        } else {
            "Plug in your Ledger with a USB cable, or turn it on with Bluetooth enabled and keep it close. Unlock it, then try again"
        }
        .into(),
    )
}

pub fn lost_connection() -> Error {
    Error::Invalid("Lost the connection to the Ledger. Keep it unlocked and close by, then try again".into())
}

#[cfg_attr(target_os = "android", allow(dead_code))]
pub fn turn_on_bluetooth() -> Error {
    Error::Invalid("Turn on Bluetooth, or plug the Ledger in, then try again".into())
}

pub fn allow_bluetooth() -> Error {
    Error::Invalid(
        "Allow Bluetooth access for Sessions Wallet in your system settings, then try again".into(),
    )
}

pub fn pair_first() -> Error {
    Error::Invalid(
        "The Ledger and this device aren't paired, or the pairing is out of date. Remove the Ledger \
         from your Bluetooth settings, then try again and confirm the code on the Ledger"
            .into(),
    )
}

/// Open a link: USB when a Ledger is plugged in, otherwise Bluetooth.
/// `on_bluetooth` runs once a Bluetooth search starts (to say so on screen),
/// `on_pairing` when the system asks to pair.
pub async fn open(
    app: &tauri::AppHandle,
    on_bluetooth: impl Fn() + Send + Sync + 'static,
    on_pairing: impl Fn() + Send + Sync + 'static,
) -> sessions_core::Result<Box<dyn Link>> {
    match imp::open_usb(app).await {
        Ok(Some(link)) => return Ok(link),
        Ok(None) => {}
        Err(e) => return Err(e),
    }
    on_bluetooth();
    imp::open_ble(app, on_pairing).await
}

// --- computers -----------------------------------------------------------------

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod imp {
    use std::sync::{Arc, Mutex as SyncMutex};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use btleplug::api::{
        Central, CentralState, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
    };
    use btleplug::platform::{Adapter, Manager, Peripheral};
    use futures_util::StreamExt;
    use sessions_core::{Error, Result};
    use tauri::AppHandle;
    use tokio::sync::{mpsc, Mutex};
    use uuid::Uuid;

    use super::super::frame::{self, Reader, CHANNEL, HID_REPORT};
    use super::{ble_uuid, Fail, Link, Via, BLE_MODELS, VENDOR_ID};

    /// The Ledger's APDU interface (its others are FIDO and such).
    const USAGE_PAGE: u16 = 0xffa0;
    const SCAN: Duration = Duration::from_secs(15);
    const SETTLE: Duration = Duration::from_millis(1500);

    pub async fn open_usb(_app: &AppHandle) -> Result<Option<Box<dyn Link>>> {
        let dev = tokio::task::spawn_blocking(|| -> Result<Option<hidapi::HidDevice>> {
            let api = hidapi::HidApi::new()
                .map_err(|e| Error::Invalid(format!("Couldn't use USB on this computer: {e}")))?;
            let mut list: Vec<_> = api
                .device_list()
                .filter(|d| d.vendor_id() == VENDOR_ID)
                .collect();
            // The APDU interface: usage page 0xffa0 (macOS, Windows), or
            // interface 0 (Linux reports no usage page).
            list.sort_by_key(|d| (d.usage_page() != USAGE_PAGE, d.interface_number() != 0));
            let Some(info) = list.first() else {
                return Ok(None);
            };
            info.open_device(&api).map(Some).map_err(|e| {
                tracing::warn!("ledger: USB open failed: {e}");
                Error::Invalid(if cfg!(target_os = "linux") {
                    "Linux is blocking access to the Ledger. Add Ledger's udev rules (Ledger Live's \
                     \"Fix connection issues\" installs them), then unplug the Ledger and plug it back in"
                        .into()
                } else {
                    "Couldn't open the Ledger. Close other apps using it, like Ledger Live, and try again"
                        .into()
                })
            })
        })
        .await
        .map_err(|e| Error::Invalid(e.to_string()))??;
        Ok(dev.map(|d| Box::new(UsbLink(Arc::new(SyncMutex::new(d)))) as Box<dyn Link>))
    }

    struct UsbLink(Arc<SyncMutex<hidapi::HidDevice>>);

    #[async_trait]
    impl Link for UsbLink {
        async fn exchange(&self, apdu: &[u8], wait: Duration) -> std::result::Result<Vec<u8>, Fail> {
            let dev = self.0.clone();
            let apdu = apdu.to_vec();
            tokio::task::spawn_blocking(move || {
                let dev = dev.lock().unwrap_or_else(|e| e.into_inner());
                for f in frame::frames(&apdu, Some(CHANNEL), HID_REPORT) {
                    // hidapi wants a report ID first (none: 0).
                    let mut report = Vec::with_capacity(HID_REPORT + 1);
                    report.push(0);
                    report.extend_from_slice(&f);
                    dev.write(&report).map_err(|e| Fail::Link(e.to_string()))?;
                }
                let mut reader = Reader::new(Some(CHANNEL));
                let deadline = Instant::now() + wait;
                let mut buf = [0u8; HID_REPORT];
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(Fail::App(Error::Invalid(
                            "The Ledger didn't answer in time. Try again".into(),
                        )));
                    }
                    let ms = left.as_millis().min(1000) as i32;
                    let n = dev
                        .read_timeout(&mut buf, ms)
                        .map_err(|e| Fail::Link(e.to_string()))?;
                    if n == 0 {
                        continue;
                    }
                    match reader.push(&buf[..n]) {
                        Ok(Some(resp)) => return Ok(resp),
                        Ok(None) | Err(frame::FrameError::Ignore) => {}
                        Err(frame::FrameError::Bad(why)) => return Err(Fail::Link(why.into())),
                    }
                }
            })
            .await
            .map_err(|e| Fail::Link(e.to_string()))?
        }

        async fn close(&self) {}

        fn via(&self) -> Via {
            Via::Usb
        }
    }

    // --- Bluetooth ------------------------------------------------------------

    fn uuid(model: u16, part: u16) -> Uuid {
        Uuid::parse_str(&ble_uuid(model, part)).expect("valid UUID")
    }

    pub async fn open_ble(_app: &AppHandle, _on_pairing: impl Fn()) -> Result<Box<dyn Link>> {
        let manager = Manager::new().await.map_err(setup_error)?;
        let adapter = manager
            .adapters()
            .await
            .map_err(setup_error)?
            .into_iter()
            .next()
            .ok_or_else(super::not_found)?;
        if matches!(adapter.adapter_state().await, Ok(CentralState::PoweredOff)) {
            return Err(super::turn_on_bluetooth());
        }
        let services: Vec<Uuid> = BLE_MODELS.iter().map(|(_, m)| uuid(*m, 0)).collect();
        adapter
            .start_scan(ScanFilter {
                services: services.clone(),
            })
            .await
            .map_err(setup_error)?;
        let found = find(&adapter, &services).await;
        let _ = adapter.stop_scan().await;
        let peripheral = found?.ok_or_else(super::not_found)?;
        connect(adapter, peripheral).await
    }

    async fn find(adapter: &Adapter, services: &[Uuid]) -> Result<Option<Peripheral>> {
        let start = Instant::now();
        let mut first_seen: Option<Instant> = None;
        let mut best: Option<(i16, Peripheral)> = None;
        loop {
            for p in adapter.peripherals().await.map_err(setup_error)? {
                let Ok(Some(props)) = p.properties().await else {
                    continue;
                };
                let is_ledger = props.services.iter().any(|s| services.contains(s));
                let Some(rssi) = props.rssi else { continue };
                if is_ledger && best.as_ref().is_none_or(|(r, _)| rssi > *r) {
                    best = Some((rssi, p));
                }
            }
            if best.is_some() && first_seen.get_or_insert_with(Instant::now).elapsed() >= SETTLE {
                break;
            }
            if start.elapsed() >= SCAN {
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Ok(best.map(|(_, p)| p))
    }

    async fn connect(adapter: Adapter, peripheral: Peripheral) -> Result<Box<dyn Link>> {
        if !peripheral.is_connected().await.unwrap_or(false) {
            match tokio::time::timeout(Duration::from_secs(20), peripheral.connect()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!("ledger: connect failed: {e}");
                    return Err(super::not_found());
                }
                Err(_) => return Err(super::not_found()),
            }
        }
        let setup = async {
            peripheral.discover_services().await.map_err(setup_error)?;
            let chars = peripheral.characteristics();
            let model = BLE_MODELS
                .iter()
                .map(|(_, m)| *m)
                .find(|m| chars.iter().any(|c| c.uuid == uuid(*m, 1)))
                .ok_or_else(|| Error::Invalid("That Bluetooth device isn't a Ledger".into()))?;
            let find = |part| chars.iter().find(|c| c.uuid == uuid(model, part)).cloned();
            let (Some(notify), Some(write)) = (find(1), find(2)) else {
                return Err(Error::Invalid("That Bluetooth device isn't a Ledger".into()));
            };
            let mut stream = peripheral.notifications().await.map_err(setup_error)?;
            peripheral.subscribe(&notify).await.map_err(|e| match link_error(e) {
                Fail::App(e) => e,
                Fail::Link(_) => super::lost_connection(),
            })?;
            let (sender, receiver) = mpsc::unbounded_channel();
            let notify_uuid = notify.uuid;
            let task = tokio::spawn(async move {
                while let Some(n) = stream.next().await {
                    if n.uuid == notify_uuid && sender.send(n.value).is_err() {
                        break;
                    }
                }
            });
            Ok((write, receiver, task))
        };
        let (write, receiver, task) = match setup.await {
            Ok(x) => x,
            Err(e) => {
                let _ = peripheral.disconnect().await;
                return Err(e);
            }
        };
        let link = BleLink {
            _adapter: adapter,
            peripheral,
            write,
            incoming: Mutex::new(receiver),
            task,
            mtu: std::sync::atomic::AtomicUsize::new(20),
        };
        link.negotiate().await.map_err(|e| match e {
            Fail::App(e) => e,
            Fail::Link(_) => super::lost_connection(),
        })?;
        Ok(Box::new(link))
    }

    struct BleLink {
        _adapter: Adapter,
        peripheral: Peripheral,
        write: Characteristic,
        incoming: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
        task: tokio::task::JoinHandle<()>,
        /// Frame size the Ledger takes.
        mtu: std::sync::atomic::AtomicUsize,
    }

    impl BleLink {
        async fn send(&self, bytes: &[u8]) -> std::result::Result<(), Fail> {
            self.peripheral
                .write(&self.write, bytes, WriteType::WithResponse)
                .await
                .map_err(link_error)
        }

        async fn recv(&self, wait: Duration) -> std::result::Result<Vec<u8>, Fail> {
            let mut incoming = self.incoming.lock().await;
            let deadline = Instant::now() + wait;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(Fail::App(Error::Invalid(
                        "The Ledger didn't answer in time. Try again".into(),
                    )));
                }
                tokio::select! {
                    msg = incoming.recv() => {
                        return msg.ok_or_else(|| Fail::Link("disconnected".into()));
                    }
                    _ = tokio::time::sleep(left.min(Duration::from_secs(2))) => {
                        if !self.peripheral.is_connected().await.unwrap_or(false) {
                            return Err(Fail::Link("disconnected".into()));
                        }
                    }
                }
            }
        }

        /// Ask the Ledger which frame size it takes.
        async fn negotiate(&self) -> std::result::Result<(), Fail> {
            self.send(&[frame::TAG_MTU, 0, 0, 0, 0]).await?;
            let resp = self.recv(Duration::from_secs(10)).await?;
            if resp.len() >= 6 && resp[0] == frame::TAG_MTU {
                let mtu = (resp[5] as usize).max(20);
                self.mtu.store(mtu, std::sync::atomic::Ordering::SeqCst);
                tracing::info!("ledger: bluetooth frames of {mtu} bytes");
            }
            Ok(())
        }
    }

    #[async_trait]
    impl Link for BleLink {
        async fn exchange(&self, apdu: &[u8], wait: Duration) -> std::result::Result<Vec<u8>, Fail> {
            let mtu = self.mtu.load(std::sync::atomic::Ordering::SeqCst);
            for f in frame::frames(apdu, None, mtu) {
                self.send(&f).await?;
            }
            let mut reader = Reader::new(None);
            let deadline = Instant::now() + wait;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let f = self.recv(left).await?;
                match reader.push(&f) {
                    Ok(Some(resp)) => return Ok(resp),
                    Ok(None) | Err(frame::FrameError::Ignore) => {}
                    Err(frame::FrameError::Bad(why)) => return Err(Fail::Link(why.into())),
                }
            }
        }

        async fn close(&self) {
            self.task.abort();
            let _ = self.peripheral.disconnect().await;
        }

        fn via(&self) -> Via {
            Via::Bluetooth
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
            E::PermissionDenied => super::allow_bluetooth(),
            E::NoAdapterAvailable => super::turn_on_bluetooth(),
            _ => {
                let msg = e.to_string();
                tracing::warn!("ledger: bluetooth: {msg}");
                if msg.contains("NotReady") || msg.to_ascii_lowercase().contains("powered off") {
                    super::turn_on_bluetooth()
                } else {
                    Error::Invalid(format!("Bluetooth error: {msg}"))
                }
            }
        }
    }

    fn link_error(e: btleplug::Error) -> Fail {
        use btleplug::Error as E;
        match e {
            E::NotConnected | E::DeviceNotFound => Fail::Link(e.to_string()),
            E::PermissionDenied => Fail::App(super::allow_bluetooth()),
            other => {
                let msg = other.to_string();
                let m = msg.to_ascii_lowercase();
                tracing::warn!("ledger: bluetooth write failed: {msg}");
                if ["authenticat", "encrypt", "notauthorized", "not authorized", "not paired"]
                    .iter()
                    .any(|n| m.contains(n))
                {
                    Fail::App(super::pair_first())
                } else {
                    Fail::Link(msg)
                }
            }
        }
    }
}

// --- Android ---------------------------------------------------------------------

#[cfg(target_os = "android")]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use sessions_core::{Error, Result};
    use tauri::{AppHandle, Manager};
    use tauri_plugin_bitbox::{BitboxUsb, UsbError};
    use tauri_plugin_jade::{BleError, BleEvent, BleProfile, ConnectOutcome, JadeBle};
    use tokio::sync::{mpsc, Mutex};

    use super::super::frame::{self, Reader, CHANNEL, HID_REPORT};
    use super::{ble_uuid, Fail, Link, Via, BLE_MODELS, VENDOR_ID};

    /// Android's "allow access" prompt waits on the person.
    const PERMISSION_WAIT: Duration = Duration::from_secs(120);

    pub async fn open_usb(app: &AppHandle) -> Result<Option<Box<dyn Link>>> {
        let usb = app.state::<BitboxUsb<tauri::Wry>>();
        match tokio::time::timeout(PERMISSION_WAIT, usb.open_hid(VENDOR_ID, None, "Ledger")).await {
            Err(_) => Err(allow_usb()),
            Ok(Err(UsbError::NotFound)) => Ok(None),
            Ok(Err(UsbError::PermissionDenied)) => Err(allow_usb()),
            Ok(Err(UsbError::Busy)) => Err(Error::Invalid(
                "Another app is using the Ledger. Close it (Ledger Live, for example) and try again".into(),
            )),
            Ok(Err(e)) => {
                tracing::warn!("ledger: USB open failed: {e}");
                Err(Error::Invalid(
                    "Couldn't open the Ledger. Unplug it, plug it back in and try again".into(),
                ))
            }
            Ok(Ok(opened)) => {
                tracing::info!("ledger: opened {} over USB", opened.product);
                Ok(Some(Box::new(UsbLink { app: app.clone() })))
            }
        }
    }

    fn allow_usb() -> Error {
        Error::Invalid("Allow Sessions Wallet to use the Ledger when Android asks, then try again".into())
    }

    struct UsbLink {
        app: AppHandle,
    }

    #[async_trait]
    impl Link for UsbLink {
        async fn exchange(&self, apdu: &[u8], wait: Duration) -> std::result::Result<Vec<u8>, Fail> {
            let usb = self.app.state::<BitboxUsb<tauri::Wry>>();
            let mut out = Vec::new();
            for f in frame::frames(apdu, Some(CHANNEL), HID_REPORT) {
                out.extend_from_slice(&f);
            }
            usb.write(&out).await.map_err(|e| Fail::Link(e.to_string()))?;
            let mut reader = Reader::new(Some(CHANNEL));
            let deadline = Instant::now() + wait;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(Fail::App(Error::Invalid(
                        "The Ledger didn't answer in time. Try again".into(),
                    )));
                }
                let report = match usb.read(left.min(Duration::from_secs(30))).await {
                    Ok(r) => r,
                    Err(UsbError::Timeout) => continue,
                    Err(e) => return Err(Fail::Link(e.to_string())),
                };
                match reader.push(&report) {
                    Ok(Some(resp)) => return Ok(resp),
                    Ok(None) | Err(frame::FrameError::Ignore) => {}
                    Err(frame::FrameError::Bad(why)) => return Err(Fail::Link(why.into())),
                }
            }
        }

        async fn close(&self) {
            self.app.state::<BitboxUsb<tauri::Wry>>().close().await;
        }

        fn via(&self) -> Via {
            Via::Usb
        }
    }

    fn profile() -> BleProfile {
        BleProfile {
            label: "Ledger".into(),
            services: BLE_MODELS.iter().map(|(_, m)| ble_uuid(*m, 0)).collect(),
            writes: BLE_MODELS.iter().map(|(_, m)| ble_uuid(*m, 2)).collect(),
            notifies: BLE_MODELS.iter().map(|(_, m)| ble_uuid(*m, 1)).collect(),
            names: vec!["Ledger".into(), "Nano".into(), "Stax".into(), "Flex".into()],
        }
    }

    pub async fn open_ble(app: &AppHandle, on_pairing: impl Fn() + Send + Sync + 'static) -> Result<Box<dyn Link>> {
        let ble = app.state::<JadeBle<tauri::Wry>>();
        let (sender, receiver) = mpsc::unbounded_channel();
        let up = Arc::new(AtomicBool::new(true));
        let up2 = up.clone();
        let on_pairing = Arc::new(on_pairing);
        let mut last = None;
        for attempt in 0..2 {
            let sender = sender.clone();
            let up = up2.clone();
            let on_pairing = on_pairing.clone();
            let outcome = ble
                .connect_device(Some(&profile()), 15_000, false, move |e| match e {
                    BleEvent::Data(d) => {
                        let _ = sender.send(d);
                    }
                    BleEvent::Pairing => on_pairing(),
                    BleEvent::Disconnected(_) => up.store(false, Ordering::SeqCst),
                    _ => {}
                })
                .await
                .map_err(ble_error)?;
            match outcome {
                ConnectOutcome::Connected(c) => {
                    tracing::info!("ledger: connected to {} over Bluetooth (mtu {})", c.name, c.mtu);
                    up.store(true, Ordering::SeqCst);
                    let link = BleLink {
                        app: app.clone(),
                        incoming: Mutex::new(receiver),
                        up: up2,
                        mtu: AtomicUsize::new((c.mtu as usize).saturating_sub(3).max(20)),
                    };
                    link.negotiate().await.map_err(|e| match e {
                        Fail::App(e) => e,
                        Fail::Link(_) => super::lost_connection(),
                    })?;
                    return Ok(Box::new(link));
                }
                ConnectOutcome::Failed(f) => {
                    tracing::warn!("ledger: bluetooth connect attempt {attempt} failed: {f:?}");
                    last = Some(f);
                }
            }
        }
        Err(match last {
            Some(f) if f.fresh_bond => Error::Invalid(
                "Pairing with the Ledger didn't finish. Try again, and confirm the same code on the Ledger and your phone".into(),
            ),
            Some(f) if matches!(f.kind.as_str(), "auth" | "bondLost") => super::pair_first(),
            _ => super::not_found(),
        })
    }

    fn ble_error(e: BleError) -> Error {
        match e {
            BleError::NoBluetooth | BleError::BluetoothOff => {
                Error::Invalid("Turn on Bluetooth, or plug the Ledger in, then try again".into())
            }
            BleError::LocationOff => Error::Invalid("Turn on Location, then try again".into()),
            BleError::PermissionDenied => super::allow_bluetooth(),
            BleError::NotFound => super::not_found(),
            BleError::PairingFailed => Error::Invalid(
                "Pairing with the Ledger didn't finish. Try again, and confirm the code on the Ledger".into(),
            ),
            BleError::PairingStale => super::pair_first(),
            BleError::Disconnected => super::lost_connection(),
            BleError::Other(m) => Error::Invalid(format!("Bluetooth error: {m}")),
        }
    }

    struct BleLink {
        app: AppHandle,
        incoming: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
        up: Arc<AtomicBool>,
        mtu: AtomicUsize,
    }

    impl BleLink {
        async fn send(&self, bytes: &[u8]) -> std::result::Result<(), Fail> {
            let ble = self.app.state::<JadeBle<tauri::Wry>>();
            ble.write(bytes).await.map_err(|e| match e {
                BleError::PairingStale => Fail::App(super::pair_first()),
                other => Fail::Link(other.to_string()),
            })
        }

        async fn recv(&self, wait: Duration) -> std::result::Result<Vec<u8>, Fail> {
            let mut incoming = self.incoming.lock().await;
            let deadline = Instant::now() + wait;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(Fail::App(Error::Invalid(
                        "The Ledger didn't answer in time. Try again".into(),
                    )));
                }
                tokio::select! {
                    msg = incoming.recv() => return msg.ok_or_else(|| Fail::Link("disconnected".into())),
                    _ = tokio::time::sleep(left.min(Duration::from_secs(1))) => {
                        if !self.up.load(Ordering::SeqCst) {
                            return Err(Fail::Link("disconnected".into()));
                        }
                    }
                }
            }
        }

        async fn negotiate(&self) -> std::result::Result<(), Fail> {
            self.send(&[frame::TAG_MTU, 0, 0, 0, 0]).await?;
            let resp = self.recv(Duration::from_secs(10)).await?;
            if resp.len() >= 6 && resp[0] == frame::TAG_MTU {
                let mtu = (resp[5] as usize).max(20).min(self.mtu.load(Ordering::SeqCst).max(20));
                self.mtu.store(mtu, Ordering::SeqCst);
                tracing::info!("ledger: bluetooth frames of {mtu} bytes");
            }
            Ok(())
        }
    }

    #[async_trait]
    impl Link for BleLink {
        async fn exchange(&self, apdu: &[u8], wait: Duration) -> std::result::Result<Vec<u8>, Fail> {
            if !self.up.load(Ordering::SeqCst) {
                return Err(Fail::Link("disconnected".into()));
            }
            let mtu = self.mtu.load(Ordering::SeqCst);
            for f in frame::frames(apdu, None, mtu) {
                self.send(&f).await?;
            }
            let mut reader = Reader::new(None);
            let deadline = Instant::now() + wait;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let f = self.recv(left).await?;
                match reader.push(&f) {
                    Ok(Some(resp)) => return Ok(resp),
                    Ok(None) | Err(frame::FrameError::Ignore) => {}
                    Err(frame::FrameError::Bad(why)) => return Err(Fail::Link(why.into())),
                }
            }
        }

        async fn close(&self) {
            self.app.state::<JadeBle<tauri::Wry>>().disconnect().await;
        }

        fn via(&self) -> Via {
            Via::Bluetooth
        }
    }
}

#[cfg(target_os = "ios")]
mod imp {
    use sessions_core::{Error, Result};
    use tauri::AppHandle;

    use super::Link;

    pub async fn open_usb(_app: &AppHandle) -> Result<Option<Box<dyn Link>>> {
        Ok(None)
    }

    pub async fn open_ble(_app: &AppHandle, _on_pairing: impl Fn()) -> Result<Box<dyn Link>> {
        Err(Error::Unsupported("connect a Ledger here yet".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ble_uuids_match_ledgers_list() {
        assert_eq!(ble_uuid(0x3004, 0), "13d63400-2c97-3004-0000-4c6564676572");
        assert_eq!(ble_uuid(0x8004, 1), "13d63400-2c97-8004-0001-4c6564676572");
        assert_eq!(ble_uuid(0x0004, 2), "13d63400-2c97-0004-0002-4c6564676572");
    }
}

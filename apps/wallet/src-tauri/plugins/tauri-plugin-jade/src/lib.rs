//! Bluetooth LE access to a Blockstream Jade on Android.
//!
//! The Kotlin side (`android/`) asks for the Bluetooth permissions, scans for
//! Jade's UART service, connects, bonds (Android shows the pairing dialog,
//! Jade shows the matching number), turns on indications and writes. Bytes
//! Jade sends come back through a channel. The wallet speaks Jade's CBOR RPC
//! on top.
//!
//! On other platforms the plugin does nothing.

use tauri::plugin::{Builder, TauriPlugin};
use tauri::Runtime;

#[cfg(target_os = "android")]
pub use android::{BleError, BleEvent, Connected, JadeBle};

/// Register with `tauri::Builder::plugin`.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("jade")
        .setup(|_app, _api| {
            #[cfg(target_os = "android")]
            {
                use tauri::Manager;
                let handle = _api.register_android_plugin("io.btcsessions.jade", "JadePlugin")?;
                _app.manage(JadeBle(handle));
            }
            Ok(())
        })
        .build()
}

#[cfg(target_os = "android")]
mod android {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    use serde::Deserialize;
    use tauri::ipc::{Channel, InvokeResponseBody};
    use tauri::plugin::mobile::PluginInvokeError;
    use tauri::plugin::PluginHandle;
    use tauri::Runtime;

    /// Managed state: `app.state::<JadeBle<R>>()`.
    pub struct JadeBle<R: Runtime>(pub(crate) PluginHandle<R>);

    /// What arrives from Jade after `connect`.
    #[derive(Debug)]
    pub enum BleEvent {
        /// Found a Jade; connecting (and pairing, if needed).
        Connecting,
        /// Bytes Jade sent (one indication).
        Data(Vec<u8>),
        /// The link dropped.
        Disconnected,
    }

    /// A connected, bonded Jade with indications on.
    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Connected {
        /// Advertised name, e.g. "Jade ABCDEF".
        pub name: String,
        /// Negotiated ATT MTU.
        pub mtu: u32,
    }

    #[derive(Debug)]
    pub enum BleError {
        /// The phone has no Bluetooth LE.
        NoBluetooth,
        /// Bluetooth is switched off.
        BluetoothOff,
        /// Android 11 and older only scan with Location switched on.
        LocationOff,
        /// The user declined the Bluetooth (or, before Android 12, location) permission.
        PermissionDenied,
        /// No Jade seen while scanning.
        NotFound,
        /// Pairing was declined or failed.
        PairingFailed,
        /// Android has a pairing Jade no longer knows: forget it in settings.
        PairingStale,
        /// Not connected, or the link dropped.
        Disconnected,
        Other(String),
    }

    impl std::fmt::Display for BleError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                BleError::NoBluetooth => f.write_str("no Bluetooth LE"),
                BleError::BluetoothOff => f.write_str("Bluetooth is off"),
                BleError::LocationOff => f.write_str("Location is off"),
                BleError::PermissionDenied => f.write_str("Bluetooth access was not allowed"),
                BleError::NotFound => f.write_str("no Jade found"),
                BleError::PairingFailed => f.write_str("pairing failed"),
                BleError::PairingStale => f.write_str("stale pairing"),
                BleError::Disconnected => f.write_str("disconnected"),
                BleError::Other(e) => f.write_str(e),
            }
        }
    }

    impl std::error::Error for BleError {}

    fn map_err(e: PluginInvokeError) -> BleError {
        match e {
            PluginInvokeError::InvokeRejected(r) => match r.code.as_deref() {
                Some("NO_BLUETOOTH") => BleError::NoBluetooth,
                Some("BLUETOOTH_OFF") => BleError::BluetoothOff,
                Some("LOCATION_OFF") => BleError::LocationOff,
                Some("PERMISSION_DENIED") => BleError::PermissionDenied,
                Some("NOT_FOUND") => BleError::NotFound,
                Some("PAIRING_FAILED") => BleError::PairingFailed,
                Some("PAIRING_STALE") => BleError::PairingStale,
                Some("DISCONNECTED") | Some("NOT_CONNECTED") => BleError::Disconnected,
                _ => BleError::Other(r.message.unwrap_or_else(|| "Bluetooth error".into())),
            },
            other => BleError::Other(other.to_string()),
        }
    }

    /// One message from the Kotlin side: `{ data: base64 }`, `{ connecting: true }`
    /// or `{ disconnected: true }`.
    #[derive(Deserialize)]
    struct Incoming {
        data: Option<String>,
        #[serde(default)]
        connecting: bool,
        #[serde(default)]
        disconnected: bool,
    }

    fn parse(body: InvokeResponseBody) -> Option<BleEvent> {
        let incoming: Incoming = match body {
            InvokeResponseBody::Json(s) => serde_json::from_str(&s).ok()?,
            InvokeResponseBody::Raw(b) => serde_json::from_slice(&b).ok()?,
        };
        if incoming.disconnected {
            return Some(BleEvent::Disconnected);
        }
        if incoming.connecting {
            return Some(BleEvent::Connecting);
        }
        incoming
            .data
            .and_then(|d| B64.decode(d).ok())
            .map(BleEvent::Data)
    }

    impl<R: Runtime> JadeBle<R> {
        /// Ask for Bluetooth access if needed, scan for up to `scan_ms`,
        /// connect to the strongest Jade, bond (Android shows its pairing
        /// dialog) and turn on indications. `on_event` hears `Connecting` once
        /// a Jade is found, then everything Jade sends, in order, until the
        /// link drops.
        pub async fn connect<F>(&self, scan_ms: u64, on_event: F) -> Result<Connected, BleError>
        where
            F: Fn(BleEvent) + Send + Sync + 'static,
        {
            let channel: Channel<serde_json::Value> = Channel::new(move |body| {
                if let Some(e) = parse(body) {
                    on_event(e);
                }
                Ok(())
            });
            self.0
                .run_mobile_plugin_async(
                    "connect",
                    serde_json::json!({ "onData": channel, "scanMs": scan_ms }),
                )
                .await
                .map_err(map_err)
        }

        /// Write bytes to Jade, split to the link's MTU. Returns once every
        /// chunk is acknowledged.
        pub async fn write(&self, bytes: &[u8]) -> Result<(), BleError> {
            self.0
                .run_mobile_plugin_async::<serde_json::Value>(
                    "write",
                    serde_json::json!({ "data": B64.encode(bytes) }),
                )
                .await
                .map(|_| ())
                .map_err(map_err)
        }

        /// Drop the connection (the pairing stays).
        pub async fn disconnect(&self) {
            let _ = self
                .0
                .run_mobile_plugin_async::<serde_json::Value>("disconnect", serde_json::json!({}))
                .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_incoming() {
            let json = |s: &str| InvokeResponseBody::Json(s.to_string());
            assert!(matches!(
                parse(json(r#"{"data":"AQID"}"#)),
                Some(BleEvent::Data(d)) if d == vec![1, 2, 3]
            ));
            assert!(matches!(
                parse(json(r#"{"disconnected":true}"#)),
                Some(BleEvent::Disconnected)
            ));
            assert!(matches!(
                parse(json(r#"{"connecting":true}"#)),
                Some(BleEvent::Connecting)
            ));
            assert!(parse(json("{}")).is_none());
        }
    }
}

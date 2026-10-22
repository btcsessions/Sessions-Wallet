//! Bluetooth LE access to a Blockstream Jade on Android.
//!
//! The Kotlin side (`android/`) asks for the Bluetooth permissions, scans for
//! Jade's UART service, connects (to the Jade the phone is already paired
//! with, by its identity address, when there is one), bonds if needed
//! (Android shows the pairing dialog, Jade shows the matching number), turns
//! on indications and writes. Bytes Jade sends come back through a channel.
//! The wallet speaks Jade's CBOR RPC on top, and decides what to do when the
//! link fails ([`LinkFailure`]): retry, re-pair (`connect` with `repair`) or
//! give up.
//!
//! On other platforms the plugin does nothing.

use tauri::plugin::{Builder, TauriPlugin};
use tauri::Runtime;

#[cfg(target_os = "android")]
pub use android::{BleError, BleEvent, ConnectOutcome, Connected, JadeBle, LinkFailure};

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
        /// Found a Jade; connecting.
        Connecting,
        /// Android is showing its pairing dialog; Jade shows the number.
        Pairing,
        /// Bytes Jade sent (one indication).
        Data(Vec<u8>),
        /// The link dropped (GATT status / HCI reason).
        Disconnected(i32),
        /// The app went to the background (`true`) or came back (`false`).
        Background(bool),
    }

    /// A connected, bonded Jade with indications on.
    #[derive(Debug)]
    pub struct Connected {
        /// Advertised name, e.g. "Jade ABCDEF".
        pub name: String,
        /// Negotiated ATT MTU.
        pub mtu: u32,
    }

    /// Why a connect attempt failed, for the caller to decide what's next.
    #[derive(Debug, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct LinkFailure {
        /// "connect" (no link), "dropped" (link went down while setting up),
        /// "auth" (Jade refused our keys), "bondLost" (Android dropped the
        /// pairing).
        pub kind: String,
        /// GATT status or HCI reason from Android.
        pub status: i32,
        /// The phone was already paired with this Jade before the attempt.
        pub bonded: bool,
        /// The attempt paired just now.
        pub fresh_bond: bool,
    }

    /// A connect attempt's result.
    #[derive(Debug)]
    pub enum ConnectOutcome {
        Connected(Connected),
        Failed(LinkFailure),
    }

    #[derive(Deserialize)]
    struct ConnectReply {
        name: Option<String>,
        mtu: Option<u32>,
        failure: Option<LinkFailure>,
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
        /// Android has a pairing Jade no longer knows, and couldn't remove it:
        /// forget Jade in settings.
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

    /// One message from the Kotlin side: `{ data: base64 }`, `{ connecting: true }`,
    /// `{ pairing: true }`, `{ disconnected: true, status }` or `{ background: bool }`.
    #[derive(Deserialize)]
    struct Incoming {
        data: Option<String>,
        #[serde(default)]
        connecting: bool,
        #[serde(default)]
        pairing: bool,
        #[serde(default)]
        disconnected: bool,
        #[serde(default)]
        status: i32,
        background: Option<bool>,
    }

    fn parse(body: InvokeResponseBody) -> Option<BleEvent> {
        let incoming: Incoming = match body {
            InvokeResponseBody::Json(s) => serde_json::from_str(&s).ok()?,
            InvokeResponseBody::Raw(b) => serde_json::from_slice(&b).ok()?,
        };
        if incoming.disconnected {
            return Some(BleEvent::Disconnected(incoming.status));
        }
        if incoming.connecting {
            return Some(BleEvent::Connecting);
        }
        if incoming.pairing {
            return Some(BleEvent::Pairing);
        }
        if let Some(b) = incoming.background {
            return Some(BleEvent::Background(b));
        }
        incoming
            .data
            .and_then(|d| B64.decode(d).ok())
            .map(BleEvent::Data)
    }

    impl<R: Runtime> JadeBle<R> {
        /// Ask for Bluetooth access if needed, scan for up to `scan_ms`,
        /// connect to the paired (else strongest) Jade, bond if needed
        /// (Android shows its pairing dialog) and turn on indications.
        /// `repair` first removes the phone's stale pairing with the last
        /// Jade, so it pairs afresh. `on_event` hears `Connecting` once a Jade
        /// is found (`Pairing` if the dialog comes up), then everything Jade
        /// sends, in order, until the link drops.
        ///
        /// Link failures come back as [`ConnectOutcome::Failed`] for the
        /// caller to decide on; other problems (permissions, Bluetooth off, no
        /// Jade around) as errors.
        pub async fn connect<F>(
            &self,
            scan_ms: u64,
            repair: bool,
            on_event: F,
        ) -> Result<ConnectOutcome, BleError>
        where
            F: Fn(BleEvent) + Send + Sync + 'static,
        {
            let channel: Channel<serde_json::Value> = Channel::new(move |body| {
                if let Some(e) = parse(body) {
                    on_event(e);
                }
                Ok(())
            });
            let reply: ConnectReply = self
                .0
                .run_mobile_plugin_async(
                    "connect",
                    serde_json::json!({ "onData": channel, "scanMs": scan_ms, "repair": repair }),
                )
                .await
                .map_err(map_err)?;
            match reply {
                ConnectReply {
                    failure: Some(f), ..
                } => Ok(ConnectOutcome::Failed(f)),
                ConnectReply {
                    name: Some(name),
                    mtu: Some(mtu),
                    ..
                } => Ok(ConnectOutcome::Connected(Connected { name, mtu })),
                _ => Err(BleError::Other(
                    "unexpected reply from the Bluetooth plugin".into(),
                )),
            }
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
                parse(json(r#"{"disconnected":true,"status":8}"#)),
                Some(BleEvent::Disconnected(8))
            ));
            assert!(matches!(
                parse(json(r#"{"pairing":true}"#)),
                Some(BleEvent::Pairing)
            ));
            assert!(matches!(
                parse(json(r#"{"background":true}"#)),
                Some(BleEvent::Background(true))
            ));
            assert!(matches!(
                parse(json(r#"{"connecting":true}"#)),
                Some(BleEvent::Connecting)
            ));
            assert!(parse(json("{}")).is_none());
        }
    }
}

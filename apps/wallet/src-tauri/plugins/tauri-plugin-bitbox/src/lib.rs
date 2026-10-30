//! USB access to a BitBox02 on Android.
//!
//! Android apps can't open USB devices on their own: the user grants access
//! per device through `UsbManager`. The Kotlin side (`android/`) finds the
//! BitBox02, asks for permission, opens it and claims its HID interface, then
//! moves raw 64-byte HID reports with `bulkTransfer` on a background thread,
//! as the BitBoxApp does. The wallet's Rust code (bitbox-api) does the U2F-HID
//! framing and the protocol on top, through `write` and `read`.
//!
//! On other platforms the plugin does nothing.

use tauri::plugin::{Builder, TauriPlugin};
use tauri::Runtime;

#[cfg(target_os = "android")]
pub use android::{BitboxUsb, Opened, UsbError};

/// Register with `tauri::Builder::plugin`.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("bitbox")
        .setup(|_app, _api| {
            #[cfg(target_os = "android")]
            {
                use tauri::Manager;
                let handle =
                    _api.register_android_plugin("io.btcsessions.bitbox", "BitboxPlugin")?;
                _app.manage(BitboxUsb(handle));
            }
            Ok(())
        })
        .build()
}

#[cfg(target_os = "android")]
mod android {
    use std::time::Duration;

    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    use serde::Deserialize;
    use tauri::plugin::mobile::PluginInvokeError;
    use tauri::plugin::PluginHandle;
    use tauri::Runtime;

    /// Managed state: `app.state::<BitboxUsb<R>>()`.
    pub struct BitboxUsb<R: Runtime>(pub(crate) PluginHandle<R>);

    /// The BitBox02 is open, its HID interface claimed.
    #[derive(Debug, Deserialize)]
    pub struct Opened {
        /// USB product string, e.g. "BitBox02BTC".
        pub product: String,
    }

    #[derive(Debug)]
    pub enum UsbError {
        /// No BitBox02 is plugged in.
        NotFound,
        /// The user declined Android's USB access prompt.
        PermissionDenied,
        /// Another app holds the device.
        Busy,
        /// Nothing arrived within the read timeout.
        Timeout,
        Other(String),
    }

    impl std::fmt::Display for UsbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                UsbError::NotFound => f.write_str("no BitBox02 found"),
                UsbError::PermissionDenied => f.write_str("USB access was not allowed"),
                UsbError::Busy => f.write_str("in use by another app"),
                UsbError::Timeout => f.write_str("no answer"),
                UsbError::Other(e) => f.write_str(e),
            }
        }
    }

    impl std::error::Error for UsbError {}

    #[derive(Deserialize)]
    struct ReadReply {
        data: String,
    }

    fn map_err(e: PluginInvokeError) -> UsbError {
        match e {
            PluginInvokeError::InvokeRejected(r) => match r.code.as_deref() {
                Some("NOT_FOUND") => UsbError::NotFound,
                Some("PERMISSION_DENIED") => UsbError::PermissionDenied,
                Some("BUSY") => UsbError::Busy,
                Some("TIMEOUT") => UsbError::Timeout,
                code => UsbError::Other(format!(
                    "{} ({})",
                    r.message.as_deref().unwrap_or("USB error"),
                    code.unwrap_or("?")
                )),
            },
            other => UsbError::Other(other.to_string()),
        }
    }

    impl<R: Runtime> BitboxUsb<R> {
        /// Find the BitBox02, ask for USB access if needed (Android shows a
        /// prompt), open it and claim its HID interface.
        pub async fn open(&self) -> Result<Opened, UsbError> {
            self.0
                .run_mobile_plugin_async("open", serde_json::json!({}))
                .await
                .map_err(map_err)
        }

        /// Open another HID device the same way: the first one from
        /// `vendor_id` (and `product_id`, or any with `None`). `label` names
        /// it in errors. Used for Ledger (one HID device open at a time).
        pub async fn open_hid(
            &self,
            vendor_id: u16,
            product_id: Option<u16>,
            label: &str,
        ) -> Result<Opened, UsbError> {
            self.0
                .run_mobile_plugin_async(
                    "open",
                    serde_json::json!({
                        "vendorId": vendor_id,
                        "productId": product_id.map(i32::from).unwrap_or(-1),
                        "label": label,
                    }),
                )
                .await
                .map_err(map_err)
        }

        /// Send `data` as 64-byte HID reports (raw: no report-ID byte; the
        /// last report is zero-padded).
        pub async fn write(&self, data: &[u8]) -> Result<(), UsbError> {
            self.0
                .run_mobile_plugin_async::<serde_json::Value>(
                    "write",
                    serde_json::json!({ "data": B64.encode(data) }),
                )
                .await
                .map(|_| ())
                .map_err(map_err)
        }

        /// One HID report, as many bytes as arrived, or `Timeout`.
        pub async fn read(&self, timeout: Duration) -> Result<Vec<u8>, UsbError> {
            let ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
            let r: ReadReply = self
                .0
                .run_mobile_plugin_async("read", serde_json::json!({ "timeoutMs": ms }))
                .await
                .map_err(map_err)?;
            B64.decode(r.data)
                .map_err(|e| UsbError::Other(format!("bad data from the plugin: {e}")))
        }

        /// Release the interface and close Android's connection. A read in
        /// progress fails at once.
        pub async fn close(&self) {
            let _ = self
                .0
                .run_mobile_plugin_async::<serde_json::Value>("close", serde_json::json!({}))
                .await;
        }
    }
}

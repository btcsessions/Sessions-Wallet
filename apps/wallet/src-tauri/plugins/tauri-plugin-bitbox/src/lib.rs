//! USB access to a BitBox02 on Android.
//!
//! Android apps can't open USB devices on their own: the user grants access
//! per device through `UsbManager`. The Kotlin side (`android/`) finds the
//! BitBox02, asks for permission, opens it and claims its HID interface, then
//! hands back the usbfs file descriptor and endpoint addresses. The wallet
//! talks to the device over that descriptor with `nusb`.
//!
//! On other platforms the plugin does nothing.

use tauri::plugin::{Builder, TauriPlugin};
use tauri::Runtime;

#[cfg(target_os = "android")]
pub use android::{BitboxUsb, OpenedDevice, UsbError};

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
    use std::os::fd::{BorrowedFd, OwnedFd};

    use serde::Deserialize;
    use tauri::plugin::mobile::PluginInvokeError;
    use tauri::plugin::PluginHandle;
    use tauri::Runtime;

    /// Managed state: `app.state::<BitboxUsb<R>>()`.
    pub struct BitboxUsb<R: Runtime>(pub(crate) PluginHandle<R>);

    /// An open BitBox02 with its HID interface claimed.
    #[derive(Debug)]
    pub struct OpenedDevice {
        /// Our own handle on the usbfs file descriptor.
        pub fd: OwnedFd,
        pub interface: u8,
        /// Interrupt IN endpoint address (e.g. 0x81).
        pub endpoint_in: u8,
        /// Interrupt OUT endpoint address (e.g. 0x01).
        pub endpoint_out: u8,
    }

    #[derive(Debug)]
    pub enum UsbError {
        /// No BitBox02 is plugged in.
        NotFound,
        /// The user declined Android's USB access prompt.
        PermissionDenied,
        Other(String),
    }

    impl std::fmt::Display for UsbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                UsbError::NotFound => f.write_str("no BitBox02 found"),
                UsbError::PermissionDenied => f.write_str("USB access was not allowed"),
                UsbError::Other(e) => f.write_str(e),
            }
        }
    }

    impl std::error::Error for UsbError {}

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Opened {
        fd: i32,
        interface: u8,
        endpoint_in: u8,
        endpoint_out: u8,
    }

    fn map_err(e: PluginInvokeError) -> UsbError {
        match e {
            PluginInvokeError::InvokeRejected(r) => match r.code.as_deref() {
                Some("NOT_FOUND") => UsbError::NotFound,
                Some("PERMISSION_DENIED") => UsbError::PermissionDenied,
                _ => UsbError::Other(r.message.unwrap_or_else(|| "USB error".into())),
            },
            other => UsbError::Other(other.to_string()),
        }
    }

    impl<R: Runtime> BitboxUsb<R> {
        /// Find the BitBox02, ask for USB access if needed (Android shows a
        /// prompt), open it and claim its HID interface.
        pub async fn open(&self) -> Result<OpenedDevice, UsbError> {
            let o: Opened = self
                .0
                .run_mobile_plugin_async("open", serde_json::json!({}))
                .await
                .map_err(map_err)?;
            if o.fd < 0 {
                return Err(UsbError::Other("invalid USB file descriptor".into()));
            }
            // Kotlin keeps its UsbDeviceConnection (and so this fd) open
            // until `close`; we take our own duplicate so its lifetime is ours.
            // SAFETY: the fd is open and owned by the live connection.
            let fd = unsafe { BorrowedFd::borrow_raw(o.fd) }
                .try_clone_to_owned()
                .map_err(|e| UsbError::Other(e.to_string()))?;
            Ok(OpenedDevice {
                fd,
                interface: o.interface,
                endpoint_in: o.endpoint_in,
                endpoint_out: o.endpoint_out,
            })
        }

        /// Release the interface and close Android's connection.
        pub async fn close(&self) {
            let _ = self
                .0
                .run_mobile_plugin_async::<serde_json::Value>("close", serde_json::json!({}))
                .await;
        }
    }
}

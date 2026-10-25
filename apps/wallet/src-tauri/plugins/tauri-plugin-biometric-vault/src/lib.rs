//! Fingerprint / face unlock: the wallet passcode, sealed under a
//! biometric-bound key.
//!
//! On Android the Kotlin side (`android/`) keeps an AES-256-GCM key in the
//! Android Keystore that needs a strong (Class 3) biometric for every use and
//! is destroyed when the user adds a fingerprint or face. The passcode is
//! encrypted and decrypted only inside a `BiometricPrompt` bound to that key
//! (a `CryptoObject`), so reading it back takes a real biometric match, not
//! just a "yes" from the prompt. The ciphertext lives in the app's private
//! no-backup directory.
//!
//! Desktop and iOS report "not available" for now; the API is the same
//! everywhere so they can be filled in later.

use serde::{Deserialize, Serialize};
use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime};
use zeroize::Zeroizing;

/// Register with `tauri::Builder::plugin`; then `app.state::<BiometricVault<R>>()`.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("biometric-vault")
        .setup(|app, _api| {
            #[cfg(target_os = "android")]
            {
                let handle = _api
                    .register_android_plugin("io.btcsessions.biometric", "BiometricVaultPlugin")?;
                app.manage(BiometricVault::<R> { handle });
            }
            #[cfg(not(target_os = "android"))]
            app.manage(BiometricVault::<R> {
                _r: std::marker::PhantomData,
            });
            Ok(())
        })
        .build()
}

/// Managed state: the platform's biometric vault.
pub struct BiometricVault<R: Runtime> {
    #[cfg(target_os = "android")]
    handle: tauri::plugin::PluginHandle<R>,
    #[cfg(not(target_os = "android"))]
    _r: std::marker::PhantomData<fn() -> R>,
}

/// Which biometric the phone most likely uses (a guess from its sensors).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Fingerprint,
    Face,
    /// Both, neither, or unsure.
    Biometric,
}

/// What the UI needs to offer biometric unlock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// The phone has a strong biometric sensor with something enrolled.
    pub available: bool,
    /// A protected passcode is stored and its key is still valid.
    pub enabled: bool,
    /// Biometric unlock was switched off because the phone's fingerprints or
    /// face changed. Stays set until it's turned on again or off (which
    /// acknowledges it), or `unlock` reports it once.
    #[serde(default)]
    pub invalidated: bool,
    pub kind: Kind,
    /// Why it's unavailable, in plain English (`None` when available).
    #[serde(default)]
    pub reason: Option<String>,
}

/// Desktop and iOS, for now.
pub const NOT_AVAILABLE: &str = "Fingerprint and face unlock are only in the Android app for now";

impl Status {
    pub fn not_available() -> Self {
        Status {
            available: false,
            enabled: false,
            invalidated: false,
            kind: Kind::Biometric,
            reason: Some(NOT_AVAILABLE.into()),
        }
    }
}

/// Plain-English messages; [`BiometricError::code`] tells them apart.
pub const INVALIDATED: &str = "Your fingerprints or face changed. Enter your passcode, then turn biometric unlock on again in Settings.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BiometricError {
    /// No strong biometric (no sensor, none enrolled, security update
    /// needed, not on this platform). The message says which.
    NotAvailable(String),
    /// Nothing stored: biometric unlock isn't turned on.
    NotEnabled,
    /// The user backed out ("Use passcode", back, or the prompt timed out).
    Cancelled,
    /// Too many failed tries. The message says for how long.
    Lockout(String),
    /// The phone's biometrics changed: the key is gone, so is the stored
    /// passcode.
    Invalidated,
    /// A prompt is already showing.
    Busy,
    Other(String),
}

impl BiometricError {
    /// Stable code, as the Kotlin side rejects with.
    pub fn code(&self) -> &'static str {
        match self {
            BiometricError::NotAvailable(_) => "UNAVAILABLE",
            BiometricError::NotEnabled => "NOT_ENABLED",
            BiometricError::Cancelled => "CANCELLED",
            BiometricError::Lockout(_) => "LOCKOUT",
            BiometricError::Invalidated => "INVALIDATED",
            BiometricError::Busy => "BUSY",
            BiometricError::Other(_) => "OTHER",
        }
    }

    /// From the Kotlin side's rejection (code + message).
    pub fn from_code(code: Option<&str>, message: String) -> Self {
        let or = |fallback: &str| {
            if message.trim().is_empty() {
                fallback.to_string()
            } else {
                message.clone()
            }
        };
        match code {
            Some("UNAVAILABLE") => {
                BiometricError::NotAvailable(or("Biometric unlock isn't available on this phone"))
            }
            Some("NOT_ENABLED") => BiometricError::NotEnabled,
            Some("CANCELLED") => BiometricError::Cancelled,
            Some("LOCKOUT") => {
                BiometricError::Lockout(or("Too many attempts. Use your passcode for now"))
            }
            Some("INVALIDATED") => BiometricError::Invalidated,
            Some("BUSY") => BiometricError::Busy,
            _ => BiometricError::Other(or("unknown error")),
        }
    }
}

impl std::fmt::Display for BiometricError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BiometricError::NotAvailable(m) | BiometricError::Lockout(m) => f.write_str(m),
            BiometricError::NotEnabled => {
                f.write_str("Biometric unlock isn't turned on. Enter your passcode")
            }
            BiometricError::Cancelled => f.write_str("Biometric unlock cancelled"),
            BiometricError::Invalidated => f.write_str(INVALIDATED),
            BiometricError::Busy => f.write_str("Already waiting for your fingerprint or face"),
            BiometricError::Other(m) => write!(f, "Biometric unlock failed: {m}"),
        }
    }
}

impl std::error::Error for BiometricError {}

/// The Kotlin side's reply to `unlock`.
#[derive(Deserialize)]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
struct Unlocked {
    passcode: String,
}

#[cfg(target_os = "android")]
mod android {
    use super::*;
    use tauri::plugin::mobile::PluginInvokeError;

    pub(super) fn map_err(e: PluginInvokeError) -> BiometricError {
        match e {
            PluginInvokeError::InvokeRejected(r) => {
                BiometricError::from_code(r.code.as_deref(), r.message.unwrap_or_default())
            }
            other => BiometricError::Other(other.to_string()),
        }
    }
}

#[cfg(target_os = "android")]
impl<R: Runtime> BiometricVault<R> {
    /// Sensor, enrolment and stored-secret state. Never shows a prompt.
    pub async fn status(&self) -> Status {
        match self
            .handle
            .run_mobile_plugin_async::<Status>("status", serde_json::json!({}))
            .await
        {
            Ok(s) => s,
            Err(e) => Status {
                reason: Some(android::map_err(e).to_string()),
                ..Status::not_available()
            },
        }
    }

    /// Show the "Turn on … unlock" prompt and seal `passcode` under the
    /// biometric-bound key (a new key every time). Check the passcode first.
    pub async fn enable(&self, passcode: &str) -> Result<(), BiometricError> {
        // The JSON bridge copies the passcode (serde_json, then a Kotlin
        // String); neither can be wiped. Kotlin overwrites its byte copy.
        self.handle
            .run_mobile_plugin_async::<serde_json::Value>(
                "enable",
                serde_json::json!({ "passcode": passcode }),
            )
            .await
            .map_err(android::map_err)?;
        Ok(())
    }

    /// Delete the key and the stored passcode (and acknowledge `invalidated`).
    pub async fn disable(&self) -> Result<(), BiometricError> {
        self.handle
            .run_mobile_plugin_async::<serde_json::Value>("disable", serde_json::json!({}))
            .await
            .map_err(android::map_err)?;
        Ok(())
    }

    /// Show the "Unlock Sessions Wallet" prompt and decrypt the passcode.
    pub async fn unlock(&self) -> Result<Zeroizing<String>, BiometricError> {
        let reply: Unlocked = self
            .handle
            .run_mobile_plugin_async("unlock", serde_json::json!({}))
            .await
            .map_err(android::map_err)?;
        Ok(Zeroizing::new(reply.passcode))
    }
}

/// Desktop and iOS: not yet.
#[cfg(not(target_os = "android"))]
impl<R: Runtime> BiometricVault<R> {
    pub async fn status(&self) -> Status {
        Status::not_available()
    }

    pub async fn enable(&self, _passcode: &str) -> Result<(), BiometricError> {
        Err(BiometricError::NotAvailable(NOT_AVAILABLE.into()))
    }

    /// Nothing is ever stored here, so there's nothing to delete.
    pub async fn disable(&self) -> Result<(), BiometricError> {
        Ok(())
    }

    pub async fn unlock(&self) -> Result<Zeroizing<String>, BiometricError> {
        Err(BiometricError::NotAvailable(NOT_AVAILABLE.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_kotlin_codes() {
        let e = |c: &str, m: &str| BiometricError::from_code(Some(c), m.into());
        assert_eq!(e("CANCELLED", "x"), BiometricError::Cancelled);
        assert_eq!(e("INVALIDATED", ""), BiometricError::Invalidated);
        assert_eq!(e("NOT_ENABLED", ""), BiometricError::NotEnabled);
        assert_eq!(e("BUSY", ""), BiometricError::Busy);
        assert_eq!(
            e("LOCKOUT", "Try again in 30 seconds"),
            BiometricError::Lockout("Try again in 30 seconds".into())
        );
        assert!(matches!(e("LOCKOUT", " "), BiometricError::Lockout(m) if !m.trim().is_empty()));
        assert_eq!(
            e("UNAVAILABLE", "No sensor"),
            BiometricError::NotAvailable("No sensor".into())
        );
        assert!(matches!(
            BiometricError::from_code(None, "".into()),
            BiometricError::Other(m) if m == "unknown error"
        ));
        assert!(matches!(e("WHAT", "boom"), BiometricError::Other(m) if m == "boom"));
        // Codes round-trip.
        for err in [
            BiometricError::NotAvailable("a".into()),
            BiometricError::NotEnabled,
            BiometricError::Cancelled,
            BiometricError::Lockout("a".into()),
            BiometricError::Invalidated,
            BiometricError::Busy,
            BiometricError::Other("a".into()),
        ] {
            assert_eq!(BiometricError::from_code(Some(err.code()), "a".into()), err);
        }
        assert_eq!(BiometricError::Invalidated.to_string(), INVALIDATED);
    }

    #[test]
    fn status_json() {
        let s: Status = serde_json::from_value(serde_json::json!({
            "available": true, "enabled": false, "kind": "face"
        }))
        .unwrap();
        assert_eq!(s.kind, Kind::Face);
        assert!(!s.invalidated);
        assert_eq!(s.reason, None);
        let v = serde_json::to_value(Status::not_available()).unwrap();
        assert_eq!(v["kind"], "biometric");
        assert_eq!(v["available"], false);
        assert_eq!(v["invalidated"], false);
        assert_eq!(v["reason"], NOT_AVAILABLE);
    }
}

//! Passkeys with the WebAuthn PRF extension, for passkey wallets.
//!
//! On Android the Kotlin side (`android/`) uses Credential Manager
//! (androidx.credentials): `create` registers a discoverable passkey for the
//! wallet's relying party and asks it to evaluate PRF with the app's salt,
//! `get` signs in with one (the user picks it) and returns its PRF output.
//! The PRF output is a 32-byte secret only that passkey can reproduce: the
//! wallet derives its keys from it.
//!
//! Desktop and iOS answer [`PasskeyError::NotSupported`] for now; the API is
//! the same everywhere so they can be filled in later.

use serde::Deserialize;
use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime};
use zeroize::Zeroizing;

/// Register with `tauri::Builder::plugin`; then `app.state::<Passkeys<R>>()`.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("passkey")
        .setup(|app, _api| {
            #[cfg(target_os = "android")]
            {
                let handle =
                    _api.register_android_plugin("io.btcsessions.passkey", "PasskeyPlugin")?;
                app.manage(Passkeys::<R> { handle });
            }
            #[cfg(not(target_os = "android"))]
            app.manage(Passkeys::<R> {
                _r: std::marker::PhantomData,
            });
            Ok(())
        })
        .build()
}

/// Managed state: the platform's passkey provider.
pub struct Passkeys<R: Runtime> {
    #[cfg(target_os = "android")]
    handle: tauri::plugin::PluginHandle<R>,
    #[cfg(not(target_os = "android"))]
    _r: std::marker::PhantomData<fn() -> R>,
}

/// Registering a new passkey.
#[derive(Debug, Clone)]
pub struct CreateRequest {
    /// Relying party: the domain whose `/.well-known/assetlinks.json` lists the app.
    pub rp_id: String,
    pub rp_name: String,
    /// WebAuthn user handle (random, base64url).
    pub user_id: String,
    /// Shown in the passkey manager's list.
    pub user_name: String,
    /// PRF salt (`eval.first`), raw bytes.
    pub prf_salt: Vec<u8>,
}

/// Signing in with an existing passkey.
#[derive(Debug, Clone)]
pub struct GetRequest {
    pub rp_id: String,
    pub prf_salt: Vec<u8>,
    /// Only these credentials (base64url ids); empty lets the user pick any.
    pub allow_credentials: Vec<String>,
}

/// A passkey and its PRF output for the salt asked.
pub struct PrfResult {
    /// Credential id, base64url.
    pub credential_id: String,
    /// PRF output (32 bytes).
    pub prf: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for PrfResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrfResult")
            .field("credential_id", &self.credential_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasskeyError {
    /// No passkeys on this platform (desktop, iOS for now, old Android).
    NotSupported,
    /// The passkey manager made the passkey but doesn't do PRF.
    PrfUnsupported,
    /// The user backed out.
    Cancelled,
    /// No passkey for this app on the phone / in the account.
    NoCredential,
    /// The domain doesn't vouch for this app (assetlinks.json missing or
    /// listing another signing key).
    NotTrusted(String),
    Other(String),
}

impl std::fmt::Display for PasskeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PasskeyError::NotSupported => {
                f.write_str("Passkeys aren't available on this device. Use a recovery phrase")
            }
            PasskeyError::PrfUnsupported => f.write_str(
                "Your passkey manager doesn't support passkey wallets yet; use a recovery phrase",
            ),
            PasskeyError::Cancelled => f.write_str("Passkey cancelled"),
            PasskeyError::NoCredential => {
                f.write_str("No Sessions Wallet passkey found on this phone or in your account")
            }
            PasskeyError::NotTrusted(m) => {
                write!(
                    f,
                    "This app isn't set up for passkeys on its domain yet ({m})"
                )
            }
            PasskeyError::Other(m) => write!(f, "Passkey error: {m}"),
        }
    }
}

impl std::error::Error for PasskeyError {}

/// The Kotlin side's reply to `create` and `get`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
struct Reply {
    credential_id: String,
    /// base64url
    prf: String,
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn into_result(r: Reply) -> Result<PrfResult, PasskeyError> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let prf = URL_SAFE_NO_PAD
        .decode(r.prf.trim_end_matches('='))
        .map_err(|e| PasskeyError::Other(format!("bad PRF output: {e}")))?;
    if prf.len() != 32 {
        return Err(PasskeyError::PrfUnsupported);
    }
    Ok(PrfResult {
        credential_id: r.credential_id,
        prf: Zeroizing::new(prf),
    })
}

/// base64url without padding, as WebAuthn JSON wants.
pub fn b64url(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(target_os = "android")]
mod android {
    use super::*;
    use tauri::plugin::mobile::PluginInvokeError;

    pub(super) fn map_err(e: PluginInvokeError) -> PasskeyError {
        match e {
            PluginInvokeError::InvokeRejected(r) => {
                let msg = r.message.unwrap_or_default();
                match r.code.as_deref() {
                    Some("NOT_SUPPORTED") => PasskeyError::NotSupported,
                    Some("PRF_UNSUPPORTED") => PasskeyError::PrfUnsupported,
                    Some("CANCELLED") => PasskeyError::Cancelled,
                    Some("NO_CREDENTIAL") => PasskeyError::NoCredential,
                    Some("NOT_TRUSTED") => PasskeyError::NotTrusted(msg),
                    _ => PasskeyError::Other(if msg.is_empty() {
                        "unknown".into()
                    } else {
                        msg
                    }),
                }
            }
            other => PasskeyError::Other(other.to_string()),
        }
    }

    #[derive(Deserialize)]
    pub(super) struct Supported {
        pub supported: bool,
    }
}

#[cfg(target_os = "android")]
impl<R: Runtime> Passkeys<R> {
    /// Whether this device can make passkeys at all (PRF support is only
    /// known once one is made).
    pub async fn is_supported(&self) -> bool {
        self.handle
            .run_mobile_plugin_async::<android::Supported>("isSupported", serde_json::json!({}))
            .await
            .map(|s| s.supported)
            .unwrap_or(false)
    }

    /// Make a passkey and get its PRF output (a second prompt if the
    /// provider doesn't evaluate PRF while creating).
    pub async fn create(&self, req: CreateRequest) -> Result<PrfResult, PasskeyError> {
        let reply: Reply = self
            .handle
            .run_mobile_plugin_async(
                "create",
                serde_json::json!({
                    "rpId": req.rp_id,
                    "rpName": req.rp_name,
                    "userId": req.user_id,
                    "userName": req.user_name,
                    "prfSalt": b64url(&req.prf_salt),
                }),
            )
            .await
            .map_err(android::map_err)?;
        into_result(reply)
    }

    /// Sign in with a passkey (the user picks one) and get its PRF output.
    pub async fn get(&self, req: GetRequest) -> Result<PrfResult, PasskeyError> {
        let reply: Reply = self
            .handle
            .run_mobile_plugin_async(
                "get",
                serde_json::json!({
                    "rpId": req.rp_id,
                    "prfSalt": b64url(&req.prf_salt),
                    "allowCredentials": req.allow_credentials,
                }),
            )
            .await
            .map_err(android::map_err)?;
        into_result(reply)
    }
}

/// Desktop and iOS: not yet. Desktop users use a recovery phrase.
#[cfg(not(target_os = "android"))]
impl<R: Runtime> Passkeys<R> {
    pub async fn is_supported(&self) -> bool {
        false
    }

    pub async fn create(&self, _req: CreateRequest) -> Result<PrfResult, PasskeyError> {
        Err(PasskeyError::NotSupported)
    }

    pub async fn get(&self, _req: GetRequest) -> Result<PrfResult, PasskeyError> {
        Err(PasskeyError::NotSupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_replies() {
        let ok = into_result(Reply {
            credential_id: "abc".into(),
            prf: b64url(&[5u8; 32]),
        })
        .unwrap();
        assert_eq!(&ok.prf[..], &[5u8; 32]);
        // Some providers pad.
        let padded = format!("{}=", b64url(&[5u8; 32]));
        assert!(into_result(Reply {
            credential_id: "abc".into(),
            prf: padded
        })
        .is_ok());
        assert_eq!(
            into_result(Reply {
                credential_id: "abc".into(),
                prf: b64url(&[5u8; 16]),
            })
            .unwrap_err(),
            PasskeyError::PrfUnsupported
        );
        assert!(!format!("{ok:?}").contains("prf: "));
    }
}

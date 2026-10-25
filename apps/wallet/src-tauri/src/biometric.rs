//! Fingerprint / face unlock (Android).
//!
//! The wallet passcode is sealed under an Android Keystore key that needs a
//! strong biometric for every use (see `tauri-plugin-biometric-vault`).
//! Unlocking decrypts it inside the biometric prompt and goes through the
//! same path as typing it (`commands::unlock_passcode`), so the vault, the
//! rate limit and everything after are unchanged. Desktop and iOS report
//! "not available".
//!
//! Errors are `{ code, message }` (see [`BiometricFailure`]): the message is
//! for the user, the code tells the UI what happened.

use serde::Serialize;
use sessions_core::Error;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_biometric_vault::{BiometricError, BiometricVault, Status, NOT_AVAILABLE};
use zeroize::Zeroizing;

use crate::state::AppState;

/// What went wrong, for the UI to branch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BiometricCode {
    /// No strong biometric here (no sensor, none enrolled, weak face unlock
    /// only, security update needed, desktop/iOS).
    Unavailable,
    /// Biometric unlock isn't turned on.
    NotEnabled,
    /// "Use passcode" / "Cancel" / back / timed out / app went to the background.
    Cancelled,
    /// Too many failed fingerprint or face tries.
    Lockout,
    /// Biometrics changed on the phone: everything was deleted.
    Invalidated,
    /// The stored passcode no longer opens the wallet: everything was deleted.
    Stale,
    /// `enable_biometric`: the passcode typed is wrong.
    WrongPasscode,
    /// Too many wrong passcodes lately (the vault's own rate limit).
    TooManyAttempts,
    /// A prompt is already showing.
    Busy,
    Failed,
}

/// A biometric command's error: `{ "code": "cancelled", "message": "…" }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BiometricFailure {
    pub code: BiometricCode,
    pub message: String,
}

pub const STALE: &str = "Your passcode no longer matches the one saved for biometric unlock. Enter your passcode, then turn biometric unlock on again in Settings.";

fn failure(code: BiometricCode, message: impl Into<String>) -> BiometricFailure {
    BiometricFailure {
        code,
        message: message.into(),
    }
}

fn from_plugin(e: BiometricError) -> BiometricFailure {
    let code = match &e {
        BiometricError::NotAvailable(_) => BiometricCode::Unavailable,
        BiometricError::NotEnabled => BiometricCode::NotEnabled,
        BiometricError::Cancelled => BiometricCode::Cancelled,
        BiometricError::Lockout(_) => BiometricCode::Lockout,
        BiometricError::Invalidated => BiometricCode::Invalidated,
        BiometricError::Busy => BiometricCode::Busy,
        BiometricError::Other(_) => BiometricCode::Failed,
    };
    failure(code, e.to_string())
}

/// Checking the passcode before turning biometric unlock on.
fn from_verify_error(e: &Error) -> BiometricFailure {
    let code = match e {
        Error::WrongPasscode => BiometricCode::WrongPasscode,
        Error::TooManyAttempts { .. } => BiometricCode::TooManyAttempts,
        _ => BiometricCode::Failed,
    };
    failure(code, e.to_string())
}

/// Unlocking with the decrypted passcode. `true`: forget the stored one.
fn from_unlock_error(e: &Error) -> (BiometricFailure, bool) {
    match e {
        // It decrypted fine, so it's what was stored, and the vault says no.
        Error::WrongPasscode => (failure(BiometricCode::Stale, STALE), true),
        Error::TooManyAttempts { .. } => (
            failure(BiometricCode::TooManyAttempts, e.to_string()),
            false,
        ),
        _ => (failure(BiometricCode::Failed, e.to_string()), false),
    }
}

fn not_available() -> BiometricFailure {
    failure(BiometricCode::Unavailable, NOT_AVAILABLE)
}

fn vault(app: &AppHandle) -> Option<State<'_, BiometricVault<tauri::Wry>>> {
    app.try_state::<BiometricVault<tauri::Wry>>()
}

/// Delete the stored passcode and its key, if any (a new wallet was made).
pub(crate) async fn forget(app: &AppHandle) {
    if let Some(v) = vault(app) {
        if let Err(e) = v.disable().await {
            tracing::warn!("couldn't clear biometric unlock: {e}");
        }
    }
}

async fn current_status(app: &AppHandle, state: &AppState) -> Status {
    let Some(v) = vault(app) else {
        return Status::not_available();
    };
    let s = v.status().await;
    if (s.enabled || s.invalidated) && !state.has_vault() {
        // The wallet is gone; so is what unlocked it.
        let _ = v.disable().await;
        return v.status().await;
    }
    s
}

/// Open the vault to check the passcode, without unlocking (no state change
/// beyond the wrong-passcode rate limit).
async fn verify_passcode(state: &AppState, passcode: &str) -> Result<(), BiometricFailure> {
    state
        .open_vault(passcode)
        .await
        .map(drop)
        .map_err(|e| from_verify_error(&e))
}

#[tauri::command]
pub async fn biometric_status(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Status, BiometricFailure> {
    Ok(current_status(&app, &state).await)
}

/// Check the passcode, then seal it behind a fingerprint / face prompt.
#[tauri::command]
pub async fn enable_biometric(
    app: AppHandle,
    state: State<'_, AppState>,
    passcode: String,
) -> Result<Status, BiometricFailure> {
    let passcode = Zeroizing::new(passcode);
    let v = vault(&app).ok_or_else(not_available)?;
    if !state.has_vault() {
        return Err(failure(
            BiometricCode::Failed,
            "There's no wallet on this device yet",
        ));
    }
    let s = v.status().await;
    if !s.available {
        return Err(failure(
            BiometricCode::Unavailable,
            s.reason
                .unwrap_or_else(|| "Biometric unlock isn't available on this phone".into()),
        ));
    }
    verify_passcode(&state, &passcode).await?;
    v.enable(&passcode).await.map_err(from_plugin)?;
    Ok(current_status(&app, &state).await)
}

#[tauri::command]
pub async fn disable_biometric(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Status, BiometricFailure> {
    if let Some(v) = vault(&app) {
        v.disable().await.map_err(from_plugin)?;
    }
    Ok(current_status(&app, &state).await)
}

/// Fingerprint / face prompt, then the same unlock as typing the passcode.
#[tauri::command]
pub async fn unlock_biometric(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), BiometricFailure> {
    let v = vault(&app).ok_or_else(not_available)?;
    if !state.has_vault() {
        let _ = v.disable().await;
        return Err(from_plugin(BiometricError::NotEnabled));
    }
    let passcode = v.unlock().await.map_err(from_plugin)?;
    match crate::commands::unlock_passcode(&state, &passcode).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let (f, forget) = from_unlock_error(&e);
            if forget {
                let _ = v.disable().await;
            }
            Err(f)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessions_core::keystore::Seed;

    #[test]
    fn plugin_errors_keep_their_meaning() {
        let cases = [
            (
                BiometricError::NotAvailable("No sensor".into()),
                BiometricCode::Unavailable,
            ),
            (BiometricError::NotEnabled, BiometricCode::NotEnabled),
            (BiometricError::Cancelled, BiometricCode::Cancelled),
            (
                BiometricError::Lockout("Too many attempts".into()),
                BiometricCode::Lockout,
            ),
            (BiometricError::Invalidated, BiometricCode::Invalidated),
            (BiometricError::Busy, BiometricCode::Busy),
            (BiometricError::Other("boom".into()), BiometricCode::Failed),
        ];
        for (e, code) in cases {
            let msg = e.to_string();
            let f = from_plugin(e);
            assert_eq!(f.code, code);
            assert_eq!(f.message, msg);
            assert!(!f.message.is_empty());
        }
        assert_eq!(
            from_plugin(BiometricError::Invalidated).message,
            "Your fingerprints or face changed. Enter your passcode, then turn biometric unlock on again in Settings."
        );
    }

    #[test]
    fn serialises_code_and_message() {
        let v = serde_json::to_value(failure(BiometricCode::NotEnabled, "x")).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "code": "not_enabled", "message": "x" })
        );
        let v = serde_json::to_value(from_unlock_error(&Error::WrongPasscode).0).unwrap();
        assert_eq!(v["code"], "stale");
        assert_eq!(
            serde_json::to_value(BiometricCode::TooManyAttempts).unwrap(),
            "too_many_attempts"
        );
    }

    #[test]
    fn unlock_errors() {
        let (f, forget) = from_unlock_error(&Error::WrongPasscode);
        assert_eq!((f.code, forget), (BiometricCode::Stale, true));
        assert_eq!(f.message, STALE);
        // Rate-limited: the passcode may be right; keep it.
        let (f, forget) = from_unlock_error(&Error::TooManyAttempts { wait_secs: 4 });
        assert_eq!((f.code, forget), (BiometricCode::TooManyAttempts, false));
        assert!(f.message.contains("4 seconds"));
        let (f, forget) = from_unlock_error(&Error::Storage("disk".into()));
        assert_eq!((f.code, forget), (BiometricCode::Failed, false));
    }

    #[test]
    fn verify_errors() {
        assert_eq!(
            from_verify_error(&Error::WrongPasscode).code,
            BiometricCode::WrongPasscode
        );
        assert_eq!(
            from_verify_error(&Error::TooManyAttempts { wait_secs: 2 }).code,
            BiometricCode::TooManyAttempts
        );
        assert_eq!(
            from_verify_error(&Error::Locked).code,
            BiometricCode::Failed
        );
    }

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[tokio::test]
    async fn verify_checks_without_unlocking_and_stale_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(dir.path().to_path_buf(), None);
        state
            .create(Seed::from_phrase(ABANDON).unwrap(), "123456")
            .await
            .unwrap();
        state.lock().await;

        // Right passcode: verified, still locked.
        verify_passcode(&state, "123456").await.unwrap();
        assert!(state.inner.lock().await.is_none());
        // Wrong one: told apart, still locked.
        let f = verify_passcode(&state, "000000").await.unwrap_err();
        assert_eq!(f.code, BiometricCode::WrongPasscode);
        assert!(state.inner.lock().await.is_none());

        // A stored passcode that no longer opens the vault reads as stale.
        let e = crate::commands::unlock_passcode(&state, "999999")
            .await
            .unwrap_err();
        assert_eq!(from_unlock_error(&e).0.code, BiometricCode::Stale);
        // The right one unlocks through the same path as `unlock`.
        crate::commands::unlock_passcode(&state, "123456")
            .await
            .unwrap();
        assert!(state.inner.lock().await.is_some());
    }
}

//! Passkey wallets and passkey backup of a recovery phrase.
//!
//! * Basic (default) wallet: the seed is derived from the passkey's PRF
//!   output, so signing in with the passkey on a new phone recreates it.
//! * A phrase wallet can also be backed up with a passkey: the phrase is
//!   sealed under a PRF-derived key and published to relays (the envelope).
//! * Restoring with a passkey: an envelope found → that phrase; none (and
//!   enough relays answered) → the passkey-derived seed.
//!
//! The passcode still unlocks the wallet day to day; the passkey is only for
//! backup and restore. See `sessions_backup::keys` for the derivations.

use sessions_backup::keys::{prf_salt, PasskeyKeys};
use sessions_backup::{envelope, KIND_APP_DATA};
use sessions_core::keystore::Seed;
use sessions_core::{Error, Result};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_passkey::{b64url, CreateRequest, GetRequest, PasskeyError, Passkeys, PrfResult};

use crate::backup::{self, now_secs, BackupStatus, RestoreResult, MIN_ANSWERS};
use crate::commands::check_passcode;
use crate::config::{BackupFlags, SeedOrigin};
use crate::state::AppState;

/// The relying party passkeys are made for. The site must serve
/// `/.well-known/assetlinks.json` (see `passkey/assetlinks.json`) naming the
/// app and its signing key, or Android refuses. Changing it orphans every
/// passkey made before: never change it after release.
pub const PASSKEY_RP_ID: &str = "wallet.btcsessions.com";
const PASSKEY_RP_NAME: &str = "Sessions Wallet";

fn err(e: PasskeyError) -> Error {
    Error::Invalid(e.to_string())
}

fn passkeys(app: &AppHandle) -> Result<State<'_, Passkeys<tauri::Wry>>> {
    app.try_state::<Passkeys<tauri::Wry>>()
        .ok_or_else(|| Error::Invalid(PasskeyError::NotSupported.to_string()))
}

/// "Sessions Wallet 2026-10-22": tells several passkeys apart in the list.
fn user_name() -> String {
    let days = (now_secs() / 86_400) as i64;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{PASSKEY_RP_NAME} {y:04}-{m:02}-{d:02}")
}

async fn create_passkey(app: &AppHandle) -> Result<PrfResult> {
    use rand::RngCore;
    let mut user_id = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut user_id);
    passkeys(app)?
        .create(CreateRequest {
            rp_id: PASSKEY_RP_ID.into(),
            rp_name: PASSKEY_RP_NAME.into(),
            user_id: b64url(&user_id),
            user_name: user_name(),
            prf_salt: prf_salt().to_vec(),
        })
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn passkey_supported(app: AppHandle) -> Result<bool> {
    Ok(match app.try_state::<Passkeys<tauri::Wry>>() {
        Some(p) => p.is_supported().await,
        None => false,
    })
}

/// Basic setup: a new passkey, and a wallet whose seed comes from it.
#[tauri::command]
pub async fn create_passkey_wallet(
    app: AppHandle,
    state: State<'_, AppState>,
    passcode: String,
) -> Result<()> {
    check_passcode(&passcode)?;
    if state.has_vault() {
        return Err(Error::Invalid(
            "A wallet already exists on this device".into(),
        ));
    }
    let made = create_passkey(&app).await?;
    let keys = PasskeyKeys::from_prf(&made.prf)?;
    let seed = Seed::from_entropy(&keys.seed_entropy[..])?;
    state
        .create_with(
            seed,
            &passcode,
            BackupFlags {
                passkey: true,
                phrase: false,
                seed_origin: SeedOrigin::Passkey,
                credential_id: Some(made.credential_id),
            },
            false,
        )
        .await?;
    // Unlocking a wallet that was never backed up schedules its first backup
    // at once, in the background; `backup://status` reports how it went.
    Ok(())
}

/// Sign in with a passkey and bring its wallet back, with its state.
#[tauri::command]
pub async fn restore_with_passkey(
    app: AppHandle,
    state: State<'_, AppState>,
    passcode: String,
) -> Result<RestoreResult> {
    check_passcode(&passcode)?;
    if state.has_vault() {
        return Err(Error::Invalid(
            "A wallet already exists on this device".into(),
        ));
    }
    let got = passkeys(&app)?
        .get(GetRequest {
            rp_id: PASSKEY_RP_ID.into(),
            prf_salt: prf_salt().to_vec(),
            allow_credentials: vec![],
        })
        .await
        .map_err(err)?;
    let keys = PasskeyKeys::from_prf(&got.prf)?;

    // Is this passkey backing up a phrase (an envelope), or is it the seed?
    let urls = sessions_backup::default_relays();
    let answers = state
        .backup
        .relays()
        .fetch(&urls, &keys.nostr.public_hex(), KIND_APP_DATA)
        .await;
    let reached = answers.iter().filter(|(_, r)| r.is_ok()).count();
    let events: Vec<_> = answers
        .into_iter()
        .filter_map(|(_, r)| r.ok())
        .flatten()
        .collect();
    let (seed, origin, envelope_event) = match envelope::open(&keys, &events) {
        Some(phrase) => {
            let event = events
                .iter()
                .filter(|e| e.d() == Some(sessions_backup::ENVELOPE_D))
                .max_by_key(|e| e.created_at)
                .cloned();
            (Seed::from_phrase(&phrase)?, SeedOrigin::Phrase, event)
        }
        None if reached >= MIN_ANSWERS => (
            Seed::from_entropy(&keys.seed_entropy[..])?,
            SeedOrigin::Passkey,
            None,
        ),
        None => {
            return Err(Error::Invalid(
                "Couldn't reach the backup servers to look up your wallet. Check your connection and try again"
                    .into(),
            ))
        }
    };
    state
        .create_with(
            seed,
            &passcode,
            BackupFlags {
                passkey: true,
                phrase: false,
                seed_origin: origin,
                credential_id: Some(got.credential_id),
            },
            true,
        )
        .await?;
    if let Some(e) = envelope_event {
        state.backup.set_envelope(e);
    }
    Ok(backup::restore_state(&state).await)
}

/// Back up this wallet's recovery phrase with a new passkey (a phrase
/// wallet, Advanced "both", or turned on later in Settings).
#[tauri::command]
pub async fn enable_passkey_backup(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<BackupStatus> {
    let (phrase, origin) = {
        let guard = state.inner.lock().await;
        let u = guard.as_ref().ok_or(Error::Locked)?;
        (u.seed.phrase(), u.config.backup.seed_origin)
    };
    if origin == SeedOrigin::Passkey {
        // The passkey already is the backup.
        return Ok(state.backup.status());
    }
    let made = create_passkey(&app).await?;
    let keys = PasskeyKeys::from_prf(&made.prf)?;
    let event = envelope::seal(&keys, &phrase, now_secs())?;
    drop(phrase);
    // Only call it backed up once a relay has it.
    let urls = {
        let guard = state.inner.lock().await;
        guard
            .as_ref()
            .ok_or(Error::Locked)?
            .config
            .backup_relays
            .clone()
    };
    let results = state
        .backup
        .relays()
        .publish(&urls, std::slice::from_ref(&event))
        .await;
    if !results.iter().any(|(_, r)| r.is_ok()) {
        return Err(Error::Invalid(
            "Couldn't reach the backup servers. Check your connection and try again".into(),
        ));
    }
    state.backup.set_envelope(event);
    {
        let mut guard = state.inner.lock().await;
        let u = guard.as_mut().ok_or(Error::Locked)?;
        u.config.backup.passkey = true;
        u.config.backup.credential_id = Some(made.credential_id);
        state.save_config(&u.config)?;
    }
    Ok(state.backup.status())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_name_has_todays_date() {
        let n = user_name();
        assert!(n.starts_with("Sessions Wallet 20"), "{n}");
        assert_eq!(n.len(), "Sessions Wallet 2026-10-22".len());
    }
}

//! Multisig vaults on a Trezor, through Trezor Connect in the webview.
//!
//! Trezor firmware doesn't take PSBTs, so the frontend (`hw/trezor.ts`) asks
//! for the Connect parameters here, has the Trezor sign or show an address,
//! and hands the result back:
//!
//! 1. `trezor_multisig_sign_request(account, psbt, fingerprint)` →
//!    `signTransaction` params for the Trezor with that root fingerprint;
//! 2. `trezor_multisig_merge(psbt, signatures, signedTx)` → the PSBT with the
//!    Trezor's signature on each input it signed (checked against the
//!    vault's keys), not finalized: other cosigners may still need to sign;
//! 3. `trezor_multisig_address_request(account, index, fingerprint)` →
//!    `getAddress` params and the address the Trezor must show.
//!
//! The BitBox02 and Jade vault commands live in `bitbox.rs` and `jade.rs`.

use rail_onchain::trezor;
use serde::Serialize;
use serde_json::Value;
use sessions_core::{AccountId, Result};
use tauri::State;

use crate::bitbox::vault;
use crate::state::AppState;

/// Trezor Connect `signTransaction` parameters for a PSBT spending from
/// multisig vault `account`, for the Trezor whose root fingerprint is
/// `fingerprint` (8 hex characters).
#[tauri::command]
pub async fn trezor_multisig_sign_request(
    state: State<'_, AppState>,
    account: AccountId,
    psbt: String,
    fingerprint: String,
) -> Result<Value> {
    let vault = vault(&state, &account).await?;
    trezor::sign_request_multisig(&psbt, &vault.config, vault.network, &fingerprint)
}

/// Add the Trezor's signatures (Connect's `signatures`, one per input, null
/// or empty where it didn't sign) to the PSBT. `signed_tx` (Connect's
/// `serializedTx`), if given, must be the same transaction. Returns the PSBT
/// (base64), not finalized.
#[tauri::command]
pub async fn trezor_multisig_merge(
    psbt: String,
    signatures: Vec<Option<String>>,
    signed_tx: Option<String>,
) -> Result<String> {
    trezor::merge_multisig_signatures(&psbt, &signatures, signed_tx.as_deref())
}

/// What the Trezor needs to show a vault address, and what it must show.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrezorAddressRequest {
    /// Trezor Connect `getAddress` parameters.
    pub params: Value,
    /// The address this wallet derives.
    pub address: String,
}

/// Trezor Connect `getAddress` parameters to show receive address `index` of
/// multisig vault `account` on the Trezor with root fingerprint
/// `fingerprint`, and the address it must show.
#[tauri::command]
pub async fn trezor_multisig_address_request(
    state: State<'_, AppState>,
    account: AccountId,
    index: u32,
    fingerprint: String,
) -> Result<TrezorAddressRequest> {
    let vault = vault(&state, &account).await?;
    let (params, address) =
        trezor::multisig_address_request(&vault.config, vault.network, &fingerprint, index)?;
    Ok(TrezorAddressRequest { params, address })
}

// --- single-sig hardware accounts --------------------------------------------

/// The external descriptor pair and network of single-sig hardware account `account`.
async fn hardware(state: &AppState, account: &AccountId) -> Result<(rail_onchain::descriptors::DescriptorPair, sessions_core::Network)> {
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(sessions_core::Error::Locked)?;
    match u.config.accounts.iter().find(|a| a.id() == account) {
        Some(crate::config::AccountSpec::Hardware { descriptors, .. }) => Ok((descriptors.clone(), u.config.network)),
        Some(_) => Err(sessions_core::Error::Unsupported("check addresses on a device for this account".into())),
        None => Err(sessions_core::Error::AccountNotFound(account.0.clone())),
    }
}

/// How far to look for an address (receive addresses handed out so far).
const MAX_ADDRESS_INDEX: u32 = 10_000;

/// Which receive index `address` is in hardware account `account`, so the
/// device can be asked to show that exact address.
#[tauri::command]
pub async fn hardware_address_index(state: State<'_, AppState>, account: AccountId, address: String) -> Result<u32> {
    let (pair, network) = hardware(&state, &account).await?;
    let want = address.trim().trim_start_matches("bitcoin:").split('?').next().unwrap_or("").to_lowercase();
    tokio::task::spawn_blocking(move || {
        for i in 0..MAX_ADDRESS_INDEX {
            if rail_onchain::descriptors::address_at(&pair, network, i)?.to_lowercase() == want {
                return Ok(i);
            }
        }
        Err(sessions_core::Error::Invalid("That address isn't one of this account's receive addresses".into()))
    })
    .await
    .map_err(|e| sessions_core::Error::Invalid(e.to_string()))?
}

/// Trezor Connect `getAddress` parameters to show receive address `index` of
/// single-sig hardware account `account`, and the address it must show.
#[tauri::command]
pub async fn trezor_address_request(state: State<'_, AppState>, account: AccountId, index: u32) -> Result<TrezorAddressRequest> {
    let (pair, network) = hardware(&state, &account).await?;
    let path = rail_onchain::bitbox::receive_path(&pair.external, index).map_err(|_| {
        sessions_core::Error::Unsupported("show this account's addresses on a Trezor".into())
    })?;
    let address = rail_onchain::descriptors::address_at(&pair, network, index)?;
    let path_str = format!(
        "m/{}",
        path.keypath
            .iter()
            .map(|c| if c & 0x8000_0000 != 0 { format!("{}'", c & 0x7fff_ffff) } else { c.to_string() })
            .collect::<Vec<_>>()
            .join("/")
    );
    let coin = if network == sessions_core::Network::Bitcoin { "btc" } else { "test" };
    Ok(TrezorAddressRequest {
        params: serde_json::json!({ "path": path_str, "coin": coin, "showOnTrezor": true }),
        address,
    })
}

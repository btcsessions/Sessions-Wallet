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

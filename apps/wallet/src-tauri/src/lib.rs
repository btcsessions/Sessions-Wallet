mod alerts;
mod backup;
mod backup_check;
mod biometric;
mod bitbox;
mod commands;
mod config;
mod jade;
mod link;
#[cfg(target_os = "android")]
mod logcat;
mod monitor;
mod multisig_hw;
#[cfg(desktop)]
mod desktop;
mod group;
mod group_join;
mod nostr_id;
mod nwc;
mod passkey;
mod state;
mod vault;

use std::sync::Arc;

use tauri::{Emitter, Manager};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "android")]
    logcat::init();
    tauri::Builder::default()
        // Trezor Suite on phones: we open its deep link, it calls us back.
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        // BitBox02 over USB on Android (USB permission + HID reports).
        .plugin(tauri_plugin_bitbox::init())
        // Blockstream Jade over Bluetooth LE on Android (scan, bond, GATT).
        .plugin(tauri_plugin_jade::init())
        // Passkeys with PRF (Android Credential Manager), for passkey wallets.
        .plugin(tauri_plugin_passkey::init())
        // Fingerprint / face unlock (Android Keystore + BiometricPrompt).
        .plugin(tauri_plugin_biometric_vault::init())
        // Payment notifications, and watching with the app closed.
        .plugin(tauri_plugin_alerts::init())
        // Save or share wallet export files.
        .plugin(tauri_plugin_files::init())
        .plugin(tauri_plugin_marmot::init())
        // The QR scanner uses the camera from inside the webview.
        .on_permission_request(|_, kind| match kind {
            tauri::webview::PermissionKind::Camera => tauri::webview::PermissionResponse::Allow,
            _ => tauri::webview::PermissionResponse::Default,
        })
        .on_window_event(|_window, _event| {
            #[cfg(desktop)]
            desktop::on_window_event(_window, _event);
        })
        .setup(|app| {
            #[cfg(desktop)]
            desktop::setup(app)?;
            #[cfg(target_os = "linux")]
            if let Some(w) = app.get_webview_window("main") {
                // WebKitGTK ships with getUserMedia switched off.
                let _ = w.with_webview(|wv| {
                    use webkit2gtk::{SettingsExt, WebViewExt};
                    if let Some(s) = wv.inner().settings() {
                        s.set_enable_media_stream(true);
                    }
                });
            }
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            // Tell the UI when an account changes so it refreshes at once:
            // `wallet://changed` with `{ account }`, and the older
            // payload-less `wallet-changed`.
            // Then look for newly arrived payments (`wallet://payment`, and a
            // notification when the app isn't on screen).
            let emitter = app.handle().clone();
            let notify: state::Notify = Arc::new(move |account: &sessions_core::AccountId| {
                let _ = emitter.emit(
                    "wallet://changed",
                    serde_json::json!({ "account": account.0 }),
                );
                let _ = emitter.emit("wallet-changed", ());
                alerts::Alerts::changed(&emitter, account.clone());
            });
            app.manage(bitbox::BitBoxState::new(&dir));
            app.manage(jade::JadeState::new());
            let state = state::AppState::new(dir, Some(notify));
            // Bind the wallet vault to this phone's Keystore.
            #[cfg(target_os = "android")]
            state.set_device_key(Arc::new(KeystoreKey(app.handle().clone())));
            // `backup://status` (BackupStatus) whenever it changes, and
            // `backup://restored` (RestoreResult) when the backup loop
            // restored a wallet's state on its own.
            let emitter = app.handle().clone();
            state
                .backup
                .set_emitter(Arc::new(move |event: &str, payload| {
                    let _ = emitter.emit(event, payload);
                }));
            let emitter = app.handle().clone();
            state
                .alerts
                .set_emitter(Arc::new(move |event: &str, payload| {
                    let _ = emitter.emit(event, payload);
                }));
            // Nostr Wallet Connect budget use goes back into the config.
            let handle = app.handle().clone();
            let _ = state
                .nwc_saver
                .set(Arc::new(move |c: &sessions_nwc::Connection| {
                    let (handle, c) = (handle.clone(), c.clone());
                    tauri::async_runtime::spawn(async move {
                        let state = handle.state::<state::AppState>();
                        nwc::save_usage(&state, &c).await;
                    });
                }));
            app.manage(state);

            // Keep the encrypted state backup up to date.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<state::AppState>();
                backup::run(&state).await;
            });

            // Keep swaps moving in the background: Boltz hold invoices only
            // settle after we claim, so this can't wait for a manual refresh.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut round: u64 = 0;
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    round += 1;
                    let state = handle.state::<state::AppState>();
                    let wallet = state.inner.lock().await.as_ref().map(|u| u.wallet.clone());
                    if let Some(w) = wallet {
                        w.tick_swaps().await;
                    }
                    // Incoming payjoins: every 30 s, for accounts that opted in.
                    if round.is_multiple_of(3) {
                        for account in state.payjoin_receivers().await {
                            if let Err(e) = account.payjoin_tick().await {
                                tracing::warn!("payjoin: {e}");
                            }
                        }
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::status,
            commands::create_wallet,
            commands::restore_wallet,
            commands::unlock,
            commands::lock,
            commands::reveal_phrase,
            commands::accounts,
            commands::add_account,
            commands::rename_account,
            jade::jade_qr_pin,
            commands::discover_hot_accounts,
            commands::scan_descriptors,
            commands::cosigner_key,
            commands::sync,
            commands::activity,
            commands::parse,
            commands::plan_payment,
            commands::plan_move,
            commands::prepare,
            commands::confirm,
            commands::resume,
            commands::receive,
            commands::redeem,
            commands::create_token,
            commands::layout,
            commands::save_layout,
            commands::fee_rates,
            commands::btc_price,
            commands::settings,
            commands::save_settings,
            commands::unavailable_accounts,
            commands::lightning_address,
            commands::lightning_address_available,
            commands::register_lightning_address,
            commands::spending_deposits,
            commands::claim_deposit,
            commands::refund_deposit,
            commands::remove_account,
            commands::payjoin_receive,
            commands::set_payjoin_receive,
            commands::inspect_descriptor,
            commands::account_details,
            commands::receive_quote,
            commands::trezor_sign_request,
            commands::trezor_merge,
            bitbox::bitbox_descriptors,
            bitbox::bitbox_sign,
            bitbox::bitbox_show_address,
            jade::jade_descriptors,
            jade::jade_sign,
            jade::jade_show_address,
            commands::tx_details,
            commands::sync_account,
            commands::check_incoming,
            commands::watch_address,
            commands::prepare_bump,
            commands::confirm_quote,
            commands::list_utxos,
            commands::set_label,
            commands::set_frozen,
            commands::backup_status,
            commands::backup_now,
            commands::set_phrase_backed_up,
            commands::restore_state,
            backup_check::test_backup,
            commands::prefs,
            commands::save_prefs,
            passkey::passkey_supported,
            passkey::create_passkey_wallet,
            passkey::restore_with_passkey,
            passkey::enable_passkey_backup,
            passkey::disable_passkey_backup,
            biometric::biometric_status,
            biometric::enable_biometric,
            biometric::disable_biometric,
            biometric::unlock_biometric,
            bitbox::bitbox_cancel,
            bitbox::bitbox_multisig_key,
            bitbox::bitbox_register_multisig,
            bitbox::bitbox_show_multisig_address,
            jade::jade_cancel,
            jade::jade_multisig_key,
            jade::jade_register_multisig,
            jade::jade_show_multisig_address,
            multisig_hw::trezor_multisig_sign_request,
            multisig_hw::trezor_multisig_merge,
            multisig_hw::trezor_multisig_address_request,
            vault::file_export,
            vault::vault_phone_key,
            vault::vault_read_key,
            vault::vault_create,
            vault::vault_import_preview,
            vault::vault_import,
            vault::vault_details,
            vault::vault_set_key_label,
            vault::vault_export,
            vault::vault_signatures,
            vault::vault_combine,
            vault::proposals,
            vault::proposal_save,
            vault::proposal_delete,
            group::group_create,
            group::group_view,
            group::group_code,
            group::group_requests,
            group::group_approve,
            group::group_ignore,
            group::group_list,
            group::group_accounts,
            group::group_delete,
            multisig_hw::hardware_address_index,
            multisig_hw::trezor_address_request,
            commands::set_secure_screen,
            commands::sync_status,
            commands::sync_now,
            link::link_offer,
            link::link_poll,
            link::link_finish,
            link::link_cancel,
            link::link_scan,
            link::link_send,
            group::group_respond,
            group::group_set_name,
            group::group_propose,
            group::group_submit_key,
            group::group_finalize,
            group::group_confirm,
            group_join::group_join_lookup,
            group_join::group_join_send,
            nostr_id::nostr_identity,
            nostr_id::nostr_update,
            nwc::nwc_list,
            nwc::nwc_create,
            nwc::nwc_update,
            nwc::nwc_delete,
            nwc::nwc_uri,
            commands::diagnostic_log,
            commands::alerts_status,
            commands::alerts_request_notifications,
            commands::alerts_request_unrestricted,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Sessions Wallet")
        .run(|_app, _event| {
            #[cfg(desktop)]
            desktop::on_run_event(_app, &_event);
        });
}

/// The Android Keystore device key (through the biometric-vault plugin).
#[cfg(target_os = "android")]
struct KeystoreKey(tauri::AppHandle);

#[cfg(target_os = "android")]
#[async_trait::async_trait]
impl state::DeviceKey for KeystoreKey {
    async fn wrap(&self, secret: &[u8]) -> sessions_core::Result<Vec<u8>> {
        let v = self.0.state::<tauri_plugin_biometric_vault::BiometricVault<tauri::Wry>>();
        v.device_wrap(secret)
            .await
            .map_err(|e| sessions_core::Error::Crypto(e.to_string()))
    }

    async fn unwrap(&self, wrapped: &[u8]) -> sessions_core::Result<Vec<u8>> {
        let v = self.0.state::<tauri_plugin_biometric_vault::BiometricVault<tauri::Wry>>();
        v.device_unwrap(wrapped)
            .await
            .map(|z| z.to_vec())
            .map_err(|e| sessions_core::Error::Crypto(e.to_string()))
    }
}

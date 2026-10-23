mod backup;
mod bitbox;
mod commands;
mod config;
mod jade;
#[cfg(target_os = "android")]
mod logcat;
mod monitor;
mod passkey;
mod state;

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
        // The QR scanner uses the camera from inside the webview.
        .on_permission_request(|_, kind| match kind {
            tauri::webview::PermissionKind::Camera => tauri::webview::PermissionResponse::Allow,
            _ => tauri::webview::PermissionResponse::Default,
        })
        .setup(|app| {
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
            let emitter = app.handle().clone();
            let notify: state::Notify = Arc::new(move |account: &sessions_core::AccountId| {
                let _ = emitter.emit(
                    "wallet://changed",
                    serde_json::json!({ "account": account.0 }),
                );
                let _ = emitter.emit("wallet-changed", ());
            });
            app.manage(bitbox::BitBoxState::new(&dir));
            app.manage(jade::JadeState::new());
            let state = state::AppState::new(dir, Some(notify));
            // `backup://status` (BackupStatus) whenever it changes, and
            // `backup://restored` (RestoreResult) when the backup loop
            // restored a wallet's state on its own.
            let emitter = app.handle().clone();
            state
                .backup
                .set_emitter(Arc::new(move |event: &str, payload| {
                    let _ = emitter.emit(event, payload);
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
            commands::prefs,
            commands::save_prefs,
            passkey::passkey_supported,
            passkey::create_passkey_wallet,
            passkey::restore_with_passkey,
            passkey::enable_passkey_backup,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Sessions Wallet");
}

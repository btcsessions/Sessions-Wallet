mod commands;
mod config;
mod state;

use std::sync::Arc;

use tauri::{Emitter, Manager};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            // Tell the UI when an account changes so it refreshes at once.
            let emitter = app.handle().clone();
            let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                let _ = emitter.emit("wallet-changed", ());
            });
            app.manage(state::AppState::new(dir, Some(notify)));

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
            commands::remove_account,
            commands::payjoin_receive,
            commands::set_payjoin_receive,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Sessions Wallet");
}

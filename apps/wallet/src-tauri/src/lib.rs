mod commands;
mod config;
mod state;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            app.manage(state::AppState::new(dir));
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running Sessions Wallet");
}

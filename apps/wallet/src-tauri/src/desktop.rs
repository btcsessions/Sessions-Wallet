//! Desktop window behaviour (macOS, Linux, Windows).
//!
//! * With payment alerts set to keep watching, closing the window hides it
//!   and the wallet keeps running in the menu bar / system tray, so payments
//!   still raise notifications. Quit from the tray (or ⌘Q) ends it.
//! * Window focus decides whether a payment gets a system notification or
//!   the in-window one.

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent, WindowEvent};

pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// The tray / menu bar icon: open the window, or quit.
pub fn setup(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Sessions Wallet", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Sessions Wallet", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("Sessions Wallet")
        .menu(&menu)
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_menu_event(|app, e| match e.id().as_ref() {
            "open" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, e| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

pub fn on_window_event(window: &tauri::Window, event: &WindowEvent) {
    match event {
        WindowEvent::Focused(on) => tauri_plugin_alerts::desktop::set_focused(*on),
        WindowEvent::CloseRequested { api, .. } if tauri_plugin_alerts::desktop::watching() => {
            // Keep watching for payments: hide instead of quitting.
            api.prevent_close();
            let _ = window.hide();
            tauri_plugin_alerts::desktop::set_focused(false);
        }
        _ => {}
    }
}

pub fn on_run_event(app: &AppHandle, event: &RunEvent) {
    // macOS: clicking the Dock icon brings the hidden window back.
    #[cfg(target_os = "macos")]
    if let RunEvent::Reopen { .. } = event {
        show_main(app);
    }
    let _ = (app, event);
}

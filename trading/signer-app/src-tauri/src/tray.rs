//! System-tray icon + menu.
//!
//! Tauri 2 ships a first-class tray API. We install a single tray
//! entry with a colored dot icon reflecting the daemon's current
//! health (green/amber/red), plus a small menu for show/quit and the
//! current paused state.

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    App, Emitter, Manager,
};

pub fn install(app: &mut App) -> tauri::Result<()> {
    let show_i = MenuItem::with_id(app, "show", "Open DegenBox Signer", true, None::<&str>)?;
    let pause_i = MenuItem::with_id(app, "pause", "Pause signing", true, None::<&str>)?;
    let logs_i = MenuItem::with_id(app, "logs", "Open log file", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_i, &pause_i, &logs_i, &sep, &quit_i])?;

    let _tray = TrayIconBuilder::with_id("main")
        .menu(&menu)
        .tooltip("DegenBox Signer")
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            "pause" => {
                // Toggle handled in the React frontend via an emit; the
                // tray click just opens the window so the user sees
                // the toggle land. Keeps state authoritatively in
                // `AppState`.
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                    let _ = w.emit("tray:toggle-pause", ());
                }
            }
            "logs" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.emit("tray:open-logs", ());
                }
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    Ok(())
}

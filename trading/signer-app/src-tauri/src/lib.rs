//! DegenBox Signer Tauri 2 application.
//!
//! Wraps `signer-core` behind the standard Tauri IPC surface so the
//! React frontend can drive onboarding, keystore unlock, status
//! polling, and pause/resume signing without ever touching the
//! private keys directly. The keys live in the Rust process; the
//! frontend only sees pubkeys, addresses, statuses, and (on
//! explicit user action) decrypted-and-then-re-encrypted blobs.
//!
//! Design notes:
//!
//! - Single instance — clicking the .app twice focuses the running
//!   window. The signer daemon would otherwise race itself for the
//!   server's `/signer/pending` queue and double-sign orders.
//! - Tray icon — green / amber / red status reflects the daemon's
//!   current health. Idle when paused, amber when a poll/sign fails,
//!   red when the keystore is locked or no agent is registered.
//! - Updater — checks `latest.json` on launch, prompts via native
//!   dialog if there's a newer release. Update payloads are
//!   ed25519-verified before install (pubkey is baked into
//!   `tauri.conf.json`).

mod auth;
mod clients;
mod commands;
mod hl;
mod local_daemon;
mod sol;
mod state;
mod trade_actions;
mod tray;

use tauri::Manager;
use tauri_plugin_deep_link::DeepLinkExt;

pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .try_init();

    tauri::Builder::default()
        // Single-instance handler — when a second launch fires, focus
        // the existing window instead of starting a parallel daemon.
        // On Windows/Linux deep links arrive as argv of that second
        // launch, so forward any degenbox:// arg to the auth handler.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
            for arg in argv {
                if arg.starts_with("degenbox://") {
                    auth::handle_deep_link(app, &arg);
                }
            }
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_deep_link::init())
        .manage(state::AppState::default())
        .setup(|app| {
            tray::install(app)?;
            // Serve the frozen signer-protocol RPC on 127.0.0.1:5829 for
            // the whole app lifetime so the DegenBox web app detects this
            // client exactly like the signer-cli daemon / extension. Key
            // endpoints stay 503-locked until the user unlocks.
            local_daemon::spawn(&app.handle().clone());
            // Runtime deep-link listener — the Discord auth callback
            // (`degenbox://auth/callback?code=…`) lands here on macOS;
            // Windows/Linux route through the single-instance argv.
            #[cfg(any(windows, target_os = "linux"))]
            {
                // Dev builds aren't registered as the scheme handler by
                // the installer — register at runtime so the flow works
                // from `tauri dev` too.
                let _ = app.deep_link().register_all();
            }
            let handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    auth::handle_deep_link(&handle, url.as_str());
                }
            });
            // Cold-start deep link: when the OS launches the app BY the
            // link (Windows/Linux argv, macOS launch event), the URL is
            // already "current" before the listener above exists.
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                let handle = app.handle().clone();
                for url in urls {
                    auth::handle_deep_link(&handle, url.as_str());
                }
            }
            // Restore the persisted kill-switch BEFORE anything can
            // spawn a daemon — "paused" must survive a relaunch.
            commands::restore_paused(&app.state::<state::AppState>());
            // Keychain auto-unlock — off the setup thread because the
            // Argon2 KDF takes a moment; the UI polls signer_status and
            // flips from "Locked" to "Active" when this lands.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                commands::try_keychain_auto_unlock(&handle);
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_version,
            commands::reset_keystore,
            commands::signer_status,
            commands::set_paused,
            commands::onboarding_state,
            commands::generate_solana_wallet,
            commands::import_solana_wallet,
            commands::generate_hl_keystore,
            commands::import_hl_keystore,
            commands::unlock_keystores,
            commands::lock_keystores,
            commands::list_recent_signs,
            commands::hl_pair,
            commands::hl_status,
            commands::hl_set_paper_mode,
            commands::hl_pairing_status,
            commands::hl_unpair,
            commands::submit_hl_totp,
            commands::pick_backend,
            commands::open_logs,
            commands::open_setup_url,
            commands::local_daemon_status,
            commands::export_sol_keystore,
            commands::reveal_sol_secret,
            commands::remove_sol_keystore,
            commands::remove_hl_keystore,
            clients::clients_list,
            clients::client_add,
            clients::client_import,
            clients::client_activate,
            clients::client_remove,
            clients::client_gateway_deregister,
            clients::client_label,
            clients::client_pause,
            clients::client_runtime_status,
            clients::client_set_primary,
            clients::client_export_keystore,
            clients::client_budget_set,
            clients::client_active_config_set,
            clients::client_active_config_clear,
            clients::client_presets_list,
            clients::client_preset_assign,
            clients::client_preset_unassign,
            clients::client_copy_configs,
            auth::discord_login_start,
            auth::discord_account_status,
            auth::discord_unlink,
            auth::access_check,
            sol::commands::sol_positions,
            sol::commands::gateway_fetch,
            sol::commands::sol_wallet_balance,
            sol::commands::bot_presets_status,
            sol::commands::copytrade_configs,
            sol::commands::copytrade_set_enabled,
            sol::commands::sol_runtime_status,
            sol::commands::sol_exec_config_get,
            sol::commands::sol_exec_config_set,
            sol::commands::sol_exec_params_set,
            sol::commands::detect_cli_keystore,
            sol::commands::import_sol_keystore_file,
            sol::commands::import_extension_keystore,
            trade_actions::tracked_wallets_list,
            trade_actions::tracked_wallet_set_copy_mode,
            trade_actions::sol_copy_configs_full,
            trade_actions::sol_copy_config_create,
            trade_actions::sol_copy_config_update,
            trade_actions::sol_copy_config_delete,
            trade_actions::sol_targets_list,
            trade_actions::sol_target_get,
            trade_actions::sol_target_arm,
            trade_actions::sol_target_disarm,
            trade_actions::sol_position_sell,
            trade_actions::alpha_presets,
            trade_actions::bot_session_create,
            trade_actions::bot_session_cancel,
            trade_actions::bot_arm,
            trade_actions::bot_disarm,
            trade_actions::bot_device_status,
            trade_actions::hl_close_position,
            trade_actions::hl_place_tpsl,
            trade_actions::hl_copy_configs_full,
            trade_actions::hl_copy_config_create,
            trade_actions::hl_copy_config_update,
            trade_actions::client_preset_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

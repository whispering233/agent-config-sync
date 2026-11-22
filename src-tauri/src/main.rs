#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod apply;
mod auto_sync;
mod commands;
mod config;
mod conflicts;
mod credentials;
mod doctor;
mod installer;
mod package;
mod packages;
mod plan;
mod platform;
mod rollback;
mod runtime;
mod scope;
mod secrets;
mod snapshot;
mod store;
mod tray;
mod versions;
mod webdav;
mod webdav_transfer;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::show_main(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::Builder::new().build())
        .setup(|app| {
            tray::setup(app)?;
            auto_sync::start(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                    #[cfg(target_os = "macos")]
                    {
                        let _ = window
                            .app_handle()
                            .set_activation_policy(tauri::ActivationPolicy::Accessory);
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::load_app_config,
            commands::validate_app_config,
            commands::save_app_config,
            commands::auto_sync_status,
            commands::trigger_auto_sync,
            commands::load_webdav_summary,
            commands::save_webdav_credentials,
            commands::test_webdav_connection,
            commands::download_remote_snapshot,
            commands::list_snapshot_packages,
            commands::upload_snapshot,
            doctor::run_check,
            installer::preview_install,
            installer::install_or_upgrade,
            packages::package_status,
            packages::align_packages,
            snapshot::export_snapshot,
            plan::verify_and_plan,
            conflicts::scan_conflicts,
            apply::apply_snapshot,
            rollback::list_backups,
            rollback::restore_backup,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

mod commands;
mod config;
mod conflicts;
mod doctor;
mod installer;
mod scope;
mod secrets;
mod snapshot;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::load_app_config,
            commands::validate_app_config,
            commands::save_app_config,
            doctor::run_check,
            installer::preview_install,
            installer::install_or_upgrade,
            snapshot::export_snapshot,
            conflicts::scan_conflicts,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

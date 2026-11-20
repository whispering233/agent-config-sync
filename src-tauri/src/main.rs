mod commands;
mod config;
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
            snapshot::export_snapshot,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

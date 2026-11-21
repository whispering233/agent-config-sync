mod commands;
mod apply;
mod config;
mod conflicts;
mod doctor;
mod installer;
mod packages;
mod plan;
mod rollback;
mod scope;
mod secrets;
mod snapshot;
// 协议层先独立落地并与 UI 解耦：接线前没有调用方，先关掉 dead_code 噪音。
#[allow(dead_code)]
mod webdav;
mod wizard;

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
            packages::package_status,
            packages::align_packages,
            snapshot::export_snapshot,
            plan::verify_and_plan,
            conflicts::scan_conflicts,
            apply::apply_snapshot,
            rollback::list_backups,
            rollback::restore_backup,
            wizard::check_secrets,
            wizard::write_secret,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

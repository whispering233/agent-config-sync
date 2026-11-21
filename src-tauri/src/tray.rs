//! 托盘图标与菜单。关闭主窗口只隐藏，不退出；退出必须走菜单。
//!
//! Linux 的 AppIndicator 只可靠地派发菜单事件，不派发图标点击事件；Windows/macOS
//! 左键唤回主窗口。暂停只作用于本次进程，持久开关在设置页。

use crate::auto_sync::{self, AutoSyncStatus};
use std::sync::Mutex;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

pub const TRAY_ID: &str = "main-tray";

/// 暂停菜单项要在点击后更新勾选状态，因此留一个句柄。
pub struct TrayState {
    pause: Mutex<Option<CheckMenuItem<Wry>>>,
}

pub fn setup(app: &mut tauri::App) -> tauri::Result<()> {
    let open = MenuItemBuilder::with_id("open", "打开主窗口").build(app)?;
    let sync = MenuItemBuilder::with_id("sync", "立即同步").build(app)?;
    let pause = CheckMenuItemBuilder::with_id("pause", "暂停自动同步")
        .checked(false)
        .build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "退出").build(app)?;
    let menu = MenuBuilder::new(app)
        .items(&[&open, &sync, &pause])
        .separator()
        .item(&quit)
        .build()?;

    let (icon, template) = icon()?;
    let builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .show_menu_on_left_click(cfg!(target_os = "linux"))
        .on_menu_event(|app, event| handle_menu(app, &event.id.0))
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                show_main(tray.app_handle());
            }
        })
        .icon(icon)
        .icon_as_template(template)
        .tooltip("Agent Config Sync");

    #[cfg(target_os = "linux")]
    // AppIndicator 需要把图标落到磁盘；应用缓存目录比 XDG_RUNTIME_DIR 稳。
    let builder = builder.temp_dir_path(app.path().app_cache_dir()?.join("tray-icon"));

    builder.build(app)?;
    app.manage(TrayState {
        pause: Mutex::new(Some(pause)),
    });
    Ok(())
}

pub fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        #[cfg(target_os = "macos")]
        {
            let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
        }
    }
}

fn handle_menu(app: &AppHandle, id: &str) {
    match id {
        "open" => show_main(app),
        "sync" => auto_sync::trigger_now(),
        "pause" => {
            let paused = auto_sync::toggle_paused();
            if let Some(state) = app.try_state::<TrayState>() {
                if let Ok(guard) = state.pause.lock() {
                    if let Some(item) = guard.as_ref() {
                        let _ = item.set_checked(paused);
                    }
                }
            }
            update_tooltip(app, &auto_sync::status());
        }
        "quit" => {
            if let Some(tray) = app.tray_by_id(TRAY_ID) {
                let _ = tray.set_visible(false);
            }
            app.exit(0);
        }
        _ => {}
    }
}

/// 状态变化时更新托盘悬停提示；托盘还没建好时静默跳过。
pub fn update_tooltip(app: &AppHandle, status: &AutoSyncStatus) {
    let label = match status.state {
        "running" => "正在同步",
        "success" => "同步完成",
        "conflict" => "云端有更新",
        "failed" => "同步失败",
        "paused" => "自动同步已暂停",
        "skipped" => "自动同步已跳过",
        _ => "空闲",
    };
    let text = match status.message.as_deref() {
        Some(message) if !message.is_empty() => {
            let mut short: String = message.chars().take(80).collect();
            if message.chars().count() > 80 {
                short.push('…');
            }
            format!("Agent Config Sync · {label}：{short}")
        }
        _ => format!("Agent Config Sync · {label}"),
    };
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(&text));
    }
}

fn icon() -> tauri::Result<(Image<'static>, bool)> {
    #[cfg(target_os = "macos")]
    {
        // 菜单栏用单色模板图，系统自动适配深浅色。
        Ok((
            Image::from_bytes(include_bytes!("../icons/tray-template.png"))?,
            true,
        ))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok((
            Image::from_bytes(include_bytes!("../icons/tray.png"))?,
            false,
        ))
    }
}

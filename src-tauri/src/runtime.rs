//! 进程级运行状态：手动操作与自动上行共用同一把锁，避免两套写盘同时跑。

use std::sync::{Mutex, MutexGuard, TryLockError};
use tauri_plugin_autostart::ManagerExt;

static OPERATION_LOCK: Mutex<()> = Mutex::new(());

/// 阻塞等待当前操作的锁。command 层在开始写盘前拿它。
pub fn lock_operation() -> MutexGuard<'static, ()> {
    OPERATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 非阻塞尝试；自动上行遇到手动操作正在跑时直接跳过，不排队。
pub fn try_lock_operation() -> Option<MutexGuard<'static, ()>> {
    match OPERATION_LOCK.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// 开机自启由系统登记，配置里的开关只是这份系统状态的镜像。
pub fn set_launch_at_login(app: &tauri::AppHandle, enabled: bool) -> Result<(), String> {
    let autolaunch = app.autolaunch();
    if enabled {
        autolaunch
            .enable()
            .map_err(|error| format!("开启开机自启失败：{error}"))
    } else {
        autolaunch
            .disable()
            .map_err(|error| format!("关闭开机自启失败：{error}"))
    }
}

//! 托盘常驻时的自动上行。
//!
//! 只上行：按配置的间隔把当前 live 导出并上传（默认不传 `auth.json`），不自动下载/应用，
//! 失败只更新状态，不重试。上传本身与手动上传同一条链路（[`crate::upload`]）。
//!
//! 远端有本机未见的新包时限常上传并报 `diverged`：包名唯一且只增不改，不存在覆盖别人
//! 的风险，没必要因此停住上行。

use crate::config::{self, AppConfig, Transport};
use crate::snapshot;
use crate::upload::{self, UploadFailure, UploadOutcome};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

/// 启动后先等一会儿，避开窗口初始化与用户刚打开应用的手动操作。
const STARTUP_DELAY: Duration = Duration::from_secs(60);
/// 托盘菜单「暂停自动同步」只在本次进程内生效；持久开关在设置页。
static PAUSED: AtomicBool = AtomicBool::new(false);
/// 「立即同步」即使自动同步关闭也执行一次。
static FORCE_ONE: AtomicBool = AtomicBool::new(false);
static WAKE: OnceLock<Sender<()>> = OnceLock::new();
static STATUS: LazyLock<Mutex<AutoSyncStatus>> =
    LazyLock::new(|| Mutex::new(AutoSyncStatus::idle()));

/// 自动上行状态。只含状态与可读文案，不含凭据或配置内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoSyncStatus {
    /// `idle` / `running` / `success` / `diverged` / `failed` / `skipped` / `paused`
    pub state: &'static str,
    pub message: Option<String>,
    /// 最近一次真正尝试上传的时间；从未尝试过为 `null`。
    pub last_at: Option<String>,
}

impl AutoSyncStatus {
    fn idle() -> Self {
        Self {
            state: "idle",
            message: None,
            last_at: None,
        }
    }
}

/// 当前状态快照，供 command 与设置页读取。
pub fn status() -> AutoSyncStatus {
    STATUS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// 立即唤醒工作线程，执行一次上行；自动同步关闭时也执行。
pub fn trigger_now() {
    FORCE_ONE.store(true, Ordering::SeqCst);
    wake();
}

/// 设置页保存后调用：唤醒线程重新读取配置，避免等满一个间隔。
pub fn wake() {
    if let Some(sender) = WAKE.get() {
        let _ = sender.send(());
    }
}

/// 托盘菜单用：切换本次进程的暂停状态，返回切换后的值。
pub fn toggle_paused() -> bool {
    let paused = !PAUSED.load(Ordering::SeqCst);
    PAUSED.store(paused, Ordering::SeqCst);
    wake();
    paused
}

/// 启动后台线程。只应在应用 setup 里调用一次。
pub fn start(app: AppHandle) {
    let (sender, receiver) = mpsc::channel();
    if WAKE.set(sender).is_err() {
        return;
    }

    std::thread::spawn(move || {
        let mut first = true;
        loop {
            let wait = if first {
                STARTUP_DELAY
            } else {
                config_interval(&app)
            };
            match receiver.recv_timeout(wait) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            first = false;

            let force = FORCE_ONE.swap(false, Ordering::SeqCst);
            if PAUSED.load(Ordering::SeqCst) && !force {
                set_status(&app, "paused", Some("自动同步已暂停".to_string()), false);
                continue;
            }

            let Some(config) = load_config(&app) else {
                set_status(&app, "failed", Some("无法读取应用配置".to_string()), true);
                continue;
            };
            if !force && !config.auto_sync {
                set_status(&app, "skipped", Some("自动同步已关闭".to_string()), false);
                continue;
            }
            if config.transport != Transport::Webdav {
                set_status(
                    &app,
                    "skipped",
                    Some("传输方式是本地文件夹，未执行 WebDAV 上行".to_string()),
                    false,
                );
                continue;
            }

            // 手动上传/下载/应用正在跑时直接跳过本次，不排队。
            let Some(_guard) = crate::runtime::try_lock_operation() else {
                set_status(
                    &app,
                    "skipped",
                    Some("另一项同步操作正在执行".to_string()),
                    false,
                );
                continue;
            };

            let result = upload::upload_from_app(&app, config.auto_sync_include_credentials);
            let (state, message, stamp) = status_for(&result);
            set_status(&app, state, Some(message), stamp);
        }
    });
}

/// 一次上行的结论 → 托盘与设置页看到的状态。只做映射，不做 IO。
fn status_for(result: &Result<UploadOutcome, UploadFailure>) -> (&'static str, String, bool) {
    match result {
        // 内容未变：远端已是最新一份，没有产生新包，也不算失败。
        Ok(outcome) if outcome.view.unchanged => {
            let mut message = format!("内容未变，已跳过（{}）", outcome.view.package);
            // 另一台机器可能在这期间推了新包：自己没上传，但云端确实有本机未见的快照。
            if outcome.unseen > 0 {
                message.push_str(&format!("；云端有 {} 份本机未见的新快照", outcome.unseen));
                return ("diverged", message, false);
            }
            ("skipped", message, false)
        }
        Ok(outcome) => {
            let mut message = format!("已上传 {}", outcome.view.package);
            if outcome.unseen > 0 {
                message.push_str(&format!("；云端有 {} 份本机未见的新快照", outcome.unseen));
            }
            if !outcome.view.prune_failed.is_empty() {
                message.push_str(&format!(
                    "；{} 份旧包清理失败",
                    outcome.view.prune_failed.len()
                ));
            }
            let state = if outcome.unseen > 0 {
                "diverged"
            } else {
                "success"
            };
            (state, message, true)
        }
        Err(failure) => ("failed", failure.message.clone(), true),
    }
}

fn config_interval(app: &AppHandle) -> Duration {
    let minutes = load_config(app)
        .map(|config| config.auto_sync_interval_minutes)
        .unwrap_or(config::DEFAULT_AUTO_SYNC_INTERVAL_MINUTES);
    let minutes = minutes.clamp(
        config::MIN_AUTO_SYNC_INTERVAL_MINUTES,
        config::MAX_AUTO_SYNC_INTERVAL_MINUTES,
    );
    Duration::from_secs(u64::from(minutes) * 60)
}

fn load_config(app: &AppHandle) -> Option<AppConfig> {
    let dir = app.path().app_config_dir().ok()?;
    config::load_config(&dir).ok()
}

fn set_status(app: &AppHandle, state: &'static str, message: Option<String>, stamp: bool) {
    let status = {
        let mut guard = STATUS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.state = state;
        guard.message = message;
        if stamp {
            guard.last_at = Some(snapshot::format_timestamp(now_seconds()));
        }
        guard.clone()
    };
    let _ = app.emit("auto-sync-status", &status);
    crate::tray::update_tooltip(app, &status);
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upload::{PruneFailure, UploadView};

    fn view(package: &str, unchanged: bool) -> UploadView {
        UploadView {
            package: package.to_string(),
            unchanged,
            pruned: Vec::new(),
            root_warnings: Vec::new(),
            prune_failed: Vec::new(),
        }
    }

    #[test]
    fn unchanged_content_is_reported_as_skipped() {
        let outcome = UploadOutcome {
            view: view("20261122-105230-deadbeef.zip", true),
            unseen: 0,
        };

        let (state, message, stamp) = status_for(&Ok(outcome));

        assert_eq!(state, "skipped");
        assert!(message.contains("内容未变"), "{message}");
        assert!(!stamp, "没有尝试上传就不该刷新上次时间");
    }

    #[test]
    fn remote_newer_packages_are_reported_as_diverged() {
        let outcome = UploadOutcome {
            view: view("20261122-105230-deadbeef.zip", false),
            unseen: 2,
        };

        let (state, message, stamp) = status_for(&Ok(outcome));

        assert_eq!(state, "diverged");
        assert!(message.contains("云端有 2 份本机未见的新快照"), "{message}");
        assert!(stamp);
    }

    /// 内容未变但云端有他机的新包：跳过的是上传，不是发现新包。
    #[test]
    fn unchanged_content_with_new_remote_packages_is_reported_as_diverged() {
        let outcome = UploadOutcome {
            view: view("20261122-105230-deadbeef.zip", true),
            unseen: 1,
        };

        let (state, message, stamp) = status_for(&Ok(outcome));

        assert_eq!(state, "diverged");
        assert!(message.contains("内容未变"), "{message}");
        assert!(message.contains("云端有 1 份本机未见的新快照"), "{message}");
        assert!(!stamp, "没有尝试上传就不该刷新上次时间");
    }

    #[test]
    fn upload_without_new_remote_packages_is_reported_as_success() {
        let outcome = UploadOutcome {
            view: view("20261122-105230-deadbeef.zip", false),
            unseen: 0,
        };

        let (state, message, _) = status_for(&Ok(outcome));

        assert_eq!(state, "success");
        assert!(message.contains("已上传"), "{message}");
    }

    #[test]
    fn prune_failures_are_mentioned_but_keep_the_success_state() {
        let mut upload = view("20261122-105230-deadbeef.zip", false);
        upload.prune_failed.push(PruneFailure {
            name: "20261121-091210-7f3e9c01.zip".to_string(),
            reason: "远端拒绝删除".to_string(),
        });
        let outcome = UploadOutcome {
            view: upload,
            unseen: 0,
        };

        let (state, message, _) = status_for(&Ok(outcome));

        assert_eq!(state, "success");
        assert!(message.contains("1 份旧包清理失败"), "{message}");
    }

    #[test]
    fn failure_is_reported_with_its_message() {
        let failure = UploadFailure::new("remote_insecure", "WebDAV 地址必须是 HTTPS");

        let (state, message, stamp) = status_for(&Err(failure));

        assert_eq!(state, "failed");
        assert_eq!(message, "WebDAV 地址必须是 HTTPS");
        assert!(stamp);
    }
}

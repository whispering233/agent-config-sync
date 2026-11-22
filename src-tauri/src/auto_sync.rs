//! 托盘常驻时的 WebDAV 自动上行。
//!
//! 只做一件事：按配置的间隔把当前 live 导出并上传。默认不传 `auth.json`，
//! 不自动下载/应用，不覆盖远端冲突；失败只更新状态，不重试。
//! 与手动上传/下载/应用共用 [`crate::runtime`] 的操作锁。

use crate::config::{self, AppConfig, Transport, AGENT_DIR_ENV_VAR};
use crate::credentials;
use crate::runtime;
use crate::snapshot;
use crate::webdav::{ReqwestTransport, WebdavClient};
use crate::webdav_transfer::{self, TransferError, UploadOutcome};
use serde::Serialize;
use std::path::PathBuf;
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
    /// `idle` / `running` / `success` / `conflict` / `failed` / `skipped` / `paused`
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
            let Some(_guard) = runtime::try_lock_operation() else {
                set_status(
                    &app,
                    "skipped",
                    Some("另一项同步操作正在执行".to_string()),
                    false,
                );
                continue;
            };
            run_once(&app, &config);
        }
    });
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

fn agent_dir(app: &AppHandle, config: &AppConfig) -> Option<PathBuf> {
    let env_dir = std::env::var_os(AGENT_DIR_ENV_VAR).map(PathBuf::from);
    let home = app.path().home_dir().ok();
    config::resolve_agent_dir(config, env_dir.as_deref(), home.as_deref()).path
}

fn run_once(app: &AppHandle, config: &AppConfig) {
    set_status(app, "running", None, false);

    let Some(agent_dir) = agent_dir(app, config) else {
        set_status(
            app,
            "failed",
            Some("无法确定 Pi 宿主目录".to_string()),
            true,
        );
        return;
    };
    let data_dir = match app.path().app_data_dir() {
        Ok(dir) => dir,
        Err(error) => {
            set_status(
                app,
                "failed",
                Some(format!("无法定位应用数据目录：{error}")),
                true,
            );
            return;
        }
    };
    let credentials = match credentials::load(&data_dir) {
        Ok(Some(credentials)) => credentials,
        Ok(None) => {
            set_status(
                app,
                "skipped",
                Some("还没有配置 WebDAV 凭据".to_string()),
                false,
            );
            return;
        }
        Err(error) => {
            set_status(
                app,
                "failed",
                Some(format!("读取 WebDAV 凭据失败：{error}")),
                true,
            );
            return;
        }
    };

    let transport = match ReqwestTransport::new(&credentials.username, &credentials.password) {
        Ok(transport) => transport,
        Err(error) => {
            set_status(
                app,
                "failed",
                Some(format!("创建 WebDAV 客户端失败：{error}")),
                true,
            );
            return;
        }
    };
    let client = WebdavClient::new(transport, &credentials.url);
    let versions = snapshot::SourceVersions::default();
    let home_dir = app.path().home_dir().ok();
    let outcome = if config.auto_sync_include_credentials {
        webdav_transfer::upload(
            &client,
            &data_dir,
            &agent_dir,
            home_dir.as_deref(),
            &versions,
            false,
        )
    } else {
        webdav_transfer::upload_excluding_auth(
            &client,
            &data_dir,
            &agent_dir,
            home_dir.as_deref(),
            &versions,
            false,
        )
    };

    match outcome {
        Ok(UploadOutcome::Uploaded {
            uploaded, skipped, ..
        }) => set_status(
            app,
            "success",
            Some(format!("已上传 {uploaded} 个文件，跳过 {skipped} 个")),
            true,
        ),
        Ok(UploadOutcome::Conflict { .. }) => set_status(
            app,
            "conflict",
            Some("云端有更新，已停止自动上行；请手动处理冲突".to_string()),
            true,
        ),
        Err(error) => {
            let message = match error {
                TransferError::Insecure(message) => message,
                other => other.to_string(),
            };
            set_status(app, "failed", Some(message), true);
        }
    }
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

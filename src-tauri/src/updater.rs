//! 应用自身更新：把官方 updater 的检查、下载、安装与签名校验包成本应用的 command。
//!
//! 只替换程序文件：不碰配置、备份、凭据与快照。开发态与未分发平台在检查前就拒绝，
//! 一个请求都不发；失败停在当前版本，不重试、不降级。

use crate::auto_sync;
use crate::runtime;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::MutexGuard;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::{Error as UpdaterError, Update, UpdaterExt};

/// 检查只请求一次 `latest.json`。
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
/// 安装包传输超时独立于检查。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// 进度事件名，与前端约定。
const PROGRESS_EVENT: &str = "app-update://progress";
/// 每个 chunk 都发事件会把 IPC 打满。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// 更新只对实际分发的平台开放：macOS 未分发，不做自更新。
const DISTRIBUTED_PLATFORM: bool = cfg!(any(target_os = "linux", target_os = "windows"));

/// 失败码与一句可读信息。不携带错误原文：里面有下载 URL。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateFailure {
    pub code: &'static str,
    pub message: String,
}

impl AppUpdateFailure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// 一次检查发现的可用版本。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateInfo {
    pub current_version: String,
    pub version: String,
    pub notes: Option<String>,
    /// 发布时间，`latest.json` 里的 RFC 3339 原文；没有填写时为 `null`。
    pub date: Option<String>,
    pub target: String,
    /// 安装包体积：检查阶段没有这项信息，下载时由进度事件给出总量。
    pub download_size: Option<u64>,
}

/// 安装动作的结论。失败走 [`AppUpdateFailure`]，因此这里只描述已发起的安装。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateResult {
    pub install_started: bool,
    pub version: String,
    /// 安装替换的是程序文件，重启后才跑新版本。
    pub restart_required: bool,
}

/// 下载/安装进度。事件里只有阶段与字节数。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateProgress {
    /// `downloading` / `installing`
    phase: &'static str,
    downloaded: u64,
    total: Option<u64>,
}

/// 检查更新。开发态与未分发平台返回 `update_unsupported`，不发请求。
#[tauri::command]
pub async fn check_app_update(app: AppHandle) -> Result<Option<AppUpdateInfo>, AppUpdateFailure> {
    check(&app).await
}

/// 下载并安装用户确认的版本。重新检查后版本变了返回 `update_stale`；
/// 有别的操作持锁返回 `update_busy`。
#[tauri::command]
pub async fn install_app_update(
    app: AppHandle,
    version: String,
) -> Result<AppUpdateResult, AppUpdateFailure> {
    gate_release()?;
    let Some(update) = fetch_update(&app, CHECK_TIMEOUT).await? else {
        return Err(AppUpdateFailure::new(
            "update_stale",
            "已经没有可安装的新版本，请重新检查",
        ));
    };
    ensure_same_version(&version, &update.version)?;

    // 下载不写 live，因此不必持锁；持锁会白白挡住用户期间的手动操作。
    let bytes = download(&app, &update).await?;

    // 安装要替换程序文件：必须独占操作锁，并暂停自动上行。
    let _guard = acquire_install_lock()?;
    let _suspend = auto_sync::suspend_for_install();
    emit_progress(
        &app,
        "installing",
        bytes.len() as u64,
        Some(bytes.len() as u64),
    );
    update
        .install(&bytes)
        .map_err(|error| mapped(Phase::Install, error))?;

    // Windows 的安装器自己退出本进程并重启；Linux 只替换文件，必须由这里重启，
    // 否则用户停在旧进程上。
    #[cfg(target_os = "windows")]
    return Ok(AppUpdateResult {
        install_started: true,
        version: update.version.clone(),
        restart_required: true,
    });

    #[cfg(not(target_os = "windows"))]
    app.restart()
}

async fn check(app: &AppHandle) -> Result<Option<AppUpdateInfo>, AppUpdateFailure> {
    gate_release()?;

    Ok(fetch_update(app, CHECK_TIMEOUT).await?.as_ref().map(info))
}

async fn fetch_update(
    app: &AppHandle,
    timeout: Duration,
) -> Result<Option<Update>, AppUpdateFailure> {
    app.updater_builder()
        .timeout(timeout)
        .build()
        .map_err(|error| mapped(Phase::Check, error))?
        .check()
        .await
        .map_err(|error| mapped(Phase::Check, error))
}

async fn download(app: &AppHandle, update: &Update) -> Result<Vec<u8>, AppUpdateFailure> {
    // `Update` 的 timeout 继承自检查时的 `Updater`，下载前换成传输超时。
    let mut update = update.clone();
    update.timeout = Some(DOWNLOAD_TIMEOUT);

    let downloaded = AtomicU64::new(0);
    let total = AtomicU64::new(0);
    let mut last = Instant::now();

    update
        .download(
            |chunk, content_length| {
                downloaded.fetch_add(chunk as u64, Ordering::Relaxed);
                if let Some(content_length) = content_length {
                    total.store(content_length, Ordering::Relaxed);
                }
                if last.elapsed() >= PROGRESS_INTERVAL {
                    last = Instant::now();
                    emit_download_progress(app, &downloaded, &total);
                }
            },
            || emit_download_progress(app, &downloaded, &total),
        )
        .await
        .map_err(|error| mapped(Phase::Download, error))
}

fn info(update: &Update) -> AppUpdateInfo {
    AppUpdateInfo {
        current_version: update.current_version.clone(),
        version: update.version.clone(),
        notes: update.body.clone(),
        date: update
            .raw_json
            .get("pub_date")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        target: update.target.clone(),
        download_size: None,
    }
}

fn emit_download_progress(app: &AppHandle, downloaded: &AtomicU64, total: &AtomicU64) {
    // 拿不到 `Content-Length` 时总量记 0：前端只显示已下载。
    let total = total.load(Ordering::Relaxed);

    emit_progress(
        app,
        "downloading",
        downloaded.load(Ordering::Relaxed),
        (total > 0).then_some(total),
    );
}

fn emit_progress(app: &AppHandle, phase: &'static str, downloaded: u64, total: Option<u64>) {
    let _ = app.emit(
        PROGRESS_EVENT,
        AppUpdateProgress {
            phase,
            downloaded,
            total,
        },
    );
}

/// 开发态与未分发平台都不检查更新：开发版本不是发布版本，macOS 未分发。
fn gate(is_dev: bool, distributed: bool) -> Result<(), AppUpdateFailure> {
    if is_dev || !distributed {
        return Err(AppUpdateFailure::new(
            "update_unsupported",
            "仅发布版支持应用更新",
        ));
    }

    Ok(())
}

fn gate_release() -> Result<(), AppUpdateFailure> {
    gate(tauri::is_dev(), DISTRIBUTED_PLATFORM)
}

/// 用户确认的版本必须与重新检查到的版本一致，否则按 `update_stale` 停住。
fn ensure_same_version(requested: &str, checked: &str) -> Result<(), AppUpdateFailure> {
    if requested != checked {
        return Err(AppUpdateFailure::new(
            "update_stale",
            format!("版本已从 {requested} 变为 {checked}，请重新检查"),
        ));
    }

    Ok(())
}

/// 非阻塞取锁：拿不到说明有快照/上行/技能盘点在跑，本次直接拒绝而不是排队。
fn acquire_install_lock() -> Result<MutexGuard<'static, ()>, AppUpdateFailure> {
    runtime::try_lock_operation()
        .ok_or_else(|| AppUpdateFailure::new("update_busy", "有其它同步操作正在执行，请稍后再安装"))
}

#[derive(Clone, Copy)]
enum Phase {
    Check,
    Download,
    Install,
}

/// 官方错误 → 设计文档的失败码。只按阶段与错误类别归类，不回显错误原文。
fn mapped(phase: Phase, error: UpdaterError) -> AppUpdateFailure {
    let code = match (&error, phase) {
        (
            UpdaterError::UnsupportedOs
            | UpdaterError::UnsupportedArch
            | UpdaterError::TargetNotFound(_)
            | UpdaterError::TargetsNotFound(_),
            _,
        ) => "update_unsupported",
        (
            UpdaterError::Minisign(_)
            | UpdaterError::Base64(_)
            | UpdaterError::SignatureUtf8(_)
            | UpdaterError::SignedVersionMismatch { .. }
            | UpdaterError::MissingSignedVersion,
            _,
        ) => "update_verify_failed",
        (_, Phase::Check) => "update_check_failed",
        (_, Phase::Download) => "update_download_failed",
        (_, Phase::Install) => "update_install_failed",
    };

    AppUpdateFailure::new(code, message_for(code))
}

fn message_for(code: &'static str) -> &'static str {
    match code {
        "update_unsupported" => "当前版本或安装格式不支持应用更新",
        "update_check_failed" => "检查更新失败：没有拿到可用的版本信息",
        "update_download_failed" => "下载安装包失败",
        "update_verify_failed" => "安装包校验失败：签名或版本不匹配",
        "update_busy" => "有其它同步操作正在执行，请稍后再安装",
        "update_stale" => "版本已变化，请重新检查",
        "update_install_failed" => "安装失败：可能取消了系统授权，或安装目录不可写",
        _ => "应用更新失败",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_builds_and_undistributed_platforms_are_unsupported() {
        assert_eq!(gate(true, true).unwrap_err().code, "update_unsupported");
        assert_eq!(gate(false, false).unwrap_err().code, "update_unsupported");
        assert!(gate(false, true).is_ok());
    }

    /// 测试构建就是开发态：真实门禁必须直接拒绝，检查不会发出请求。
    #[test]
    fn test_builds_stop_at_the_gate() {
        assert_eq!(gate_release().unwrap_err().code, "update_unsupported");
    }

    #[test]
    fn version_drift_is_reported_as_stale() {
        assert!(ensure_same_version("0.0.4", "0.0.4").is_ok());
        assert_eq!(
            ensure_same_version("0.0.4", "0.0.5").unwrap_err().code,
            "update_stale"
        );
    }

    /// 有别的操作持锁时不能安装：返回 `update_busy`，不会阻塞等待。
    #[test]
    fn install_is_refused_while_another_operation_holds_the_lock() {
        let held = runtime::lock_operation();

        assert_eq!(acquire_install_lock().unwrap_err().code, "update_busy");

        drop(held);
    }

    #[test]
    fn check_failures_map_to_check_failed() {
        for error in [
            UpdaterError::ReleaseNotFound,
            UpdaterError::Network("boom".to_string()),
            UpdaterError::EmptyEndpoints,
        ] {
            assert_eq!(mapped(Phase::Check, error).code, "update_check_failed");
        }
    }

    #[test]
    fn download_failures_map_to_download_failed() {
        let failure = mapped(Phase::Download, UpdaterError::Network("boom".to_string()));

        assert_eq!(failure.code, "update_download_failed");
    }

    #[test]
    fn install_failures_map_to_install_failed() {
        for error in [
            UpdaterError::AuthenticationFailed,
            UpdaterError::DebInstallFailed,
            UpdaterError::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        ] {
            assert_eq!(mapped(Phase::Install, error).code, "update_install_failed");
        }
    }

    /// 签名与版本绑定失败单独一码：它意味着资产可疑，不能报成普通下载失败。
    #[test]
    fn signature_failures_map_to_verify_failed() {
        let mismatch = UpdaterError::SignedVersionMismatch {
            signed: "0.0.4".to_string(),
            announced: "0.0.5".to_string(),
        };

        assert_eq!(
            mapped(Phase::Download, mismatch).code,
            "update_verify_failed"
        );
        assert_eq!(
            mapped(Phase::Download, UpdaterError::MissingSignedVersion).code,
            "update_verify_failed"
        );
        assert_eq!(
            mapped(Phase::Install, UpdaterError::MissingSignedVersion).code,
            "update_verify_failed"
        );
    }

    #[test]
    fn platform_mismatch_is_reported_as_unsupported() {
        assert_eq!(
            mapped(
                Phase::Check,
                UpdaterError::TargetNotFound("deb".to_string())
            )
            .code,
            "update_unsupported"
        );
        assert_eq!(
            mapped(
                Phase::Check,
                UpdaterError::TargetsNotFound(vec!["linux-x86_64-deb".to_string()])
            )
            .code,
            "update_unsupported"
        );
    }

    /// 文案会进日志与 UI：不能带下载 URL 的查询参数或用户路径。
    #[test]
    fn failure_messages_carry_no_url_or_path() {
        let failure = mapped(
            Phase::Check,
            UpdaterError::Network("https://example.test/latest.json?token=secret".to_string()),
        );

        assert!(!failure.message.contains("http"), "{}", failure.message);
        assert!(!failure.message.contains("secret"), "{}", failure.message);
    }

    #[test]
    fn update_info_serializes_with_camel_case() {
        let info = AppUpdateInfo {
            current_version: "0.0.3".to_string(),
            version: "0.0.4".to_string(),
            notes: Some("修复若干问题".to_string()),
            date: Some("2026-12-01T00:00:00Z".to_string()),
            target: "linux-x86_64-appimage".to_string(),
            download_size: None,
        };

        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            serde_json::json!({
                "currentVersion": "0.0.3",
                "version": "0.0.4",
                "notes": "修复若干问题",
                "date": "2026-12-01T00:00:00Z",
                "target": "linux-x86_64-appimage",
                "downloadSize": null,
            })
        );
    }

    #[test]
    fn progress_and_result_serialize_with_camel_case() {
        let progress = AppUpdateProgress {
            phase: "downloading",
            downloaded: 1024,
            total: Some(4096),
        };
        assert_eq!(
            serde_json::to_value(&progress).unwrap(),
            serde_json::json!({
                "phase": "downloading",
                "downloaded": 1024,
                "total": 4096,
            })
        );

        let result = AppUpdateResult {
            install_started: true,
            version: "0.0.4".to_string(),
            restart_required: true,
        };
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::json!({
                "installStarted": true,
                "version": "0.0.4",
                "restartRequired": true,
            })
        );
    }
}

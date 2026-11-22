//! command 层：把核心配置模块接到 Tauri 上，只做路径注入与错误映射。

use crate::auto_sync;
use crate::config::{
    self, AppConfig, ConfigError, ConfigValidation, Transport, AGENT_DIR_ENV_VAR, CONFIG_FILE_NAME,
};
use crate::credentials::{self, CredentialError, WebdavCredentials};
use crate::runtime;
use crate::snapshot;
use crate::store::{LocalStore, PackageStore, WebdavStore};
use crate::upload::{self, UploadFailure, UploadView};
use crate::webdav::{self, DavError, ReqwestTransport, WebdavClient};
use crate::webdav_transfer::{self, DownloadFailure, StagedPackage};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

/// 一次配置读取的完整结果：配置内容、生效路径与校验结果。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfigView {
    config: AppConfig,
    validation: ConfigValidation,
    config_file_path: PathBuf,
}

/// 配置读写所需的外部路径，由 command 层负责注入。
struct ConfigContext {
    config_dir: PathBuf,
    config_file: PathBuf,
    env_agent_dir: Option<PathBuf>,
    home_dir: Option<PathBuf>,
}

fn context(app: &AppHandle) -> Result<ConfigContext, String> {
    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("无法定位应用配置目录：{error}"))?;

    Ok(ConfigContext {
        config_file: config_dir.join(CONFIG_FILE_NAME),
        config_dir,
        // 只读取这一个文档化的变量；错误信息里不回显环境变量内容。
        env_agent_dir: std::env::var_os(AGENT_DIR_ENV_VAR).map(PathBuf::from),
        home_dir: app.path().home_dir().ok(),
    })
}

fn view(config: AppConfig, validation: ConfigValidation, config_file: &Path) -> AppConfigView {
    AppConfigView {
        config,
        validation,
        config_file_path: config_file.to_path_buf(),
    }
}

fn readable(error: ConfigError) -> String {
    error.to_string()
}

/// WebDAV 凭据的摘要。只回答「存了什么地址与用户名」和「是否已有密码」，
/// 密码本身永不进入 IPC 返回值。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebdavSummary {
    url: String,
    username: String,
    has_password: bool,
}

fn webdav_summary(credentials: Option<&WebdavCredentials>) -> WebdavSummary {
    match credentials {
        Some(credentials) => WebdavSummary {
            url: credentials.url.clone(),
            username: credentials.username.clone(),
            has_password: !credentials.password.is_empty(),
        },
        None => WebdavSummary {
            url: String::new(),
            username: String::new(),
            has_password: false,
        },
    }
}

fn credential_error(error: CredentialError) -> String {
    error.to_string()
}

#[tauri::command]
pub fn load_webdav_summary(app: AppHandle) -> Result<WebdavSummary, String> {
    let dir = app_data_dir(&app)?;
    let credentials = credentials::load(&dir).map_err(credential_error)?;

    Ok(webdav_summary(credentials.as_ref()))
}

/// 保存凭据。密码缺省或空串表示保留原值，因此换地址不必重输密码。
#[tauri::command]
pub fn save_webdav_credentials(
    app: AppHandle,
    url: String,
    username: String,
    password: Option<String>,
) -> Result<WebdavSummary, String> {
    let dir = app_data_dir(&app)?;
    let saved = credentials::upsert(&dir, &url, &username, password.as_deref())
        .map_err(credential_error)?;

    Ok(webdav_summary(Some(&saved)))
}

/// 连接探测的结论。只回答「目录是否本次新建」与「写测试文件是否残留」。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebdavProbeView {
    created: bool,
    leftover_write_test_file: bool,
}

/// 协议错误 → 设置页能用的一句话。设置页只展示文字，不再造一层错误码。
fn dav_error(error: DavError) -> String {
    match error {
        DavError::AuthFailed => "认证失败：凭据无效或权限不足".to_string(),
        DavError::Unreachable(detail) => format!("无法连接：{detail}"),
        DavError::Quota => "远端空间或流量不足，无法写入测试文件".to_string(),
        DavError::PreconditionFailed => {
            "远端在本次操作期间被改写：条件请求未通过，请重新尝试".to_string()
        }
        DavError::Other(detail) => detail,
    }
}

/// 凭据模块的文案是给保存流程写的：探测时用户可能只是还没填，换成测试场景的说法。
fn probe_credential_error(error: CredentialError) -> String {
    match error {
        CredentialError::MissingPassword => "还没有保存过 WebDAV 密码，请填写后再测试".to_string(),
        other => other.to_string(),
    }
}

/// 用当前表单值试一次：不保存凭据；密码留空时借用已存值，与保存语义一致。
#[tauri::command]
pub async fn test_webdav_connection(
    app: AppHandle,
    url: String,
    username: String,
    password: Option<String>,
) -> Result<WebdavProbeView, String> {
    // 控制超时 30s、传输超时 120s 都是阻塞等待，留在 UI 线程会冻住窗口。
    tauri::async_runtime::spawn_blocking(move || {
        let dir = app_data_dir(&app)?;
        let credentials = credentials::resolve(&dir, &url, &username, password.as_deref())
            .map_err(probe_credential_error)?;
        let transport = ReqwestTransport::new(&credentials.username, &credentials.password)
            .map_err(dav_error)?;
        let outcome =
            webdav::probe(&WebdavClient::new(transport, &credentials.url)).map_err(dav_error)?;

        Ok(WebdavProbeView {
            created: outcome.created,
            leftover_write_test_file: outcome.leftover_write_test_file,
        })
    })
    .await
    .map_err(|error| format!("测试连接失败：{error}"))?
}

/// 读取应用配置。
fn app_config(app: &AppHandle) -> Result<AppConfig, String> {
    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("无法定位应用配置目录：{error}"))?;

    config::load_config(&config_dir).map_err(|error| error.to_string())
}

/// 本地文件夹模式的存储根。WebDAV 的包列表还没接上：那条路只支持取最新一份。
fn local_store(app: &AppHandle) -> Result<LocalStore, String> {
    let config = app_config(app)?;
    if config.transport != Transport::Local {
        return Err("WebDAV 模式暂时只支持取最新一份：历史包列表请等后续版本".to_string());
    }
    let root = config
        .sync_root
        .ok_or_else(|| "还没有设置同步文件夹：请先在设置页选择，再回来导出或恢复".to_string())?;

    Ok(LocalStore::new(root))
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| format!("无法定位应用数据目录：{error}"))
}

/// 存储根里的一个包。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageEntryView {
    name: String,
    size: u64,
    /// RFC 3339 UTC，由包名里的时间戳换算。
    created_at: String,
}

/// 一次列举的结果。`newest` 是默认选中项：恢复时先挑名字最大的一份。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageListView {
    packages: Vec<PackageEntryView>,
    newest: Option<String>,
    /// 无法识别的文件（`.tmp-*`、云盘冲突副本等）：只展示，不可操作。
    unrecognized: Vec<String>,
}

/// 列举存储根里的快照包，按名字倒序（最新在前）。
#[tauri::command]
pub fn list_snapshot_packages(app: AppHandle) -> Result<PackageListView, String> {
    let listing = local_store(&app)?.list().map_err(|error| error.message)?;
    let mut packages: Vec<PackageEntryView> = listing
        .packages
        .into_iter()
        .map(|entry| PackageEntryView {
            name: entry.name,
            size: entry.size,
            created_at: snapshot::format_timestamp(entry.created_at),
        })
        .collect();
    packages.sort_by(|left, right| right.name.cmp(&left.name));
    let newest = packages.first().map(|entry| entry.name.clone());

    Ok(PackageListView {
        packages,
        newest,
        unrecognized: listing.unrecognized,
    })
}

/// 取包到本机暂存的结果。`stagingRoot` 顶替同步根，交给 `verify_and_plan` 与 `apply_snapshot`。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadView {
    package: String,
    /// 包文件字节数。
    size: u64,
    /// 包内快照文件数（不含 manifest）。
    files: usize,
    fingerprint: String,
    created_at: String,
    staging_root: PathBuf,
    /// 上一次成功上传/下载时看到的远端时间；本地文件夹模式没有远端，为 null。
    previous_seen_at: Option<String>,
}

impl DownloadView {
    fn new(staged: StagedPackage, previous_seen_at: Option<String>) -> Self {
        Self {
            package: staged.package_name,
            size: staged.size,
            files: staged.file_count,
            fingerprint: staged.fingerprint,
            created_at: staged.created_at,
            staging_root: staged.staging_root,
            previous_seen_at,
        }
    }
}

/// 取一份包到本机暂存目录；`package` 缺省时取名字最大的一份。
///
/// 本地文件夹模式走本地存储根，WebDAV 模式走 [`webdav_transfer::download_from_webdav`]；
/// 之后完全复用 `10-snapshot-sync.md` 第 6 节的校验 → 计划 → 备份 → 应用，本命令不写 live。
#[tauri::command]
pub async fn download_remote_snapshot(
    app: AppHandle,
    package: Option<String>,
) -> Result<DownloadView, DownloadFailure> {
    // 取包与解包都是阻塞 IO，留在 UI 线程会冻住窗口。
    tauri::async_runtime::spawn_blocking(move || {
        // 与自动上行、应用/回滚共用同一把锁，避免两套写盘同时跑。
        let _guard = runtime::lock_operation();
        let app_data = app_data_dir(&app)
            .map_err(|message| DownloadFailure::new("app_data_unavailable", message))?;
        let config = app_config(&app)
            .map_err(|message| DownloadFailure::new("app_config_unavailable", message))?;

        if config.transport == Transport::Local {
            let root = config.sync_root.ok_or_else(|| {
                DownloadFailure::new(
                    "sync_root_unset",
                    "还没有设置同步文件夹：请先在设置页选择，再回来取包",
                )
            })?;
            let staged = webdav_transfer::stage_package(
                &LocalStore::new(root),
                &app_data,
                package.as_deref(),
            )?;

            return Ok(DownloadView::new(staged, None));
        }

        let credentials = credentials::load(&app_data)
            .map_err(|error| DownloadFailure::new("remote_not_configured", error.to_string()))?
            .ok_or_else(|| {
                DownloadFailure::new(
                    "remote_not_configured",
                    "还没有保存 WebDAV 凭据：请先在设置页填写地址、用户名与密码",
                )
            })?;
        let transport = ReqwestTransport::new(&credentials.username, &credentials.password)
            .map_err(|error| DownloadFailure::new("remote_unreachable", error.to_string()))?;
        let client = WebdavClient::new(transport, &credentials.url);
        let outcome = webdav_transfer::download_from_webdav(
            &WebdavStore::new(&client),
            &credentials.url,
            &app_data,
            package.as_deref(),
        )?;

        Ok(DownloadView::new(outcome.staged, outcome.previous_seen_at))
    })
    .await
    .map_err(|error| DownloadFailure::new("download_failed", format!("取快照包失败：{error}")))?
}

/// 上传：本地模式把 live 导出成包放进存储根（由云盘客户端负责搬运）；WebDAV 模式走
/// [`upload::upload_to_webdav`]，与自动上行同一条链路。
#[tauri::command]
pub async fn upload_snapshot(app: AppHandle) -> Result<UploadView, UploadFailure> {
    // 导出要遍历、扫描、打包，留在 UI 线程会冻住窗口。
    tauri::async_runtime::spawn_blocking(move || {
        let config = app_config(&app)
            .map_err(|message| UploadFailure::new("app_config_unavailable", message))?;
        if config.transport == Transport::Local {
            let report = snapshot::export_to_local_store(&app)
                .map_err(|failure| UploadFailure::new(failure.code, failure.message))?;
            return Ok(UploadView::from_export(report));
        }

        // 与自动上行、应用/回滚共用一把锁：导出会重建工作区，不能两份同时跑。
        let _guard = runtime::lock_operation();
        // 手动上传是用户显式动作：按设计包含根级 auth.json。
        upload::upload_from_app(&app, true).map(|outcome| outcome.view)
    })
    .await
    .map_err(|error| UploadFailure::new("upload_failed", format!("上传失败：{error}")))?
}

#[tauri::command]
pub fn load_app_config(app: AppHandle) -> Result<AppConfigView, String> {
    let context = context(&app)?;
    let config = config::load_config(&context.config_dir).map_err(readable)?;
    let validation = config::validate(
        &config,
        context.env_agent_dir.as_deref(),
        context.home_dir.as_deref(),
    );

    Ok(view(config, validation, &context.config_file))
}

#[tauri::command]
pub fn validate_app_config(app: AppHandle, config: AppConfig) -> Result<AppConfigView, String> {
    let context = context(&app)?;
    let normalized = config.normalized();
    let validation = config::validate(
        &normalized,
        context.env_agent_dir.as_deref(),
        context.home_dir.as_deref(),
    );

    Ok(view(normalized, validation, &context.config_file))
}

#[tauri::command]
pub fn save_app_config(app: AppHandle, config: AppConfig) -> Result<AppConfigView, String> {
    let context = context(&app)?;
    let launch_at_login = config.launch_at_login;
    let previous_launch_at_login = config::load_config(&context.config_dir)
        .map(|current| current.launch_at_login)
        .ok();
    let launch_changed = previous_launch_at_login != Some(launch_at_login);
    if launch_changed {
        runtime::set_launch_at_login(&app, launch_at_login)?;
    }
    let saved = match config::save_config_checked(
        &context.config_dir,
        &config,
        context.env_agent_dir.as_deref(),
        context.home_dir.as_deref(),
    ) {
        Ok(saved) => saved,
        Err(error) => {
            // 配置没写成时把系统登记改回去，避免设置页显示与实际相反。
            if launch_changed {
                if let Some(previous) = previous_launch_at_login {
                    let _ = runtime::set_launch_at_login(&app, previous);
                }
            }
            return Err(readable(error));
        }
    };
    let (saved, validation) = saved;

    auto_sync::wake();

    Ok(view(saved, validation, &context.config_file))
}

/// 托盘/设置页读取自动上行状态。
#[tauri::command]
pub fn auto_sync_status() -> auto_sync::AutoSyncStatus {
    auto_sync::status()
}

/// 设置页的「立即同步」：唤醒后台线程，不等待上传完成。
#[tauri::command]
pub fn trigger_auto_sync() -> auto_sync::AutoSyncStatus {
    auto_sync::trigger_now();
    auto_sync::status()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 前端按 camelCase 字段消费；形状一变就会静默拿不到数据。
    #[test]
    fn download_view_serializes_with_camel_case() {
        let view = DownloadView {
            package: "20261122-105230-deadbeef.zip".to_string(),
            size: 4096,
            files: 7,
            fingerprint: "deadbeefcafe".to_string(),
            created_at: "2026-11-22T10:52:30Z".to_string(),
            staging_root: PathBuf::from("/data/staging/20261122-105230-deadbeef.zip"),
            previous_seen_at: Some("2026-11-21T09:12:10Z".to_string()),
        };
        assert_eq!(
            serde_json::to_value(&view).unwrap(),
            serde_json::json!({
                "package": "20261122-105230-deadbeef.zip",
                "size": 4096,
                "files": 7,
                "fingerprint": "deadbeefcafe",
                "createdAt": "2026-11-22T10:52:30Z",
                "stagingRoot": "/data/staging/20261122-105230-deadbeef.zip",
                "previousSeenAt": "2026-11-21T09:12:10Z",
            })
        );
    }

    /// 取包失败的码要能过 IPC：前端靠它区分「远端空」与「包被删」。
    #[test]
    fn download_failure_carries_code_and_message() {
        let failure = DownloadFailure::new("remote_no_packages", "还没有快照包");
        assert_eq!(
            serde_json::to_value(&failure).unwrap(),
            serde_json::json!({
                "code": "remote_no_packages",
                "message": "还没有快照包",
            })
        );
    }

    #[test]
    fn package_list_serializes_with_camel_case() {
        let list = PackageListView {
            packages: vec![PackageEntryView {
                name: "20261122-105230-deadbeef.zip".to_string(),
                size: 4096,
                created_at: "2026-11-22T10:52:30Z".to_string(),
            }],
            newest: Some("20261122-105230-deadbeef.zip".to_string()),
            unrecognized: vec!["notes.txt".to_string()],
        };
        assert_eq!(
            serde_json::to_value(&list).unwrap(),
            serde_json::json!({
                "packages": [{
                    "name": "20261122-105230-deadbeef.zip",
                    "size": 4096,
                    "createdAt": "2026-11-22T10:52:30Z",
                }],
                "newest": "20261122-105230-deadbeef.zip",
                "unrecognized": ["notes.txt"],
            })
        );
    }

    /// 失败码与文案要能过 IPC。
    #[test]
    fn upload_failure_carries_code_and_message() {
        let failure = UploadFailure::new("remote_insecure", "WebDAV 地址必须是 HTTPS");
        assert_eq!(
            serde_json::to_value(&failure).unwrap(),
            serde_json::json!({
                "code": "remote_insecure",
                "message": "WebDAV 地址必须是 HTTPS",
            })
        );
    }

    /// 上传结果是包语义：前端按这些字段渲染「已清理 N 份」「有 N 份清理失败」。
    #[test]
    fn upload_view_serializes_with_camel_case() {
        let view = UploadView {
            package: "20261122-105230-deadbeef.zip".to_string(),
            unchanged: false,
            pruned: vec!["20261121-091210-7f3e9c01.zip".to_string()],
            prune_failed: vec![upload::PruneFailure {
                name: "20261120-091210-7f3e9c01.zip".to_string(),
                reason: "远端拒绝删除".to_string(),
            }],
        };
        assert_eq!(
            serde_json::to_value(&view).unwrap(),
            serde_json::json!({
                "package": "20261122-105230-deadbeef.zip",
                "unchanged": false,
                "pruned": ["20261121-091210-7f3e9c01.zip"],
                "pruneFailed": [{
                    "name": "20261120-091210-7f3e9c01.zip",
                    "reason": "远端拒绝删除",
                }],
            })
        );
    }
}

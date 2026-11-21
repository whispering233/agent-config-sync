//! command 层：把核心配置模块接到 Tauri 上，只做路径注入与错误映射。

use crate::config::{
    self, AppConfig, ConfigError, ConfigValidation, AGENT_DIR_ENV_VAR, CONFIG_FILE_NAME,
};
use crate::credentials::{self, CredentialError, WebdavCredentials};
use crate::doctor::{self, ShellRunner};
use crate::platform::HostOs;
use crate::secrets::SecretFinding;
use crate::snapshot;
use crate::webdav::{self, DavError, ReqwestTransport, WebdavClient};
use crate::webdav_transfer::{self, TransferError, UploadOutcome};
use std::path::PathBuf;
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

fn view(config: AppConfig, validation: ConfigValidation, config_file: &PathBuf) -> AppConfigView {
    AppConfigView {
        config,
        validation,
        config_file_path: config_file.clone(),
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

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| format!("无法定位应用数据目录：{error}"))
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

/// 一次下载的结果。`stagingRoot` 顶替同步根，交给 `verify_and_plan` 与 `apply_snapshot`。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadView {
    staging_root: PathBuf,
    fingerprint: String,
    file_count: usize,
    created_at: String,
    /// 本次下载之前记录的同步时间；本机没有记录时为 null。
    previous_seen_at: Option<String>,
}

/// 凭据缺失就是「还没配置」：这句话要能直接指路，不能只报文件不存在。
fn download_credential_error(error: CredentialError) -> String {
    match error {
        CredentialError::Read { .. } | CredentialError::Corrupt { .. } => error.to_string(),
        other => format!("还没有可用的 WebDAV 凭据：{other}；请先在设置页填写地址、用户名与密码"),
    }
}

/// 网络类错误沿用设置页那套中文说法，其余直接用下载模块的说明。
fn transfer_error(error: TransferError) -> String {
    match error {
        TransferError::Network(dav) => dav_error(dav),
        other => other.to_string(),
    }
}

/// 按远端 manifest 把快照取到本机暂存目录。不写 live，不碰同步文件夹。
#[tauri::command]
pub async fn download_remote_snapshot(app: AppHandle) -> Result<DownloadView, String> {
    // 单文件传输超时 120s 是阻塞等待，留在 UI 线程会冻住窗口。
    tauri::async_runtime::spawn_blocking(move || {
        let dir = app_data_dir(&app)?;
        let credentials = credentials::load(&dir)
            .map_err(download_credential_error)?
            .ok_or_else(|| "还没有配置 WebDAV：请先在设置页填写地址、用户名与密码".to_string())?;
        let transport = ReqwestTransport::new(&credentials.username, &credentials.password)
            .map_err(dav_error)?;
        let outcome =
            webdav_transfer::download(&WebdavClient::new(transport, &credentials.url), &dir)
                .map_err(transfer_error)?;

        Ok(DownloadView {
            staging_root: outcome.staging_root,
            fingerprint: outcome.manifest.fingerprint,
            file_count: outcome.manifest.files.len(),
            created_at: outcome.manifest.created_at,
            previous_seen_at: outcome.previous_seen_at,
        })
    })
    .await
    .map_err(|error| format!("下载失败：{error}"))?
}

/// 一次上传的结局。前端按 `kind` 区分「已上传」与「需要用户先选一边」。
#[derive(Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UploadView {
    #[serde(rename_all = "camelCase")]
    Uploaded {
        fingerprint: String,
        uploaded: usize,
        skipped: usize,
        /// 本次上传后记录的远端状态时间；前端直接拿它刷「上次同步时间」。
        seen_at: String,
    },
    #[serde(rename_all = "camelCase")]
    Conflict {
        remote_fingerprint: String,
        added: Vec<String>,
        modified: Vec<String>,
        removed: Vec<String>,
        /// 远端清单损坏：只能覆盖，无法先下载。
        remote_corrupt: bool,
    },
}

/// 上传失败。码沿用 `40-webdav.md` §8；`secrets_unconfirmed` 与导出流程同义。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadFailure {
    code: &'static str,
    message: String,
    /// 只有明文密钥阻断才会带待确认条目。
    findings: Vec<SecretFinding>,
}

fn upload_failure(code: &'static str, message: String) -> UploadFailure {
    UploadFailure {
        code,
        message,
        findings: Vec::new(),
    }
}

/// 失败码：先定码，再取文案（导出失败有自己的码，其余按传输层分类）。
fn upload_failure_code(error: &TransferError) -> &'static str {
    match error {
        TransferError::Export(failure) => failure.code,
        TransferError::NotFound => "remote_not_found",
        TransferError::Corrupt(_) => "remote_manifest_corrupt",
        TransferError::RemoteChanged => "remote_changed",
        TransferError::Network(DavError::AuthFailed) => "remote_auth_failed",
        TransferError::Network(DavError::Quota) => "remote_quota",
        TransferError::Network(_) => "remote_unreachable",
        _ => "upload_failed",
    }
}

fn upload_failure_from(error: TransferError) -> UploadFailure {
    let code = upload_failure_code(&error);
    match error {
        TransferError::Export(failure) => UploadFailure {
            code,
            message: failure.message,
            findings: failure.findings,
        },
        other => upload_failure(code, transfer_error(other)),
    }
}

/// 把当前 live 导出并推到 WebDAV。冲突时远端一个字节都不写，等用户选「覆盖」或「从云端更新」。
#[tauri::command]
pub async fn upload_snapshot(
    app: AppHandle,
    overwrite: bool,
    allow_plaintext: Option<bool>,
) -> Result<UploadView, UploadFailure> {
    // 导出、逐文件 PUT 与 120s 单文件超时都是阻塞等待，留在 UI 线程会冻住窗口。
    tauri::async_runtime::spawn_blocking(move || {
        let config_dir = app.path().app_config_dir().map_err(|error| {
            upload_failure(
                "app_config_unavailable",
                format!("无法定位应用配置目录：{error}"),
            )
        })?;
        let config = config::load_config(&config_dir)
            .map_err(|error| upload_failure("app_config_unavailable", error.to_string()))?;
        // 只读取这一个文档化的变量；错误信息里不回显其内容。
        let env_agent_dir = std::env::var_os(AGENT_DIR_ENV_VAR).map(PathBuf::from);
        let home_dir = app.path().home_dir().ok();
        let agent_dir =
            config::resolve_agent_dir(&config, env_agent_dir.as_deref(), home_dir.as_deref())
                .path
                .ok_or_else(|| {
                    upload_failure(
                        "agent_dir_missing",
                        "无法确定宿主目录，请先在设置页填写".to_string(),
                    )
                })?;

        let dir =
            app_data_dir(&app).map_err(|error| upload_failure("app_data_unavailable", error))?;
        let credentials = credentials::load(&dir)
            .map_err(|error| {
                upload_failure("remote_not_configured", download_credential_error(error))
            })?
            .ok_or_else(|| {
                upload_failure(
                    "remote_not_configured",
                    "还没有配置 WebDAV：请先在设置页填写地址、用户名与密码".to_string(),
                )
            })?;
        let transport = ReqwestTransport::new(&credentials.username, &credentials.password)
            .map_err(|error| upload_failure("remote_unreachable", dav_error(error)))?;
        // 与本地导出同一份探测：manifest 的 source 记下这次上传来自哪个宿主版本。
        let (pi, node) = doctor::probe_versions(
            Some(&agent_dir),
            &ShellRunner::from_env(),
            HostOs::current(),
        );
        let versions = snapshot::SourceVersions { pi, node };
        let outcome = webdav_transfer::upload(
            &WebdavClient::new(transport, &credentials.url),
            &dir,
            &agent_dir,
            &versions,
            allow_plaintext.unwrap_or(false),
            overwrite,
        )
        .map_err(upload_failure_from)?;

        Ok(match outcome {
            UploadOutcome::Uploaded {
                fingerprint,
                uploaded,
                skipped,
                seen_at,
            } => UploadView::Uploaded {
                fingerprint,
                uploaded,
                skipped,
                seen_at,
            },
            UploadOutcome::Conflict {
                remote_fingerprint,
                added,
                modified,
                removed,
                remote_corrupt,
            } => UploadView::Conflict {
                remote_fingerprint,
                added,
                modified,
                removed,
                remote_corrupt,
            },
        })
    })
    .await
    .map_err(|error| upload_failure("upload_failed", format!("上传失败：{error}")))?
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
    let (saved, validation) = config::save_config_checked(
        &context.config_dir,
        &config,
        context.env_agent_dir.as_deref(),
        context.home_dir.as_deref(),
    )
    .map_err(readable)?;

    Ok(view(saved, validation, &context.config_file))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 前端按 `kind` 与 camelCase 字段消费；形状一变就会静默拿不到数据。
    #[test]
    fn upload_view_serializes_with_kind_and_camel_case() {
        let uploaded = UploadView::Uploaded {
            fingerprint: "abc123".to_string(),
            uploaded: 2,
            skipped: 3,
            seen_at: "2026-11-20T05:30:00Z".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&uploaded).unwrap(),
            serde_json::json!({
                "kind": "uploaded",
                "fingerprint": "abc123",
                "uploaded": 2,
                "skipped": 3,
                "seenAt": "2026-11-20T05:30:00Z",
            })
        );

        let conflict = UploadView::Conflict {
            remote_fingerprint: "def456".to_string(),
            added: vec!["snapshot/a.md".to_string()],
            modified: Vec::new(),
            removed: Vec::new(),
            remote_corrupt: false,
        };
        assert_eq!(
            serde_json::to_value(&conflict).unwrap(),
            serde_json::json!({
                "kind": "conflict",
                "remoteFingerprint": "def456",
                "added": ["snapshot/a.md"],
                "modified": [],
                "removed": [],
                "remoteCorrupt": false,
            })
        );
    }

    /// 明文密钥阻断要能过 IPC：码与待确认条目都得带上。
    #[test]
    fn upload_failure_carries_code_and_findings() {
        let failure = upload_failure("remote_changed", "远端被改写".to_string());
        assert_eq!(
            serde_json::to_value(&failure).unwrap(),
            serde_json::json!({
                "code": "remote_changed",
                "message": "远端被改写",
                "findings": [],
            })
        );
    }
}

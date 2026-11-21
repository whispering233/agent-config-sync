//! command 层：把核心配置模块接到 Tauri 上，只做路径注入与错误映射。

use crate::config::{
    self, AppConfig, ConfigError, ConfigValidation, AGENT_DIR_ENV_VAR, CONFIG_FILE_NAME,
};
use crate::credentials::{self, CredentialError, WebdavCredentials};
use crate::webdav::{self, DavError, ReqwestTransport, WebdavClient};
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

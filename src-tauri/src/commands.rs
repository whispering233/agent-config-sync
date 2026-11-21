//! command 层：把核心配置模块接到 Tauri 上，只做路径注入与错误映射。

use crate::config::{
    self, AppConfig, ConfigError, ConfigValidation, AGENT_DIR_ENV_VAR, CONFIG_FILE_NAME,
};
use crate::credentials::{self, CredentialError, WebdavCredentials};
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

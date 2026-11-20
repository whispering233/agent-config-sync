//! 应用配置：同步根目录、宿主目录覆盖与备份保留数。
//!
//! 核心层模块：不依赖 Tauri，可在临时目录里单测。校验规则只在这里实现一次，
//! command 层与 UI 都只消费 [`validate`] 返回的结构化结果。

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

/// 应用配置目录下的文件名。
pub const CONFIG_FILE_NAME: &str = "config.json";
/// 本应用写出的配置文件版本。
pub const CONFIG_VERSION: u32 = 1;
/// 备份保留数的默认值。
pub const DEFAULT_BACKUP_RETENTION: u32 = 10;
/// 备份保留数下限。
pub const MIN_BACKUP_RETENTION: u32 = 1;
/// 备份保留数上限。
pub const MAX_BACKUP_RETENTION: u32 = 100;
/// 宿主目录的第二优先级来源（优先级：配置覆盖 > 该环境变量 > `~/.pi/agent`）。
pub const AGENT_DIR_ENV_VAR: &str = "PI_CODING_AGENT_DIR";

/// 应用配置的内容。字段名会以 camelCase 序列化给前端。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub version: u32,
    pub sync_root: Option<PathBuf>,
    pub agent_dir_override: Option<PathBuf>,
    #[serde(default = "default_backup_retention")]
    pub backup_retention: u32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            sync_root: None,
            agent_dir_override: None,
            backup_retention: DEFAULT_BACKUP_RETENTION,
        }
    }
}

impl AppConfig {
    /// 去掉路径两端的空白，并把空字符串视为「未设置」。
    pub fn normalized(&self) -> Self {
        Self {
            version: self.version,
            sync_root: normalize_optional_path(self.sync_root.as_deref()),
            agent_dir_override: normalize_optional_path(self.agent_dir_override.as_deref()),
            backup_retention: self.backup_retention,
        }
    }
}

fn default_backup_retention() -> u32 {
    DEFAULT_BACKUP_RETENTION
}

fn normalize_optional_path(value: Option<&Path>) -> Option<PathBuf> {
    let path = value?;
    let text = path.as_os_str().to_string_lossy();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// 单个路径的状态，供 UI 直接着色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathStatus {
    /// 未设置（同步根目录留空 / 宿主目录无法确定）。
    Unset,
    /// 路径合法且可写入。
    Ok,
    /// 路径本身不合法（相对路径、含 `..`、不是文件夹、不可读）。
    Invalid,
    /// 路径合法但目录不存在。
    Missing,
    /// 目录存在但不可写。
    NotWritable,
}

/// 校验项的严重程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingLevel {
    /// 阻断保存。
    Error,
    /// 允许保存，但需要提醒。
    Warning,
}

/// 校验项归属的配置字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConfigField {
    SyncRoot,
    AgentDir,
    BackupRetention,
}

/// 一条校验结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationFinding {
    pub level: FindingLevel,
    pub field: ConfigField,
    pub code: &'static str,
    pub message: String,
}

/// 生效宿主目录的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDirSource {
    /// 来自配置里的宿主目录覆盖。
    Override,
    /// 来自环境变量 `PI_CODING_AGENT_DIR`。
    Env,
    /// 来自 `~/.pi/agent`。
    Default,
    /// 无法确定（拿不到家目录或覆盖值非法）。
    Unresolved,
}

/// 宿主目录的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgentDir {
    pub path: Option<PathBuf>,
    pub source: AgentDirSource,
}

/// 一次校验的完整结果：路径状态、校验项与生效路径。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigValidation {
    /// 没有任何 `error` 级校验项时为 true。
    pub ok: bool,
    pub findings: Vec<ValidationFinding>,
    pub sync_root_status: PathStatus,
    pub agent_dir_status: PathStatus,
    pub effective_agent_dir: Option<PathBuf>,
    pub agent_dir_source: AgentDirSource,
}

impl ConfigValidation {
    /// 阻断保存的校验项。
    pub fn errors(&self) -> Vec<ValidationFinding> {
        self.findings
            .iter()
            .filter(|finding| finding.level == FindingLevel::Error)
            .cloned()
            .collect()
    }
}

/// 读写配置文件时可能出现的可读错误。
#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Write {
        path: PathBuf,
        source: io::Error,
    },
    Corrupt {
        path: PathBuf,
        detail: String,
    },
    UnsupportedVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    Validation(Vec<ValidationFinding>),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read { path, source } => {
                write!(formatter, "无法读取应用配置 {}：{}", path.display(), source)
            }
            ConfigError::Write { path, source } => {
                write!(formatter, "无法保存应用配置 {}：{}", path.display(), source)
            }
            ConfigError::Corrupt { path, detail } => write!(
                formatter,
                "应用配置格式损坏（{}）：{}；原文件未改动",
                path.display(),
                detail
            ),
            ConfigError::UnsupportedVersion {
                path,
                found,
                supported,
            } => write!(
                formatter,
                "应用配置版本不受支持（{}）：文件为 v{}，本应用支持 v{}；原文件未改动",
                path.display(),
                found,
                supported
            ),
            ConfigError::Validation(findings) => {
                write!(formatter, "配置未通过校验：")?;
                for (index, finding) in findings.iter().enumerate() {
                    if index > 0 {
                        write!(formatter, "；")?;
                    }
                    write!(formatter, "{}", finding.message)?;
                }
                Ok(())
            }
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ConfigError::Read { source, .. } | ConfigError::Write { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// 读取配置。文件不存在时返回默认值；损坏或版本不认识时报错且不写回。
pub fn load_config(config_dir: &Path) -> Result<AppConfig, ConfigError> {
    let path = config_dir.join(CONFIG_FILE_NAME);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(AppConfig::default()),
        Err(source) => return Err(ConfigError::Read { path, source }),
    };

    let parsed: AppConfig = serde_json::from_str(&contents).map_err(|error| ConfigError::Corrupt {
        path: path.clone(),
        detail: error.to_string(),
    })?;

    if parsed.version > CONFIG_VERSION {
        return Err(ConfigError::UnsupportedVersion {
            path,
            found: parsed.version,
            supported: CONFIG_VERSION,
        });
    }

    Ok(parsed.normalized())
}

/// 保存配置：同目录临时文件 + rename 原子替换，POSIX 下写 0600。
pub fn save_config(config_dir: &Path, config: &AppConfig) -> Result<AppConfig, ConfigError> {
    fs::create_dir_all(config_dir).map_err(|source| ConfigError::Write {
        path: config_dir.to_path_buf(),
        source,
    })?;

    let mut normalized = config.normalized();
    normalized.version = CONFIG_VERSION;

    let target = config_dir.join(CONFIG_FILE_NAME);
    let temp = config_dir.join(format!("{CONFIG_FILE_NAME}.tmp"));

    let mut contents =
        serde_json::to_string_pretty(&normalized).map_err(|error| ConfigError::Write {
            path: target.clone(),
            source: io::Error::new(io::ErrorKind::InvalidData, error.to_string()),
        })?;
    contents.push('\n');

    write_private_file(&temp, contents.as_bytes()).map_err(|source| ConfigError::Write {
        path: temp.clone(),
        source,
    })?;

    if let Err(source) = fs::rename(&temp, &target) {
        let _ = fs::remove_file(&temp);
        return Err(ConfigError::Write {
            path: target,
            source,
        });
    }

    Ok(normalized)
}

/// 先校验再保存：有 `error` 级校验项时不写文件。
pub fn save_config_checked(
    config_dir: &Path,
    config: &AppConfig,
    env_agent_dir: Option<&Path>,
    home_dir: Option<&Path>,
) -> Result<(AppConfig, ConfigValidation), ConfigError> {
    let normalized = config.normalized();
    let validation = validate(&normalized, env_agent_dir, home_dir);
    if !validation.ok {
        return Err(ConfigError::Validation(validation.errors()));
    }

    let saved = save_config(config_dir, &normalized)?;
    let validation = validate(&saved, env_agent_dir, home_dir);
    Ok((saved, validation))
}

/// 生效宿主目录：配置覆盖 → 环境变量 → `~/.pi/agent`。
///
/// 环境变量与家目录由调用方注入，核心层不读取进程环境，便于单测。
pub fn resolve_agent_dir(
    config: &AppConfig,
    env_agent_dir: Option<&Path>,
    home_dir: Option<&Path>,
) -> ResolvedAgentDir {
    if let Some(override_dir) = normalize_optional_path(config.agent_dir_override.as_deref()) {
        return ResolvedAgentDir {
            path: Some(override_dir),
            source: AgentDirSource::Override,
        };
    }

    if let Some(env_dir) = normalize_optional_path(env_agent_dir) {
        return ResolvedAgentDir {
            path: Some(env_dir),
            source: AgentDirSource::Env,
        };
    }

    match normalize_optional_path(home_dir) {
        Some(home) => ResolvedAgentDir {
            path: Some(home.join(".pi").join("agent")),
            source: AgentDirSource::Default,
        },
        None => ResolvedAgentDir {
            path: None,
            source: AgentDirSource::Unresolved,
        },
    }
}

/// 校验配置并返回结构化结果。`error` 阻断保存，`warning` 只提醒。
pub fn validate(
    config: &AppConfig,
    env_agent_dir: Option<&Path>,
    home_dir: Option<&Path>,
) -> ConfigValidation {
    let config = config.normalized();
    let mut findings = Vec::new();

    if !(MIN_BACKUP_RETENTION..=MAX_BACKUP_RETENTION).contains(&config.backup_retention) {
        findings.push(ValidationFinding {
            level: FindingLevel::Error,
            field: ConfigField::BackupRetention,
            code: "backup_retention_out_of_range",
            message: format!(
                "备份保留数必须在 {MIN_BACKUP_RETENTION}–{MAX_BACKUP_RETENTION} 之间（当前 {}），请在配置文件中修改",
                config.backup_retention
            ),
        });
    }

    let sync_root_status = match config.sync_root.as_deref() {
        None => PathStatus::Unset,
        Some(sync_root) => {
            let inspection = inspect_path(sync_root, true);
            if let Some(issue) = inspection.issue {
                findings.push(issue.to_finding(ConfigField::SyncRoot));
            }
            inspection.status
        }
    };

    let resolved = resolve_agent_dir(&config, env_agent_dir, home_dir);
    let agent_dir_status = match resolved.path.as_deref() {
        None => {
            findings.push(ValidationFinding {
                level: FindingLevel::Error,
                field: ConfigField::AgentDir,
                code: "agent_dir_unresolved",
                message: format!(
                    "无法确定宿主目录：请填写宿主目录，或设置环境变量 {AGENT_DIR_ENV_VAR}"
                ),
            });
            PathStatus::Unset
        }
        Some(agent_dir) => {
            let inspection = inspect_path(agent_dir, false);
            if let Some(issue) = inspection.issue {
                findings.push(issue.to_finding(ConfigField::AgentDir));
            }
            inspection.status
        }
    };

    if let (Some(sync_root), Some(agent_dir)) = (config.sync_root.as_deref(), resolved.path.as_deref())
    {
        if is_plain_absolute(sync_root) && is_plain_absolute(agent_dir) {
            if let Some(message) = overlap_message(sync_root, agent_dir) {
                findings.push(ValidationFinding {
                    level: FindingLevel::Error,
                    field: ConfigField::SyncRoot,
                    code: "sync_root_overlaps_agent_dir",
                    message: message.to_string(),
                });
            }
        }
    }

    let ok = findings
        .iter()
        .all(|finding| finding.level != FindingLevel::Error);

    ConfigValidation {
        ok,
        findings,
        sync_root_status,
        agent_dir_status,
        effective_agent_dir: resolved.path,
        agent_dir_source: resolved.source,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathIssue {
    NotAbsolute,
    ParentComponent,
    Missing,
    NotDirectory,
    NotWritable,
    Unreadable,
}

impl PathIssue {
    fn level(self) -> FindingLevel {
        match self {
            PathIssue::Missing => FindingLevel::Warning,
            _ => FindingLevel::Error,
        }
    }

    fn code(self) -> &'static str {
        match self {
            PathIssue::NotAbsolute => "path_not_absolute",
            PathIssue::ParentComponent => "path_has_parent_component",
            PathIssue::Missing => "path_missing",
            PathIssue::NotDirectory => "path_not_directory",
            PathIssue::NotWritable => "path_not_writable",
            PathIssue::Unreadable => "path_unreadable",
        }
    }

    fn to_finding(self, field: ConfigField) -> ValidationFinding {
        ValidationFinding {
            level: self.level(),
            field,
            code: self.code(),
            message: self.message(field).to_string(),
        }
    }

    fn message(self, field: ConfigField) -> &'static str {
        let is_sync_root = field == ConfigField::SyncRoot;
        match self {
            PathIssue::NotAbsolute => {
                if is_sync_root {
                    "同步文件夹必须是绝对路径，可用“浏览…”选择"
                } else {
                    "宿主目录必须是绝对路径，可用“浏览…”选择"
                }
            }
            PathIssue::ParentComponent => "路径不能包含 .. 片段，请填写规范化的绝对路径",
            PathIssue::Missing => {
                if is_sync_root {
                    "同步文件夹不存在：保存后仍会提醒，创建该目录即可消除"
                } else {
                    "宿主目录不存在：请确认 Pi 已安装，或填写正确的目录"
                }
            }
            PathIssue::NotDirectory => "路径指向的不是文件夹",
            PathIssue::NotWritable => "同步文件夹不可写，无法写入快照",
            PathIssue::Unreadable => "无法访问该路径：权限不足或路径不可读",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PathInspection {
    status: PathStatus,
    issue: Option<PathIssue>,
}

fn inspect_path(path: &Path, require_writable: bool) -> PathInspection {
    if !path.is_absolute() {
        return PathInspection {
            status: PathStatus::Invalid,
            issue: Some(PathIssue::NotAbsolute),
        };
    }

    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return PathInspection {
            status: PathStatus::Invalid,
            issue: Some(PathIssue::ParentComponent),
        };
    }

    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return PathInspection {
                status: PathStatus::Missing,
                issue: Some(PathIssue::Missing),
            }
        }
        Err(_) => {
            return PathInspection {
                status: PathStatus::Invalid,
                issue: Some(PathIssue::Unreadable),
            }
        }
    };

    if !metadata.is_dir() {
        return PathInspection {
            status: PathStatus::Invalid,
            issue: Some(PathIssue::NotDirectory),
        };
    }

    // 只检查权限位，不落探测文件：目录 setgid/ACL 场景由后续写入时的真实错误兜底。
    if require_writable && metadata.permissions().readonly() {
        return PathInspection {
            status: PathStatus::NotWritable,
            issue: Some(PathIssue::NotWritable),
        };
    }

    PathInspection {
        status: PathStatus::Ok,
        issue: None,
    }
}

fn is_plain_absolute(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
}

fn overlap_message(sync_root: &Path, agent_dir: &Path) -> Option<&'static str> {
    if sync_root == agent_dir {
        Some("同步文件夹不能与宿主目录相同")
    } else if sync_root.starts_with(agent_dir) {
        Some("同步文件夹不能位于宿主目录内部")
    } else if agent_dir.starts_with(sync_root) {
        Some("宿主目录不能位于同步文件夹内部")
    } else {
        None
    }
}

fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = create_private_file(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    set_private_permissions(path)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT_DIR_ID: AtomicU32 = AtomicU32::new(0);

    /// 临时目录夹具：不引入 tempfile，退出时自行清理。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = NEXT_DIR_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-{}-{}-{}",
                label,
                std::process::id(),
                unique
            ));
            if path.exists() {
                fs::remove_dir_all(&path).expect("清理残留临时目录");
            }
            fs::create_dir_all(&path).expect("创建临时目录");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn join(&self, child: &str) -> PathBuf {
            self.path.join(child)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn config_with(sync_root: Option<&Path>, agent_dir_override: Option<&Path>) -> AppConfig {
        AppConfig {
            sync_root: sync_root.map(Path::to_path_buf),
            agent_dir_override: agent_dir_override.map(Path::to_path_buf),
            ..AppConfig::default()
        }
    }

    fn find_code<'a>(
        validation: &'a ConfigValidation,
        code: &str,
    ) -> Option<&'a ValidationFinding> {
        validation.findings.iter().find(|item| item.code == code)
    }

    #[test]
    fn defaults_match_documented_values() {
        let config = AppConfig::default();
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(config.sync_root, None);
        assert_eq!(config.agent_dir_override, None);
        assert_eq!(config.backup_retention, DEFAULT_BACKUP_RETENTION);
        assert_eq!(DEFAULT_BACKUP_RETENTION, 10);
    }

    #[test]
    fn load_returns_defaults_when_file_is_missing() {
        let dir = TempDir::new("load-default");
        let loaded = load_config(dir.path()).expect("文件不存在时应返回默认值");
        assert_eq!(loaded, AppConfig::default());
        assert!(!dir.join(CONFIG_FILE_NAME).exists());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = TempDir::new("round-trip");
        let home = dir.join("home");
        let cloud = dir.join("cloud");
        let host = dir.join("host");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&cloud).unwrap();
        fs::create_dir_all(&host).unwrap();

        let config = AppConfig {
            sync_root: Some(cloud.clone()),
            agent_dir_override: Some(host.clone()),
            backup_retention: 3,
            ..AppConfig::default()
        };

        let (saved, validation) =
            save_config_checked(dir.path(), &config, None, Some(&home)).expect("保存应成功");
        assert!(validation.ok);
        assert_eq!(saved.sync_root.as_deref(), Some(cloud.as_path()));

        let loaded = load_config(dir.path()).expect("读取应成功");
        assert_eq!(loaded, saved);
        assert_eq!(loaded.backup_retention, 3);
        assert_eq!(loaded.version, CONFIG_VERSION);
    }

    #[test]
    fn empty_paths_are_normalized_to_unset() {
        let dir = TempDir::new("normalize");
        let config = AppConfig {
            sync_root: Some(PathBuf::from("   ")),
            agent_dir_override: Some(PathBuf::from("")),
            ..AppConfig::default()
        };
        let saved = save_config(dir.path(), &config).expect("保存应成功");
        assert_eq!(saved.sync_root, None);
        assert_eq!(saved.agent_dir_override, None);
        let loaded = load_config(dir.path()).unwrap();
        assert_eq!(loaded.sync_root, None);
        assert_eq!(loaded.agent_dir_override, None);
    }

    #[test]
    fn config_file_holds_only_known_non_secret_fields() {
        let dir = TempDir::new("keys");
        save_config(
            dir.path(),
            &AppConfig {
                sync_root: Some(PathBuf::from("/home/tester/cloud/sync")),
                agent_dir_override: Some(PathBuf::from("/home/tester/agent")),
                backup_retention: 5,
                ..AppConfig::default()
            },
        )
        .expect("保存应成功");

        let text = fs::read_to_string(dir.join(CONFIG_FILE_NAME)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let keys: BTreeSet<String> = value
            .as_object()
            .expect("配置应为 JSON 对象")
            .keys()
            .cloned()
            .collect();
        let expected: BTreeSet<String> = [
            "agentDirOverride",
            "backupRetention",
            "syncRoot",
            "version",
        ]
        .iter()
        .map(|key| key.to_string())
        .collect();
        assert_eq!(keys, expected);

        for key in &keys {
            let lowered = key.to_lowercase();
            for banned in ["secret", "token", "password", "apikey", "api_key", "auth"] {
                assert!(
                    !lowered.contains(banned),
                    "配置键 {key} 不应出现密钥语义"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn default_config_file_contains_explicit_nulls() {
        let dir = TempDir::new("explicit-nulls");
        save_config(dir.path(), &AppConfig::default()).expect("保存应成功");

        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(CONFIG_FILE_NAME)).unwrap()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "version": CONFIG_VERSION,
                "syncRoot": serde_json::Value::Null,
                "agentDirOverride": serde_json::Value::Null,
                "backupRetention": DEFAULT_BACKUP_RETENTION,
            })
        );
    }

    #[test]
    fn saved_config_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("permissions");
        save_config(dir.path(), &AppConfig::default()).expect("保存应成功");

        let mode = fs::metadata(dir.join(CONFIG_FILE_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "config.json 应为 0600");
    }

    #[test]
    fn corrupt_json_is_reported_and_file_kept_intact() {
        let dir = TempDir::new("corrupt");
        let path = dir.join(CONFIG_FILE_NAME);
        let garbage = "{ \"syncRoot\": \"/tmp/cloud\", ";
        fs::write(&path, garbage).unwrap();

        let error = load_config(dir.path()).expect_err("损坏文件应报错");
        assert!(matches!(error, ConfigError::Corrupt { .. }));
        assert!(error.to_string().contains("原文件未改动"));
        assert_eq!(fs::read_to_string(&path).unwrap(), garbage);
    }

    #[test]
    fn unsupported_version_is_reported_and_file_kept_intact() {
        let dir = TempDir::new("version");
        let path = dir.join(CONFIG_FILE_NAME);
        let contents = format!("{{\"version\": {}}}", CONFIG_VERSION + 1);
        fs::write(&path, &contents).unwrap();

        let error = load_config(dir.path()).expect_err("版本不支持应报错");
        assert!(matches!(error, ConfigError::UnsupportedVersion { .. }));
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    }

    #[test]
    fn relative_sync_root_is_rejected() {
        let dir = TempDir::new("relative");
        let home = dir.join("home");
        fs::create_dir_all(&home).unwrap();

        let config = config_with(Some(Path::new("cloud/sync")), None);
        let validation = validate(&config, None, Some(&home));

        assert!(!validation.ok);
        assert_eq!(validation.sync_root_status, PathStatus::Invalid);
        let finding = find_code(&validation, "path_not_absolute").expect("应报相对路径错误");
        assert_eq!(finding.level, FindingLevel::Error);
        assert_eq!(finding.field, ConfigField::SyncRoot);

        let error = save_config_checked(dir.path(), &config, None, Some(&home))
            .expect_err("相对路径不应被保存");
        assert!(matches!(error, ConfigError::Validation(_)));
        assert!(!dir.join(CONFIG_FILE_NAME).exists());
    }

    #[test]
    fn parent_component_is_rejected() {
        let dir = TempDir::new("parent");
        let home = dir.join("home");
        fs::create_dir_all(&home).unwrap();

        let nested = dir.join("cloud").join("..").join("cloud");
        let config = config_with(Some(&nested), None);
        let validation = validate(&config, None, Some(&home));

        assert!(!validation.ok);
        assert_eq!(validation.sync_root_status, PathStatus::Invalid);
        assert!(find_code(&validation, "path_has_parent_component").is_some());
    }

    #[test]
    fn sync_root_cannot_equal_agent_dir() {
        let dir = TempDir::new("overlap-same");
        let agent = dir.join("agent");
        fs::create_dir_all(&agent).unwrap();

        let config = config_with(Some(&agent), Some(&agent));
        let validation = validate(&config, None, Some(dir.path()));

        assert!(!validation.ok);
        let finding =
            find_code(&validation, "sync_root_overlaps_agent_dir").expect("相同路径应被拒绝");
        assert!(finding.message.contains("相同"));
    }

    #[test]
    fn sync_root_cannot_be_nested_inside_agent_dir() {
        let dir = TempDir::new("overlap-inside");
        let agent = dir.join("agent");
        let sync_root = agent.join("snapshots");
        fs::create_dir_all(&sync_root).unwrap();

        let config = config_with(Some(&sync_root), Some(&agent));
        let validation = validate(&config, None, Some(dir.path()));

        assert!(!validation.ok);
        let finding = find_code(&validation, "sync_root_overlaps_agent_dir").unwrap();
        assert!(finding.message.contains("同步文件夹不能位于宿主目录内部"));
    }

    #[test]
    fn agent_dir_cannot_be_nested_inside_sync_root() {
        let dir = TempDir::new("overlap-agent-inside");
        // 默认宿主目录 ~/.pi/agent，同步文件夹取父级目录
        let config = config_with(Some(dir.path()), None);
        let validation = validate(&config, None, Some(dir.path()));

        assert!(!validation.ok);
        let finding = find_code(&validation, "sync_root_overlaps_agent_dir").unwrap();
        assert!(finding.message.contains("宿主目录不能位于同步文件夹内部"));
    }

    #[test]
    fn sibling_directories_are_accepted() {
        let dir = TempDir::new("siblings");
        let cloud = dir.join("cloud");
        let agent = dir.join("agent");
        fs::create_dir_all(&cloud).unwrap();
        fs::create_dir_all(&agent).unwrap();

        // 前缀相同的兄弟目录不能误判为嵌套
        let sibling = dir.join("cloud-backup");
        fs::create_dir_all(&sibling).unwrap();

        let config = config_with(Some(&cloud), Some(&agent));
        let validation = validate(&config, None, Some(dir.path()));
        assert!(validation.ok, "同级目录应通过校验: {:?}", validation.findings);

        let prefix_config = config_with(Some(&sibling), Some(&agent));
        let prefix_validation = validate(&prefix_config, None, Some(dir.path()));
        assert!(prefix_validation.ok, "前缀相同的路径不应视为嵌套");
    }

    #[test]
    fn missing_directories_are_warnings_and_still_savable() {
        let dir = TempDir::new("missing");
        let home = dir.join("home");
        fs::create_dir_all(&home).unwrap();

        let absent = dir.join("cloud");
        let config = config_with(Some(&absent), None);
        let validation = validate(&config, None, Some(&home));

        assert!(validation.ok, "目录不存在不应阻断保存");
        assert_eq!(validation.sync_root_status, PathStatus::Missing);
        assert_eq!(validation.agent_dir_status, PathStatus::Missing);
        assert_eq!(
            find_code(&validation, "path_missing").map(|item| item.level),
            Some(FindingLevel::Warning)
        );

        save_config_checked(dir.path(), &config, None, Some(&home)).expect("允许保存");
        assert_eq!(load_config(dir.path()).unwrap().sync_root, Some(absent));
    }

    #[cfg(unix)]
    #[test]
    fn not_writable_sync_root_is_an_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("readonly");
        let home = dir.join("home");
        fs::create_dir_all(&home).unwrap();
        let cloud = dir.join("cloud");
        fs::create_dir_all(&cloud).unwrap();

        let original = fs::metadata(&cloud).unwrap().permissions();
        fs::set_permissions(&cloud, fs::Permissions::from_mode(0o500)).unwrap();

        let config = config_with(Some(&cloud), None);
        let validation = validate(&config, None, Some(&home));

        fs::set_permissions(&cloud, original).unwrap();

        assert!(!validation.ok);
        assert_eq!(validation.sync_root_status, PathStatus::NotWritable);
        let finding = find_code(&validation, "path_not_writable").expect("应报不可写");
        assert_eq!(finding.level, FindingLevel::Error);
    }

    #[test]
    fn backup_retention_out_of_range_is_rejected() {
        let dir = TempDir::new("retention");
        let home = dir.join("home");
        fs::create_dir_all(&home).unwrap();

        for value in [0, MAX_BACKUP_RETENTION + 1] {
            let config = AppConfig {
                backup_retention: value,
                ..AppConfig::default()
            };
            let validation = validate(&config, None, Some(&home));
            assert!(!validation.ok, "retention={value} 应被拒绝");
            assert!(find_code(&validation, "backup_retention_out_of_range").is_some());

            let error = save_config_checked(dir.path(), &config, None, Some(&home))
                .expect_err("越界 retention 不应被保存");
            assert!(matches!(error, ConfigError::Validation(_)));
            assert!(!dir.join(CONFIG_FILE_NAME).exists());
        }

        for value in [MIN_BACKUP_RETENTION, DEFAULT_BACKUP_RETENTION, MAX_BACKUP_RETENTION] {
            let config = AppConfig {
                backup_retention: value,
                ..AppConfig::default()
            };
            assert!(validate(&config, None, Some(&home)).ok, "retention={value} 应允许");
        }
    }

    #[test]
    fn agent_dir_resolution_prefers_override_then_env_then_default() {
        let dir = TempDir::new("resolve");
        let home = dir.join("home");
        let override_dir = dir.join("host");
        let env_dir = dir.join("env-host");

        let config = config_with(None, Some(&override_dir));
        let resolved = resolve_agent_dir(&config, Some(&env_dir), Some(&home));
        assert_eq!(resolved.source, AgentDirSource::Override);
        assert_eq!(resolved.path.as_deref(), Some(override_dir.as_path()));

        let config = config_with(None, None);
        let resolved = resolve_agent_dir(&config, Some(&env_dir), Some(&home));
        assert_eq!(resolved.source, AgentDirSource::Env);
        assert_eq!(resolved.path.as_deref(), Some(env_dir.as_path()));

        let resolved = resolve_agent_dir(&config, None, Some(&home));
        assert_eq!(resolved.source, AgentDirSource::Default);
        assert_eq!(resolved.path, Some(home.join(".pi").join("agent")));

        let resolved = resolve_agent_dir(&config, None, None);
        assert_eq!(resolved.source, AgentDirSource::Unresolved);
        assert_eq!(resolved.path, None);
    }

    #[test]
    fn validation_serializes_with_frontend_field_names() {
        let config = config_with(Some(Path::new("cloud/sync")), None);
        let validation = validate(&config, None, None);
        let value = serde_json::to_value(&validation).expect("校验结果应可序列化");
        let object = value.as_object().expect("校验结果应为 JSON 对象");

        let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            BTreeSet::from([
                "agentDirSource",
                "agentDirStatus",
                "effectiveAgentDir",
                "findings",
                "ok",
                "syncRootStatus",
            ])
        );
        assert_eq!(object["syncRootStatus"], serde_json::json!("invalid"));
        assert_eq!(object["agentDirStatus"], serde_json::json!("unset"));
        assert_eq!(object["agentDirSource"], serde_json::json!("unresolved"));
        assert_eq!(object["effectiveAgentDir"], serde_json::Value::Null);

        let finding = &object["findings"][0];
        let finding_keys: BTreeSet<&str> =
            finding.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            finding_keys,
            BTreeSet::from(["code", "field", "level", "message"])
        );
        assert_eq!(finding["field"], serde_json::json!("syncRoot"));
        assert_eq!(finding["level"], serde_json::json!("error"));
    }

    #[test]
    fn unresolved_agent_dir_blocks_saving() {
        let dir = TempDir::new("unresolved");
        let config = config_with(None, None);
        let validation = validate(&config, None, None);

        assert!(!validation.ok);
        assert_eq!(validation.agent_dir_status, PathStatus::Unset);
        assert!(find_code(&validation, "agent_dir_unresolved").is_some());
        assert!(save_config_checked(dir.path(), &config, None, None).is_err());
    }
}

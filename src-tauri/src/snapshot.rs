//! 快照导出流水线。
//!
//! 流程来源：`docs/design/10-snapshot-sync.md` 第 5 节。
//! 顺序固定为：预检 → 物化到临时目录 → 密钥引用化/扫描 → 原子替换 → 最后写 manifest。
//! 任一步失败都删掉临时目录，原快照与 manifest 保持原样。
//!
//! `export_snapshot` 是 command 层入口；核心逻辑 [`export`] 不碰进程环境之外的东西，
//! 可直接用临时目录单测。command 定义在本文件是因为并行开发约束下不能改 `commands.rs`。

use crate::config::{self, AGENT_DIR_ENV_VAR};
use crate::doctor::{self, ShellRunner};
use crate::platform::HostOs;
use crate::scope;
use crate::secrets::{self, RequiredSecret, SecretFinding, SecretLevel};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::Manager;

/// 快照目录名（放在同步根目录下）。
pub const SNAPSHOT_DIR_NAME: &str = "snapshot";
/// manifest 文件名。
pub const MANIFEST_FILE_NAME: &str = "manifest.json";
/// manifest 格式版本。不认识的值一律拒绝读取。
pub const FORMAT_VERSION: u32 = 1;
/// v1 只有一套配置。
pub const PROFILE_ID: &str = "default";

const REQUIREMENT_NODE: &str = ">=22.19.0";
const REQUIREMENT_PI: &str = ">=1.5.0";

/// 快照 manifest。字段名与 `10-snapshot-sync.md` 第 4 节一致（camelCase）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format_version: u32,
    pub profile_id: String,
    pub created_at: String,
    pub fingerprint: String,
    pub source: SourceInfo,
    pub requirements: Requirements,
    pub files: Vec<ManifestFile>,
    pub symlinks: Vec<ManifestSymlink>,
    pub required_secrets: Vec<RequiredSecret>,
    pub required_packages: Vec<RequiredPackage>,
    /// 用户在知情前提下保留了明文密钥；导入端据此给出警告。
    #[serde(default, skip_serializing_if = "is_false")]
    pub plaintext_secrets: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub app: String,
    pub os: String,
    pub arch: String,
    /// 宿主版本由环境检测探测；这里只记录结论，未知时为 null。
    pub pi: Option<String>,
    pub node: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Requirements {
    pub node: String,
    pub pi: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestFile {
    /// 相对同步根目录的路径，形如 `snapshot/settings.json`。
    pub path: String,
    pub sha256: String,
    pub size: u64,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestSymlink {
    pub path: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequiredPackage {
    pub source: String,
    pub enabled: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// 一次导出的结果，供 UI 展示。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportReport {
    pub files: usize,
    pub fingerprint: String,
    pub required_secrets: Vec<RequiredSecret>,
    pub required_packages: Vec<RequiredPackage>,
    /// 断链等被跳过的条目数。
    pub skipped: usize,
    /// 低置信提示：不阻断，也没有自动改写。
    pub warnings: Vec<SecretFinding>,
    pub plaintext_secrets: bool,
    pub snapshot_dir: PathBuf,
    pub manifest_path: PathBuf,
}

/// 导出失败。失败码见 `docs/design/10-snapshot-sync.md` 第 9 节与任务卡。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportFailure {
    pub code: &'static str,
    pub message: String,
    /// 仅 `secrets_unconfirmed` 会带上待确认条目。
    pub findings: Vec<SecretFinding>,
}

impl ExportFailure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            findings: Vec::new(),
        }
    }

    fn with_findings(
        code: &'static str,
        message: impl Into<String>,
        findings: Vec<SecretFinding>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            findings,
        }
    }
}

/// 导出时记进 manifest 的宿主版本。探测不到就留空，导出照常进行。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceVersions {
    pub pi: Option<String>,
    pub node: Option<String>,
}

/// 导出快照（不记来源版本）。生产路径都用 [`export_with`] 带上探测结果，
/// 这里留给单测：临时目录夹具不关心宿主版本。
#[cfg(test)]
pub fn export(
    agent_dir: &Path,
    sync_root: &Path,
    allow_plaintext: bool,
) -> Result<ExportReport, ExportFailure> {
    export_with(
        agent_dir,
        sync_root,
        allow_plaintext,
        SourceVersions::default(),
    )
}

/// 与 [`export`] 相同，额外把探测到的 node/pi 版本写进 `manifest.source`。
pub fn export_with(
    agent_dir: &Path,
    sync_root: &Path,
    allow_plaintext: bool,
    versions: SourceVersions,
) -> Result<ExportReport, ExportFailure> {
    precheck(agent_dir, sync_root)?;

    let stamp = unique_stamp();
    let temp_dir = sync_root.join(format!("{SNAPSHOT_DIR_NAME}.tmp-{stamp}"));
    // 只有上次进程崩溃才可能留下同名目录；先清掉再写。
    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir).map_err(|error| {
            ExportFailure::new(
                "no_write_permission",
                format!("无法清理残留临时目录：{error}"),
            )
        })?;
    }

    let materialized = scope::materialize(agent_dir, &temp_dir).map_err(|error| {
        let _ = fs::remove_dir_all(&temp_dir);
        ExportFailure::new("export_failed", format!("复制受管文件失败：{error}"))
    })?;

    let sanitized = secrets::sanitize_snapshot(&temp_dir).map_err(|error| {
        let _ = fs::remove_dir_all(&temp_dir);
        ExportFailure::new("export_failed", format!("密钥扫描失败：{error}"))
    })?;

    let blocking: Vec<SecretFinding> = sanitized
        .findings
        .iter()
        .filter(|finding| finding.level == SecretLevel::High)
        .cloned()
        .collect();
    if !blocking.is_empty() && !allow_plaintext {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(ExportFailure::with_findings(
            "secrets_unconfirmed",
            format!(
                "发现 {} 处明文密钥，未做自动改写；确认后可以仍然导出",
                blocking.len()
            ),
            blocking,
        ));
    }
    let plaintext_secrets = !blocking.is_empty();

    let collected = collect_snapshot_files(&temp_dir).map_err(|error| {
        let _ = fs::remove_dir_all(&temp_dir);
        ExportFailure::new("export_failed", format!("读取临时快照失败：{error}"))
    })?;
    let required_packages = declared_packages(&temp_dir);
    let files: Vec<ManifestFile> = collected
        .iter()
        .map(|file| ManifestFile {
            path: manifest_path(SNAPSHOT_DIR_NAME, &file.relative),
            sha256: file.sha256.clone(),
            size: file.size,
            kind: file.kind.to_string(),
        })
        .collect();

    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        profile_id: PROFILE_ID.to_string(),
        created_at: format_timestamp(now_seconds()),
        fingerprint: fingerprint(FORMAT_VERSION, &files),
        source: SourceInfo {
            app: env!("CARGO_PKG_VERSION").to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            pi: versions.pi,
            node: versions.node,
        },
        requirements: Requirements {
            node: REQUIREMENT_NODE.to_string(),
            pi: REQUIREMENT_PI.to_string(),
        },
        files,
        symlinks: materialized
            .symlinks
            .iter()
            .map(|(path, target)| ManifestSymlink {
                path: manifest_path(SNAPSHOT_DIR_NAME, path),
                target: target.to_string_lossy().into_owned(),
            })
            .collect(),
        required_secrets: sanitized.required_secrets,
        required_packages,
        plaintext_secrets,
    };

    let fingerprint = manifest.fingerprint.clone();
    swap_in(temp_dir, sync_root, stamp, &manifest)?;

    Ok(ExportReport {
        files: manifest.files.len(),
        fingerprint,
        required_secrets: manifest.required_secrets,
        required_packages: manifest.required_packages,
        skipped: materialized.skipped.len(),
        warnings: sanitized
            .findings
            .into_iter()
            .filter(|finding| finding.level == SecretLevel::Low)
            .collect(),
        plaintext_secrets,
        snapshot_dir: sync_root.join(SNAPSHOT_DIR_NAME),
        manifest_path: sync_root.join(MANIFEST_FILE_NAME),
    })
}

/// 预检：目录存在、路径规范、同步根与宿主目录不互相嵌套。
fn precheck(agent_dir: &Path, sync_root: &Path) -> Result<(), ExportFailure> {
    if !is_plain_absolute(sync_root) {
        return Err(ExportFailure::new(
            "sync_root_invalid",
            "同步文件夹必须是绝对路径且不含 ..，请在设置页重新选择",
        ));
    }
    if !sync_root.is_dir() {
        return Err(ExportFailure::new(
            "sync_root_missing",
            "同步文件夹不存在或不是文件夹，请在设置页选择",
        ));
    }
    if !agent_dir.is_dir() {
        return Err(ExportFailure::new(
            "agent_dir_missing",
            "宿主目录不存在或不是文件夹，请在设置页确认",
        ));
    }

    // 规范化后比前缀：`config::validate` 用的是同一套判定，这里不重复实现规则。
    let canonical_sync = fs::canonicalize(sync_root).map_err(|error| {
        ExportFailure::new("sync_root_invalid", format!("无法访问同步文件夹：{error}"))
    })?;
    let canonical_agent = fs::canonicalize(agent_dir).map_err(|error| {
        ExportFailure::new("agent_dir_missing", format!("无法访问宿主目录：{error}"))
    })?;
    if canonical_sync.starts_with(&canonical_agent) || canonical_agent.starts_with(&canonical_sync)
    {
        return Err(ExportFailure::new(
            "sync_root_overlaps_agent_dir",
            "同步文件夹与宿主目录不能相同，也不能互相嵌套",
        ));
    }

    Ok(())
}

fn is_plain_absolute(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
}

/// 原子替换：先挪走旧快照，再把临时目录改名为 `snapshot/`，最后写 manifest。
/// manifest 或自检失败时把旧快照挪回来。
fn swap_in(
    temp_dir: PathBuf,
    sync_root: &Path,
    stamp: u128,
    manifest: &Manifest,
) -> Result<(), ExportFailure> {
    let final_dir = sync_root.join(SNAPSHOT_DIR_NAME);
    let old_dir = sync_root.join(format!("{SNAPSHOT_DIR_NAME}.old-{stamp}"));
    let had_previous = final_dir.exists();

    if had_previous {
        fs::rename(&final_dir, &old_dir).map_err(|error| {
            let _ = fs::remove_dir_all(&temp_dir);
            ExportFailure::new("no_write_permission", format!("无法替换旧快照：{error}"))
        })?;
    }
    if let Err(error) = fs::rename(&temp_dir, &final_dir) {
        if had_previous {
            let _ = fs::rename(&old_dir, &final_dir);
        }
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(ExportFailure::new(
            "no_write_permission",
            format!("无法写入新快照：{error}"),
        ));
    }

    let result = restore_point(sync_root, manifest);
    if let Err(failure) = result {
        let _ = fs::remove_dir_all(&final_dir);
        if had_previous {
            let _ = fs::rename(&old_dir, &final_dir);
        }
        return Err(failure);
    }
    if had_previous {
        let _ = fs::remove_dir_all(&old_dir);
    }
    Ok(())
}

/// 写 manifest 并自检哈希。任何一步失败都把旧 manifest 写回去，避免“新 manifest + 旧快照”。
fn restore_point(sync_root: &Path, manifest: &Manifest) -> Result<(), ExportFailure> {
    let manifest_path = sync_root.join(MANIFEST_FILE_NAME);
    let previous = fs::read(&manifest_path).ok();

    let result =
        write_manifest(sync_root, manifest).and_then(|()| verify_files(sync_root, &manifest.files));
    if result.is_err() {
        match previous {
            Some(bytes) => {
                let _ = fs::write(&manifest_path, bytes);
            }
            None => {
                let _ = fs::remove_file(&manifest_path);
            }
        }
    }
    result
}

fn write_manifest(sync_root: &Path, manifest: &Manifest) -> Result<(), ExportFailure> {
    let target = sync_root.join(MANIFEST_FILE_NAME);
    let temp = sync_root.join(format!("{MANIFEST_FILE_NAME}.tmp"));

    let mut text = serde_json::to_string_pretty(manifest).map_err(|error| {
        ExportFailure::new("export_failed", format!("manifest 序列化失败：{error}"))
    })?;
    text.push('\n');

    fs::write(&temp, text).map_err(|error| {
        ExportFailure::new(
            "no_write_permission",
            format!("写入 manifest 失败：{error}"),
        )
    })?;
    fs::rename(&temp, &target).map_err(|error| {
        let _ = fs::remove_file(&temp);
        ExportFailure::new(
            "no_write_permission",
            format!("替换 manifest 失败：{error}"),
        )
    })
}

/// 逐文件重算哈希。导入校验直接复用；导出在写完 manifest 后自检一次。
pub fn verify_files(sync_root: &Path, files: &[ManifestFile]) -> Result<(), ExportFailure> {
    for file in files {
        let bytes = fs::read(sync_root.join(&file.path)).map_err(|error| {
            ExportFailure::new(
                "hash_mismatch",
                format!("快照文件读取失败：{}（{error}）", file.path),
            )
        })?;
        if bytes.len() as u64 != file.size || sha256_hex(&bytes) != file.sha256 {
            return Err(ExportFailure::new(
                "hash_mismatch",
                format!("快照文件与 manifest 不一致：{}", file.path),
            ));
        }
    }
    Ok(())
}

/// 快照指纹：`formatVersion` 与按 path 排序的 `{path, sha256}` 一起取 SHA-256，取前 12 位。
pub fn fingerprint(format_version: u32, files: &[ManifestFile]) -> String {
    let mut sorted: Vec<&ManifestFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));

    let mut hasher = Sha256::new();
    hasher.update(format!("{format_version}\n").as_bytes());
    for file in sorted {
        hasher.update(file.path.as_bytes());
        hasher.update([0u8]);
        hasher.update(file.sha256.as_bytes());
        hasher.update(*b"\n");
    }

    sha256_hex(&hasher.finalize())[..12].to_string()
}

/// 文件内容哈希：导入校验与导出共用同一套算法。
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `settings.json` 的 `packages`：字符串与 `{source, enabled}` 两种写法都认。
fn declared_packages(snapshot_dir: &Path) -> Vec<RequiredPackage> {
    let Ok(text) = fs::read_to_string(snapshot_dir.join("settings.json")) else {
        return Vec::new();
    };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(packages) = settings.get("packages").and_then(|value| value.as_array()) else {
        return Vec::new();
    };

    packages
        .iter()
        .filter_map(|item| match item {
            serde_json::Value::String(source) => Some(RequiredPackage {
                source: source.clone(),
                enabled: true,
            }),
            serde_json::Value::Object(map) => {
                let source = ["source", "name", "package"]
                    .iter()
                    .find_map(|key| map.get(*key)?.as_str())?;
                Some(RequiredPackage {
                    source: source.to_string(),
                    enabled: map
                        .get("enabled")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(true),
                })
            }
            _ => None,
        })
        .collect()
}

fn manifest_path(prefix: &str, relative: &Path) -> String {
    let mut path = String::from(prefix);
    for component in relative.components() {
        path.push('/');
        path.push_str(&component.as_os_str().to_string_lossy());
    }
    path
}

struct CollectedFile {
    relative: PathBuf,
    size: u64,
    sha256: String,
    kind: &'static str,
}

fn collect_snapshot_files(root: &Path) -> io::Result<Vec<CollectedFile>> {
    fn visit(root: &Path, relative: &Path, files: &mut Vec<CollectedFile>) -> io::Result<()> {
        let mut names: Vec<OsString> = Vec::new();
        for item in fs::read_dir(root.join(relative))? {
            names.push(item?.file_name());
        }
        names.sort();

        for name in names {
            let child = relative.join(&name);
            let metadata = fs::metadata(root.join(&child))?;
            if metadata.is_dir() {
                visit(root, &child, files)?;
            } else if metadata.is_file() {
                let bytes = fs::read(root.join(&child))?;
                files.push(CollectedFile {
                    relative: child.clone(),
                    size: metadata.len(),
                    sha256: sha256_hex(&bytes),
                    kind: kind_for(&child, &bytes),
                });
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    visit(root, Path::new(""), &mut files)?;
    Ok(files)
}

fn kind_for(relative: &Path, bytes: &[u8]) -> &'static str {
    match relative
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("json") => "json",
        Some("md") | Some("mdx") | Some("markdown") => "markdown",
        _ if std::str::from_utf8(bytes).is_ok() => "text",
        _ => "binary",
    }
}

fn unique_stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// RFC 3339 UTC（秒级）。只用 std：chrono 不在允许新增的依赖里。
/// 备份与暂存目录的命名也用它，不另写第二套日历换算。
pub(crate) fn format_timestamp(unix_seconds: u64) -> String {
    let days = (unix_seconds / 86_400) as i64;
    let seconds_of_day = unix_seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    )
}

/// Howard Hinnant 的 civil_from_days：从 epoch 天数换回年月日。
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_position + 2) / 5 + 1;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// command 层入口：从应用配置读取同步根目录与宿主目录，再跑导出。
#[tauri::command]
pub async fn export_snapshot(
    app: tauri::AppHandle,
    allow_plaintext: Option<bool>,
) -> Result<ExportReport, ExportFailure> {
    // 版本探测要起子进程，最多各等 5 秒；放阻塞线程池，别占住 UI 线程。
    tauri::async_runtime::spawn_blocking(move || export_snapshot_blocking(&app, allow_plaintext))
        .await
        .map_err(|error| ExportFailure::new("export_failed", format!("导出失败：{error}")))?
}

fn export_snapshot_blocking(
    app: &tauri::AppHandle,
    allow_plaintext: Option<bool>,
) -> Result<ExportReport, ExportFailure> {
    let config_dir = app.path().app_config_dir().map_err(|error| {
        ExportFailure::new(
            "app_config_unavailable",
            format!("无法定位应用配置目录：{error}"),
        )
    })?;
    let config = config::load_config(&config_dir)
        .map_err(|error| ExportFailure::new("app_config_unavailable", error.to_string()))?;

    let sync_root = config.sync_root.clone().ok_or_else(|| {
        ExportFailure::new(
            "sync_root_missing",
            "还没有设置同步文件夹，请先在设置页选择",
        )
    })?;

    // 只读取这一个文档化的环境变量；错误信息里不回显其内容。
    let env_agent_dir = std::env::var_os(AGENT_DIR_ENV_VAR).map(PathBuf::from);
    let home_dir = app.path().home_dir().ok();
    let resolved =
        config::resolve_agent_dir(&config, env_agent_dir.as_deref(), home_dir.as_deref());
    let agent_dir = resolved.path.ok_or_else(|| {
        ExportFailure::new("agent_dir_missing", "无法确定宿主目录，请先在设置页填写")
    })?;

    // 记下来源版本，另一台机器据此判断快照来自哪个宿主；探测不到就留空。
    let (pi, node) = doctor::probe_versions(
        Some(&agent_dir),
        &ShellRunner::from_env(),
        HostOs::current(),
    );
    export_with(
        &agent_dir,
        &sync_root,
        allow_plaintext.unwrap_or(false),
        SourceVersions { pi, node },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 临时目录夹具：Drop 时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-snapshot-{}-{}-{}",
                std::process::id(),
                label,
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("创建测试临时目录");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const SECRET: &str = "Zx7Qv2Lm9Rt4Bc8Wn3Yp6Kd1";

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("创建父目录");
        }
        fs::write(path, content).expect("写入夹具文件");
    }

    /// 一套最小的 agent 目录夹具：覆盖根级 json、指令文件与资源目录。
    fn fixture_agent(root: &Path) {
        write_file(
            &root.join("settings.json"),
            "{\"packages\": [\"npm:pi-web-access\", {\"source\": \"npm:pi-notify\", \"enabled\": false}]}",
        );
        write_file(
            &root.join("models.json"),
            &format!("{{\"providers\": {{\"openrouter\": {{\"apiKey\": \"sk-or-{SECRET}\"}}}}}}"),
        );
        write_file(
            &root.join("web-search.json"),
            &format!("{{\"exaApiKey\": \"{SECRET}\"}}"),
        );
        write_file(&root.join("AGENTS.md"), "# 指令\n");
        write_file(&root.join("skills/antd/SKILL.md"), "# 技能\n");
        // 范围外：永不进入快照。
        write_file(
            &root.join("auth.json"),
            "{\"openai\": {\"key\": \"sk-secret\"}}",
        );
        write_file(&root.join("sessions/session.json"), "{}");
    }

    fn read_manifest(sync_root: &Path) -> Manifest {
        let text = fs::read_to_string(sync_root.join(MANIFEST_FILE_NAME)).expect("读取 manifest");
        serde_json::from_str(&text).expect("解析 manifest")
    }

    fn dir_entries(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(root)
            .expect("读取目录")
            .map(|item| {
                item.expect("目录项")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn export_records_probed_versions_and_leaves_them_null_when_unknown() {
        let temp = TempDir::new("versions");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fixture_agent(&agent);
        fs::create_dir_all(&sync_root).expect("创建同步目录");

        let versions = SourceVersions {
            pi: Some("1.6.0".to_string()),
            node: Some("24.14.0".to_string()),
        };
        export_with(&agent, &sync_root, false, versions).expect("导出应成功");

        let manifest = read_manifest(&sync_root);
        assert_eq!(manifest.source.pi.as_deref(), Some("1.6.0"));
        assert_eq!(manifest.source.node.as_deref(), Some("24.14.0"));

        // 探测不到时留空：导出照常成功，manifest 里是 null。
        export(&agent, &sync_root, false).expect("导出应成功");
        let manifest = read_manifest(&sync_root);
        assert_eq!(manifest.source.pi, None);
        assert_eq!(manifest.source.node, None);
    }

    #[test]
    fn export_writes_manifest_matching_snapshot() {
        let temp = TempDir::new("export");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fixture_agent(&agent);
        fs::create_dir_all(&sync_root).expect("创建同步目录");

        let report = export(&agent, &sync_root, false).expect("导出应成功");
        assert_eq!(report.files, 5, "{:?}", report);
        assert_eq!(report.fingerprint.len(), 12);
        assert!(report.fingerprint.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert!(!report.plaintext_secrets);

        let manifest = read_manifest(&sync_root);
        assert_eq!(manifest.format_version, FORMAT_VERSION);
        assert_eq!(manifest.profile_id, PROFILE_ID);
        assert_eq!(manifest.created_at.len(), 20);
        assert!(manifest.created_at.ends_with('Z'));
        assert_eq!(manifest.fingerprint, report.fingerprint);
        assert!(manifest.symlinks.is_empty());
        assert_eq!(manifest.source.app, env!("CARGO_PKG_VERSION"));
        assert_eq!(manifest.source.os, std::env::consts::OS);
        assert_eq!(manifest.requirements.node, REQUIREMENT_NODE);

        // manifest 的文件表与磁盘一致：路径、大小、哈希都能复算。
        let mut paths: Vec<String> = manifest
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                "snapshot/AGENTS.md",
                "snapshot/models.json",
                "snapshot/settings.json",
                "snapshot/skills/antd/SKILL.md",
                "snapshot/web-search.json",
            ]
        );
        verify_files(&sync_root, &manifest.files).expect("自检哈希");

        // 指纹只由 formatVersion 与 {path, sha256} 决定，可复算。
        assert_eq!(
            fingerprint(FORMAT_VERSION, &manifest.files),
            report.fingerprint
        );

        // 包声明：字符串与对象两种写法都进 requiredPackages。
        assert_eq!(
            manifest.required_packages,
            vec![
                RequiredPackage {
                    source: "npm:pi-web-access".to_string(),
                    enabled: true,
                },
                RequiredPackage {
                    source: "npm:pi-notify".to_string(),
                    enabled: false,
                },
            ]
        );

        // 密钥已被引用化，快照里没有明文；范围外文件没有进入快照。
        let models = fs::read_to_string(sync_root.join("snapshot/models.json")).expect("读取快照");
        assert!(!models.contains(SECRET), "{models}");
        assert!(models.contains("$PI_OPENROUTER_API_KEY"), "{models}");
        assert!(!sync_root.join("snapshot/auth.json").exists());
        assert!(!sync_root.join("snapshot/sessions").exists());

        assert_eq!(
            report
                .required_secrets
                .iter()
                .map(|secret| secret.name.as_str())
                .collect::<Vec<_>>(),
            vec!["PI_EXA_API_KEY", "PI_OPENROUTER_API_KEY"]
        );

        // 同步根目录里只多了约定产物；没有临时目录残留。
        assert_eq!(dir_entries(&sync_root), vec!["manifest.json", "snapshot"]);
    }

    #[test]
    fn fingerprint_is_order_independent_and_content_sensitive() {
        let files = vec![
            ManifestFile {
                path: "snapshot/b.json".to_string(),
                sha256: "bb".repeat(32),
                size: 2,
                kind: "json".to_string(),
            },
            ManifestFile {
                path: "snapshot/a.json".to_string(),
                sha256: "aa".repeat(32),
                size: 1,
                kind: "json".to_string(),
            },
        ];
        let mut reversed = files.clone();
        reversed.reverse();

        let expected = fingerprint(FORMAT_VERSION, &files);
        assert_eq!(expected, fingerprint(FORMAT_VERSION, &reversed));
        assert_eq!(expected.len(), 12);

        let mut changed = files.clone();
        changed[0].sha256 = "cc".repeat(32);
        assert_ne!(expected, fingerprint(FORMAT_VERSION, &changed));
        assert_ne!(expected, fingerprint(FORMAT_VERSION + 1, &files));
    }

    #[test]
    fn hash_mismatch_is_reported() {
        let temp = TempDir::new("hash");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fixture_agent(&agent);
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        export(&agent, &sync_root, false).expect("导出应成功");
        let manifest = read_manifest(&sync_root);

        // 原样通过。
        verify_files(&sync_root, &manifest.files).expect("未被改动时应通过");

        // 改一个字节：哈希不符。
        let target = sync_root.join("snapshot/settings.json");
        let mut bytes = fs::read(&target).expect("读取快照文件");
        bytes.push(b' ');
        fs::write(&target, bytes).expect("改写快照文件");

        let failure = verify_files(&sync_root, &manifest.files).expect_err("应报哈希不符");
        assert_eq!(failure.code, "hash_mismatch");
        assert!(failure.message.contains("snapshot/settings.json"));
    }

    #[test]
    fn second_export_failure_keeps_previous_snapshot_and_manifest() {
        let temp = TempDir::new("atomic");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fixture_agent(&agent);
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        export(&agent, &sync_root, false).expect("首次导出应成功");

        let manifest_before =
            fs::read_to_string(sync_root.join(MANIFEST_FILE_NAME)).expect("读 manifest");
        let settings_before =
            fs::read_to_string(sync_root.join("snapshot/settings.json")).expect("读快照文件");

        // 第二次：宿主目录里出现非已知文件中的明文密钥，未确认时必须中止。
        write_file(
            &agent.join("extensions/leak/index.ts"),
            &format!("const apiKey = \"sk-{SECRET}\";\n"),
        );
        let failure = export(&agent, &sync_root, false).expect_err("应阻断");
        assert_eq!(failure.code, "secrets_unconfirmed");
        assert_eq!(failure.findings.len(), 1);
        assert_eq!(
            failure.findings[0].path,
            "snapshot/extensions/leak/index.ts"
        );

        assert_eq!(
            fs::read_to_string(sync_root.join(MANIFEST_FILE_NAME)).expect("读 manifest"),
            manifest_before
        );
        assert_eq!(
            fs::read_to_string(sync_root.join("snapshot/settings.json")).expect("读快照文件"),
            settings_before
        );
        assert!(!sync_root.join("snapshot/extensions/leak").exists());
        // 没有临时/旧目录残留。
        assert_eq!(dir_entries(&sync_root), vec!["manifest.json", "snapshot"]);

        // 用户确认后可以导出，manifest 记录明文。
        let report = export(&agent, &sync_root, true).expect("确认后应成功");
        assert!(report.plaintext_secrets);
        assert!(read_manifest(&sync_root).plaintext_secrets);
    }

    #[test]
    fn nested_sync_root_is_rejected() {
        let temp = TempDir::new("nested");
        let agent = temp.path().join("agent");
        fixture_agent(&agent);

        let inside = agent.join("cloud");
        fs::create_dir_all(&inside).expect("创建嵌套同步目录");
        let failure = export(&agent, &inside, false).expect_err("嵌套应被拒绝");
        assert_eq!(failure.code, "sync_root_overlaps_agent_dir");

        let missing = temp.path().join("missing");
        let failure = export(&agent, &missing, false).expect_err("缺失应被拒绝");
        assert_eq!(failure.code, "sync_root_missing");

        let failure =
            export(&agent, Path::new("relative/path"), false).expect_err("相对路径应被拒绝");
        assert_eq!(failure.code, "sync_root_invalid");
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let manifest = Manifest {
            format_version: FORMAT_VERSION,
            profile_id: PROFILE_ID.to_string(),
            created_at: "2026-11-20T05:30:00Z".to_string(),
            fingerprint: "a1b2c3d4e5f6".to_string(),
            source: SourceInfo {
                app: "0.1.0".to_string(),
                os: "linux".to_string(),
                arch: "x64".to_string(),
                pi: None,
                node: None,
            },
            requirements: Requirements {
                node: REQUIREMENT_NODE.to_string(),
                pi: REQUIREMENT_PI.to_string(),
            },
            files: vec![ManifestFile {
                path: "snapshot/settings.json".to_string(),
                sha256: "ab".repeat(32),
                size: 1234,
                kind: "json".to_string(),
            }],
            symlinks: vec![ManifestSymlink {
                path: "snapshot/skills/antd".to_string(),
                target: "../../../.agents/skills/antd".to_string(),
            }],
            required_secrets: vec![RequiredSecret {
                name: "EXA_API_KEY".to_string(),
                used_by: vec!["snapshot/web-search.json".to_string()],
                hint: None,
            }],
            required_packages: vec![RequiredPackage {
                source: "npm:pi-web-access".to_string(),
                enabled: true,
            }],
            plaintext_secrets: false,
        };

        let text = serde_json::to_string_pretty(&manifest).expect("序列化");
        let value: serde_json::Value = serde_json::from_str(&text).expect("解析");
        // 字段名契约：与设计文档第 4 节一致。
        for key in [
            "formatVersion",
            "profileId",
            "createdAt",
            "fingerprint",
            "source",
            "requirements",
            "files",
            "symlinks",
            "requiredSecrets",
            "requiredPackages",
        ] {
            assert!(value.get(key).is_some(), "manifest 缺少字段 {key}");
        }
        assert!(
            value.get("plaintextSecrets").is_none(),
            "false 时不写该字段"
        );
        assert_eq!(value["files"][0]["sha256"].as_str().map(str::len), Some(64));

        let parsed: Manifest = serde_json::from_str(&text).expect("反序列化");
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn timestamp_is_rfc3339_utc() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_timestamp(946_684_800), "2000-01-01T00:00:00Z");
        assert_eq!(format_timestamp(1_767_225_600), "2026-01-01T00:00:00Z");
        assert_eq!(format_timestamp(1_795_000_000), "2026-11-18T11:06:40Z");
    }

    #[test]
    fn packages_support_string_and_object_forms() {
        let temp = TempDir::new("packages");
        let root = temp.path();
        write_file(
            &root.join("settings.json"),
            "{\"packages\": [\"npm:a\", {\"source\": \"npm:b\"}, {\"name\": \"npm:c\", \"enabled\": true}, {\"enabled\": true}, 42]}",
        );
        assert_eq!(
            declared_packages(root),
            vec![
                RequiredPackage {
                    source: "npm:a".to_string(),
                    enabled: true,
                },
                RequiredPackage {
                    source: "npm:b".to_string(),
                    enabled: true,
                },
                RequiredPackage {
                    source: "npm:c".to_string(),
                    enabled: true,
                },
            ]
        );

        // 没有 settings.json 或没有 packages 字段时为空，不阻断导出。
        let empty = TempDir::new("packages-empty");
        write_file(&empty.path().join("settings.json"), "{}");
        assert!(declared_packages(empty.path()).is_empty());
    }
}

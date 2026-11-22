//! 快照导出流水线。
//!
//! 流程来源：`docs/design/10-snapshot-sync.md` 第 5 节。
//! 顺序固定为：预检 → 物化到临时目录 → 只读密钥扫描 → 原子替换 → 最后写 manifest。
//! 任一步失败都删掉临时目录，原快照与 manifest 保持原样。
//!
//! `export_snapshot` 是 command 层入口；核心逻辑 [`export_with`] 只碰文件系统与给定的版本号，
//! 可直接用临时目录单测。

use crate::config::{self, AGENT_DIR_ENV_VAR};
use crate::doctor::{self, ShellRunner};
use crate::platform::{self, HostOs};
use crate::scope::{self, ManagedRoot, ManagedRoots, RootId};
use crate::secrets::{self, SecretFinding, SecretLevel};
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
pub const FORMAT_VERSION: u32 = 3;
/// 快照目录权限：含明文凭据，只允许本用户进入。
const SNAPSHOT_DIR_MODE: u32 = 0o700;
/// 凭据文件权限。
const AUTH_FILE_MODE: u32 = 0o600;
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
    /// 本快照受管的根 id。`agent` 始终在，`agents-skills` 只在源机存在 `~/.agents/skills` 时出现。
    ///
    /// 缺字段按空处理：`remote-state.json` 存的是整份 manifest，旧版本（v2）没有这个字段，
    /// 解析失败会让 WebDAV 上传/下载与定时上行全部停掉。快照本身依旧只按 `formatVersion` 拒绝旧格式。
    #[serde(default)]
    pub managed_roots: Vec<RootId>,
    pub created_at: String,
    pub fingerprint: String,
    pub source: SourceInfo,
    pub requirements: Requirements,
    pub files: Vec<ManifestFile>,
    pub symlinks: Vec<ManifestSymlink>,
    pub required_packages: Vec<RequiredPackage>,
    /// 快照含 auth.json 或扫描到高危明文；导入端据此给出警告。
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
    /// 本次写进 manifest 的受管根。
    pub managed_roots: Vec<RootId>,
    pub required_packages: Vec<RequiredPackage>,
    /// 断链等被跳过的条目数。
    pub skipped: usize,
    /// 只读扫描结果：不阻断，也没有自动改写。
    pub warnings: Vec<SecretFinding>,
    pub plaintext_secrets: bool,
    /// 根级别的告警：例如根是符号链接、外部根是断链。与密钥扫描分开。
    pub root_warnings: Vec<String>,
    pub snapshot_dir: PathBuf,
    pub manifest_path: PathBuf,
}

/// 导出失败。失败码见 `docs/design/10-snapshot-sync.md` 第 9 节。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportFailure {
    pub code: &'static str,
    pub message: String,
}

impl ExportFailure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
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
/// 这里留给单测：临时目录夹具不关心宿主版本，也没有外部根。
#[cfg(test)]
pub fn export(
    agent_dir: &Path,
    home_dir: &Path,
    sync_root: &Path,
) -> Result<ExportReport, ExportFailure> {
    export_with(
        agent_dir,
        Some(home_dir),
        sync_root,
        SourceVersions::default(),
    )
}

/// 与 [`export`] 相同，额外把探测到的 node/pi 版本写进 `manifest.source`。
/// `home_dir` 用来解析 `~/.agents/skills`：解析不到就报 `home_dir_missing`。
pub fn export_with(
    agent_dir: &Path,
    home_dir: Option<&Path>,
    sync_root: &Path,
    versions: SourceVersions,
) -> Result<ExportReport, ExportFailure> {
    export_with_options(agent_dir, home_dir, sync_root, versions, true)
}

/// 与 [`export_with`] 相同，但不复制根级 `auth.json`；自动上行默认用它。
pub fn export_without_auth(
    agent_dir: &Path,
    home_dir: Option<&Path>,
    sync_root: &Path,
    versions: SourceVersions,
) -> Result<ExportReport, ExportFailure> {
    export_with_options(agent_dir, home_dir, sync_root, versions, false)
}

fn export_with_options(
    agent_dir: &Path,
    home_dir: Option<&Path>,
    sync_root: &Path,
    versions: SourceVersions,
    include_auth: bool,
) -> Result<ExportReport, ExportFailure> {
    precheck_sync_root(sync_root)?;
    let mut root_warnings: Vec<String> = Vec::new();
    let roots = detect_roots(agent_dir, home_dir, &mut root_warnings)?;
    precheck_roots(&roots, sync_root)?;

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

    // 每个根物化到它在快照里的位置：agent 根占快照根，外部根落在 `external/<id>/`。
    let mut skipped = 0;
    let mut symlinks: Vec<(RootId, PathBuf, PathBuf)> = Vec::new();
    for root in roots.iter() {
        let dest = snapshot_root_dir(&temp_dir, root.id);
        let materialized = scope::materialize_root(root, &dest, include_auth).map_err(|error| {
            let _ = fs::remove_dir_all(&temp_dir);
            ExportFailure::new("export_failed", format!("复制受管文件失败：{error}"))
        })?;
        skipped += materialized.skipped.len();
        symlinks.extend(
            materialized
                .symlinks
                .into_iter()
                .map(|(path, target)| (root.id, path, target)),
        );
    }

    // 快照目录含明文凭据：目录只给本用户，auth.json 再收紧到 0600。
    platform::set_mode(&temp_dir, SNAPSHOT_DIR_MODE).map_err(|error| {
        let _ = fs::remove_dir_all(&temp_dir);
        ExportFailure::new(
            "no_write_permission",
            format!("设置快照目录权限失败：{error}"),
        )
    })?;
    let auth_path = temp_dir.join("auth.json");
    let auth_present = auth_path.is_file();
    if auth_present {
        platform::set_mode(&auth_path, AUTH_FILE_MODE).map_err(|error| {
            let _ = fs::remove_dir_all(&temp_dir);
            ExportFailure::new(
                "no_write_permission",
                format!("设置 auth.json 权限失败：{error}"),
            )
        })?;
    }

    // 扫描只读：命中只用于告警，不改写快照内容。
    let secret_findings = secrets::scan_snapshot(&temp_dir).map_err(|error| {
        let _ = fs::remove_dir_all(&temp_dir);
        ExportFailure::new("export_failed", format!("密钥扫描失败：{error}"))
    })?;
    let has_high = secret_findings
        .iter()
        .any(|finding| finding.level == SecretLevel::High);
    let plaintext_secrets = auth_present || has_high;
    let warnings: Vec<SecretFinding> = secret_findings
        .into_iter()
        .filter(|finding| finding.path != "snapshot/auth.json")
        .collect();

    let collected = collect_snapshot_files(&roots, &temp_dir).map_err(|error| {
        let _ = fs::remove_dir_all(&temp_dir);
        ExportFailure::new("export_failed", format!("读取临时快照失败：{error}"))
    })?;
    let required_packages = declared_packages(&temp_dir);
    let files: Vec<ManifestFile> = collected
        .iter()
        .map(|file| ManifestFile {
            path: manifest_path(file.root, &file.relative),
            sha256: file.sha256.clone(),
            size: file.size,
            kind: file.kind.to_string(),
        })
        .collect();

    let managed_roots = roots.ids();
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        profile_id: PROFILE_ID.to_string(),
        managed_roots: managed_roots.clone(),
        created_at: format_timestamp(now_seconds()),
        fingerprint: fingerprint(FORMAT_VERSION, &managed_roots, &files),
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
        symlinks: symlinks
            .into_iter()
            .map(|(root, path, target)| ManifestSymlink {
                path: manifest_path(root, &path),
                target: target.to_string_lossy().into_owned(),
            })
            .collect(),
        required_packages,
        plaintext_secrets,
    };

    let fingerprint = manifest.fingerprint.clone();
    swap_in(temp_dir, sync_root, stamp, &manifest)?;

    Ok(ExportReport {
        files: manifest.files.len(),
        fingerprint,
        managed_roots,
        required_packages: manifest.required_packages,
        skipped,
        warnings,
        root_warnings,
        plaintext_secrets,
        snapshot_dir: sync_root.join(SNAPSHOT_DIR_NAME),
        manifest_path: sync_root.join(MANIFEST_FILE_NAME),
    })
}

/// 源机的受管根。
///
/// `agent` 根始终声明，不可用就是错误；`agents-skills` 根只看源机有没有 `<home>/.agents/skills`：
/// 不存在就不声明（目标机完全不读、不写、不删），存在（含空目录）就声明，不可读或不是目录则导出失败。
fn detect_roots(
    agent_dir: &Path,
    home_dir: Option<&Path>,
    warnings: &mut Vec<String>,
) -> Result<ManagedRoots, ExportFailure> {
    // 导出与应用都要用 home 解析 `~/.agents/skills`：解析不到就说不清该不该声明该根，
    // 静默降级成 agent-only 会让另一台机器把外部技能当成不存在。
    let home_dir = home_dir.ok_or_else(|| {
        ExportFailure::new(
            "home_dir_missing",
            "无法确定 home 目录，无法判断是否需要声明 ~/.agents/skills；已停止导出",
        )
    })?;

    // 根的类型不进入快照：链接只跟随内容，导出与应用各给一条告警。
    if platform::is_link_like(agent_dir) {
        warnings.push(format!(
            "{} 是符号链接，只同步其内容，不记录根链接",
            agent_dir.display()
        ));
    }

    let mut roots = vec![ManagedRoot::agent(agent_dir)];
    let root = ManagedRoot::agents_skills(home_dir);
    if inspect_agents_skills(&root.path, warnings)? {
        roots.push(root);
    }
    Ok(ManagedRoots::new(roots))
}

/// `Ok(true)` 声明该根，`Ok(false)` 不声明。
///
/// 根的类型（真目录 / 符号链接）不进入快照：是链接就只跟随内容、不重建根链接；
/// 断链按不存在处理并告警。
fn inspect_agents_skills(path: &Path, warnings: &mut Vec<String>) -> Result<bool, ExportFailure> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(ExportFailure::new(
                "external_root_unreadable",
                format!("无法读取 {}：{error}", path.display()),
            ))
        }
    };

    if platform::is_link_like(path) {
        match fs::metadata(path) {
            Ok(target) if target.is_dir() => warnings.push(format!(
                "{} 是符号链接，只同步其内容，不记录根链接",
                path.display()
            )),
            Ok(_) => return Err(external_root_invalid(path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                warnings.push(format!(
                    "{} 是断链，按不存在处理，本次不声明该根",
                    path.display()
                ));
                return Ok(false);
            }
            Err(error) => {
                return Err(ExportFailure::new(
                    "external_root_unreadable",
                    format!("无法读取 {}：{error}", path.display()),
                ))
            }
        }
    } else if !metadata.is_dir() {
        return Err(external_root_invalid(path));
    }

    // 目录存在但读不出内容时导出会得到半套快照，宁可先失败。
    fs::read_dir(path).map_err(|error| {
        ExportFailure::new(
            "external_root_unreadable",
            format!("无法读取 {}：{error}", path.display()),
        )
    })?;
    Ok(true)
}

fn external_root_invalid(path: &Path) -> ExportFailure {
    ExportFailure::new(
        "external_root_invalid",
        format!("{} 存在但不是目录，无法作为受管根同步", path.display()),
    )
}

/// 预检：同步文件夹路径规范且存在。
fn precheck_sync_root(sync_root: &Path) -> Result<(), ExportFailure> {
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
    Ok(())
}

/// 预检：受管根可用，且都不与同步文件夹互相嵌套。
fn precheck_roots(roots: &ManagedRoots, sync_root: &Path) -> Result<(), ExportFailure> {
    let agent_dir = roots.path(RootId::Agent).ok_or_else(|| {
        ExportFailure::new("agent_dir_missing", "无法确定宿主目录，请先在设置页填写")
    })?;
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
    for root in roots.iter() {
        let canonical_root = fs::canonicalize(&root.path).map_err(|error| {
            let code = match root.id {
                RootId::Agent => "agent_dir_missing",
                RootId::AgentsSkills => "external_root_unreadable",
            };
            ExportFailure::new(code, format!("无法访问 {}：{error}", root.path.display()))
        })?;
        if canonical_sync.starts_with(&canonical_root)
            || canonical_root.starts_with(&canonical_sync)
        {
            let (code, message) = match root.id {
                RootId::Agent => (
                    "sync_root_overlaps_agent_dir",
                    "同步文件夹与宿主目录不能相同，也不能互相嵌套",
                ),
                RootId::AgentsSkills => (
                    "sync_root_overlaps_external_root",
                    "同步文件夹与 ~/.agents/skills 不能相同，也不能互相嵌套",
                ),
            };
            return Err(ExportFailure::new(code, message));
        }
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

/// 快照指纹：`formatVersion`、排序后的 `managedRoots` 与按 path 排序的 `{path, sha256}` 一起取
/// SHA-256，取前 12 位。受管根不同（例如一边声明了空的外部根、另一边没声明）必须指纹不同。
pub fn fingerprint(
    format_version: u32,
    managed_roots: &[RootId],
    files: &[ManifestFile],
) -> String {
    let mut roots: Vec<&str> = managed_roots.iter().map(|id| id.as_str()).collect();
    roots.sort_unstable();
    let mut sorted: Vec<&ManifestFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));

    let mut hasher = Sha256::new();
    hasher.update(format!("{format_version}\n").as_bytes());
    for root in roots {
        hasher.update(root.as_bytes());
        hasher.update([0u8]);
    }
    hasher.update(b"\n");
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

/// 根内相对路径在容器里的虚拟路径：agent 根就是相对路径本身，外部根带 `external/<id>/` 前缀。
/// 备份目录直接用这个布局，manifest 路径再在前面加 `snapshot/`。
pub fn virtual_path(root: RootId, relative: &Path) -> String {
    let mut path = String::from(root.snapshot_prefix());
    for component in relative.components() {
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(&component.as_os_str().to_string_lossy());
    }
    path
}

/// 一个根内相对路径的 manifest 路径，形如 `snapshot/settings.json` 或
/// `snapshot/external/agents-skills/foo/SKILL.md`。
pub fn manifest_path(root: RootId, relative: &Path) -> String {
    format!("{SNAPSHOT_DIR_NAME}/{}", virtual_path(root, relative))
}

/// manifest 路径 → （所属根, 根内相对路径）。不在 `snapshot/` 之下或为空时返回 `None`。
pub fn split_manifest_path(path: &str) -> Option<(RootId, PathBuf)> {
    let rest = path
        .strip_prefix(SNAPSHOT_DIR_NAME)
        .and_then(|rest| rest.strip_prefix('/'))?;
    if rest.is_empty() {
        return None;
    }

    let prefix = format!("{}/", RootId::AgentsSkills.snapshot_prefix());
    if let Some(relative) = rest.strip_prefix(&prefix) {
        return (!relative.is_empty()).then(|| (RootId::AgentsSkills, PathBuf::from(relative)));
    }
    // agent 根占快照根：它的白名单里没有 `external/`，所以其余路径都归它。
    Some((RootId::Agent, PathBuf::from(rest)))
}

/// 某个根在快照目录里的位置：agent 根就是快照根，外部根在 `external/<id>/`。
fn snapshot_root_dir(snapshot_dir: &Path, id: RootId) -> PathBuf {
    let prefix = id.snapshot_prefix();
    if prefix.is_empty() {
        snapshot_dir.to_path_buf()
    } else {
        snapshot_dir.join(prefix)
    }
}

struct CollectedFile {
    root: RootId,
    /// 根内相对路径。
    relative: PathBuf,
    size: u64,
    sha256: String,
    kind: &'static str,
}

/// 逐根读临时快照，算出每个文件的哈希与 manifest 路径。
fn collect_snapshot_files(
    roots: &ManagedRoots,
    snapshot_dir: &Path,
) -> io::Result<Vec<CollectedFile>> {
    let mut files = Vec::new();
    for root in roots.iter() {
        let base = snapshot_root_dir(snapshot_dir, root.id);
        if base.is_dir() {
            collect_root(root.id, &base, Path::new(""), &mut files)?;
        }
    }
    files.sort_by(|left, right| {
        left.root
            .cmp(&right.root)
            .then_with(|| left.relative.cmp(&right.relative))
    });
    Ok(files)
}

fn collect_root(
    root: RootId,
    base: &Path,
    relative: &Path,
    files: &mut Vec<CollectedFile>,
) -> io::Result<()> {
    let mut names: Vec<OsString> = Vec::new();
    for item in fs::read_dir(base.join(relative))? {
        names.push(item?.file_name());
    }
    names.sort();

    for name in names {
        // `external/` 是其它受管根在快照里的位置，不属于 agent 根的内容。
        if root == RootId::Agent && relative.as_os_str().is_empty() && name == "external" {
            continue;
        }
        let child = relative.join(&name);
        let metadata = fs::metadata(base.join(&child))?;
        if metadata.is_dir() {
            collect_root(root, base, &child, files)?;
        } else if metadata.is_file() {
            let bytes = fs::read(base.join(&child))?;
            files.push(CollectedFile {
                root,
                relative: child.clone(),
                size: metadata.len(),
                sha256: sha256_hex(&bytes),
                kind: kind_for(&child, &bytes),
            });
        }
    }
    Ok(())
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
pub async fn export_snapshot(app: tauri::AppHandle) -> Result<ExportReport, ExportFailure> {
    // 版本探测要起子进程，最多各等 5 秒；放阻塞线程池，别占住 UI 线程。
    tauri::async_runtime::spawn_blocking(move || export_snapshot_blocking(&app))
        .await
        .map_err(|error| ExportFailure::new("export_failed", format!("导出失败：{error}")))?
}

fn export_snapshot_blocking(app: &tauri::AppHandle) -> Result<ExportReport, ExportFailure> {
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
        home_dir.as_deref(),
        &sync_root,
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
        // 根级 auth.json 进快照；sessions 仍在范围外。
        write_file(
            &root.join("auth.json"),
            "{\"openai\": {\"key\": \"sk-secret\"}}",
        );
        write_file(&root.join("sessions/session.json"), "{}");
    }

    /// 单根夹具的 home：存在但没有 `~/.agents/skills`，导出只声明 agent 根。
    fn home_without_skills(temp: &TempDir) -> PathBuf {
        let home = temp.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        home
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
        let home = home_without_skills(&temp);
        export_with(&agent, Some(&home), &sync_root, versions).expect("导出应成功");

        let manifest = read_manifest(&sync_root);
        assert_eq!(manifest.source.pi.as_deref(), Some("1.6.0"));
        assert_eq!(manifest.source.node.as_deref(), Some("24.14.0"));

        // 探测不到时留空：导出照常成功，manifest 里是 null。
        export(&agent, &home, &sync_root).expect("导出应成功");
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

        let report = export(&agent, &home_without_skills(&temp), &sync_root).expect("导出应成功");
        assert_eq!(report.files, 6, "{:?}", report);
        assert_eq!(report.fingerprint.len(), 12);
        assert!(report.fingerprint.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert!(report.plaintext_secrets, "auth.json 与字面密钥都应在快照里");

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
                "snapshot/auth.json",
                "snapshot/models.json",
                "snapshot/settings.json",
                "snapshot/skills/antd/SKILL.md",
                "snapshot/web-search.json",
            ]
        );
        verify_files(&sync_root, &manifest.files).expect("自检哈希");

        // 指纹只由 formatVersion、managedRoots 与 {path, sha256} 决定，可复算。
        assert_eq!(
            fingerprint(FORMAT_VERSION, &manifest.managed_roots, &manifest.files),
            report.fingerprint
        );
        assert_eq!(manifest.managed_roots, vec![RootId::Agent]);
        assert_eq!(report.managed_roots, vec![RootId::Agent]);

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

        // 文件原样保留：字面密钥与 auth.json 都进入快照，不再引用化。
        let models = fs::read_to_string(sync_root.join("snapshot/models.json")).expect("读取快照");
        assert!(models.contains(SECRET), "{models}");
        assert!(!models.contains("$PI_OPENROUTER_API_KEY"), "{models}");
        assert!(sync_root.join("snapshot/auth.json").is_file());
        assert!(!sync_root.join("snapshot/sessions").exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let auth_mode = fs::metadata(sync_root.join("snapshot/auth.json"))
                .expect("auth 元数据")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(auth_mode, 0o600, "快照里的 auth.json 必须是 0600");
            let dir_mode = fs::metadata(sync_root.join("snapshot"))
                .expect("快照目录元数据")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700, "快照目录必须是 0700");
        }

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

        let expected = fingerprint(FORMAT_VERSION, &[RootId::Agent], &files);
        assert_eq!(
            expected,
            fingerprint(FORMAT_VERSION, &[RootId::Agent], &reversed)
        );
        assert_eq!(expected.len(), 12);

        let mut changed = files.clone();
        changed[0].sha256 = "cc".repeat(32);
        assert_ne!(
            expected,
            fingerprint(FORMAT_VERSION, &[RootId::Agent], &changed)
        );
        assert_ne!(
            expected,
            fingerprint(FORMAT_VERSION + 1, &[RootId::Agent], &files)
        );
        // 受管根不同必须指纹不同：一边声明了（可为空的）外部根，另一边没有。
        let both = [RootId::Agent, RootId::AgentsSkills];
        assert_ne!(expected, fingerprint(FORMAT_VERSION, &both, &files));
        // 根顺序不影响指纹。
        assert_eq!(
            fingerprint(FORMAT_VERSION, &both, &files),
            fingerprint(
                FORMAT_VERSION,
                &[RootId::AgentsSkills, RootId::Agent],
                &files
            )
        );
    }

    #[test]
    fn hash_mismatch_is_reported() {
        let temp = TempDir::new("hash");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fixture_agent(&agent);
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        export(&agent, &home_without_skills(&temp), &sync_root).expect("导出应成功");
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
    fn second_export_replaces_snapshot_without_temp_leftovers() {
        let temp = TempDir::new("atomic");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fixture_agent(&agent);
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        let home = home_without_skills(&temp);
        export(&agent, &home, &sync_root).expect("首次导出应成功");

        // 第二次：新增的扩展文件也进入快照，旧快照整体被替换。
        write_file(
            &agent.join("extensions/leak/index.ts"),
            &format!("const apiKey = \"sk-{SECRET}\";\n"),
        );
        let report = export(&agent, &home, &sync_root).expect("第二次导出应成功");
        assert!(report.plaintext_secrets);
        assert!(sync_root
            .join("snapshot/extensions/leak/index.ts")
            .is_file());
        assert!(read_manifest(&sync_root).plaintext_secrets);
        assert_eq!(dir_entries(&sync_root), vec!["manifest.json", "snapshot"]);
    }

    #[test]
    fn nested_sync_root_is_rejected() {
        let temp = TempDir::new("nested");
        let agent = temp.path().join("agent");
        fixture_agent(&agent);
        let home = home_without_skills(&temp);

        let inside = agent.join("cloud");
        fs::create_dir_all(&inside).expect("创建嵌套同步目录");
        let failure = export(&agent, &home, &inside).expect_err("嵌套应被拒绝");
        assert_eq!(failure.code, "sync_root_overlaps_agent_dir");

        let missing = temp.path().join("missing");
        let failure = export(&agent, &home, &missing).expect_err("缺失应被拒绝");
        assert_eq!(failure.code, "sync_root_missing");

        let failure =
            export(&agent, &home, Path::new("relative/path")).expect_err("相对路径应被拒绝");
        assert_eq!(failure.code, "sync_root_invalid");
    }

    /// `~/.agents/skills` 夹具：与 agent 目录平铺在同一个临时 home 下。
    fn fixture_home(temp: &TempDir, fill: impl FnOnce(&Path)) -> PathBuf {
        let home = temp.path().join("home");
        let skills = home.join(".agents/skills");
        fs::create_dir_all(&skills).expect("创建外部根");
        fill(&skills);
        home
    }

    #[test]
    fn export_declares_agents_skills_and_puts_its_files_under_external() {
        let temp = TempDir::new("external");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fixture_agent(&agent);
        let home = fixture_home(&temp, |skills| {
            write_file(&skills.join("antd/SKILL.md"), "# 共享技能\n");
            write_file(&skills.join("auth.json"), "{}");
        });
        fs::create_dir_all(&sync_root).expect("创建同步目录");

        let report = export_with(&agent, Some(&home), &sync_root, SourceVersions::default())
            .expect("导出应成功");
        assert_eq!(
            report.managed_roots,
            vec![RootId::Agent, RootId::AgentsSkills]
        );

        let manifest = read_manifest(&sync_root);
        assert_eq!(
            manifest.managed_roots,
            vec![RootId::Agent, RootId::AgentsSkills]
        );
        let paths: Vec<String> = manifest
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect();
        assert!(
            paths.contains(&"snapshot/external/agents-skills/antd/SKILL.md".to_string()),
            "{paths:?}"
        );
        // 外部根里的 auth.json 不是凭据位置，不进快照。
        assert!(
            !paths.contains(&"snapshot/external/agents-skills/auth.json".to_string()),
            "{paths:?}"
        );
        assert_eq!(
            fs::read_to_string(sync_root.join("snapshot/external/agents-skills/antd/SKILL.md"))
                .expect("读取快照里的外部根文件"),
            "# 共享技能\n"
        );

        // 声明与不声明外部根是两份不同的快照。
        let agent_only = TempDir::new("external-agent-only");
        let agent_only_root = agent_only.path().join("cloud");
        fs::create_dir_all(&agent_only_root).expect("创建同步目录");
        let plain = export(&agent, &home_without_skills(&agent_only), &agent_only_root)
            .expect("导出 agent-only 快照");
        assert_eq!(plain.managed_roots, vec![RootId::Agent]);
        assert_ne!(plain.fingerprint, report.fingerprint);
    }

    #[test]
    fn empty_agents_skills_dir_is_still_declared() {
        let temp = TempDir::new("external-empty");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fixture_agent(&agent);
        let home = fixture_home(&temp, |_| {});
        fs::create_dir_all(&sync_root).expect("创建同步目录");

        let report = export_with(&agent, Some(&home), &sync_root, SourceVersions::default())
            .expect("导出应成功");
        assert_eq!(
            report.managed_roots,
            vec![RootId::Agent, RootId::AgentsSkills]
        );
        // 空目录也要在快照里出现，目标机才能把多出的技能列为删除候选。
        assert!(sync_root.join("snapshot/external/agents-skills").is_dir());
        assert!(read_manifest(&sync_root)
            .managed_roots
            .contains(&RootId::AgentsSkills));
    }

    #[test]
    fn missing_agents_skills_dir_produces_no_external_paths() {
        let temp = TempDir::new("external-missing");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fixture_agent(&agent);
        let home = temp.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        fs::create_dir_all(&sync_root).expect("创建同步目录");

        let report = export_with(&agent, Some(&home), &sync_root, SourceVersions::default())
            .expect("导出应成功");
        assert_eq!(report.managed_roots, vec![RootId::Agent]);
        assert!(!sync_root.join("snapshot/external").exists());
    }

    #[test]
    fn agents_skills_that_is_not_a_directory_fails_the_export() {
        let temp = TempDir::new("external-file");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fixture_agent(&agent);
        let home = temp.path().join("home");
        write_file(&home.join(".agents/skills"), "not a dir");
        fs::create_dir_all(&sync_root).expect("创建同步目录");

        let failure = export_with(&agent, Some(&home), &sync_root, SourceVersions::default())
            .expect_err("不是目录应导出失败");
        assert_eq!(failure.code, "external_root_invalid");
        assert!(!sync_root.join("snapshot").exists(), "失败不留下半套快照");
    }

    #[cfg(unix)]
    #[test]
    fn agents_skills_symlink_is_followed_and_reported() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("external-link");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fixture_agent(&agent);
        let home = temp.path().join("home");
        let shared = temp.path().join("shared-skills");
        write_file(&shared.join("antd/SKILL.md"), "# 共享技能\n");
        fs::create_dir_all(home.join(".agents")).expect("创建 .agents");
        symlink(&shared, home.join(".agents/skills")).expect("建外部根链接");
        fs::create_dir_all(&sync_root).expect("创建同步目录");

        let report = export_with(&agent, Some(&home), &sync_root, SourceVersions::default())
            .expect("导出应成功");
        // 根的类型不进快照：只跟内容，不记录根链接。
        assert_eq!(
            read_manifest(&sync_root).managed_roots,
            vec![RootId::Agent, RootId::AgentsSkills]
        );
        assert!(sync_root
            .join("snapshot/external/agents-skills/antd/SKILL.md")
            .is_file());
        assert_eq!(report.root_warnings.len(), 1, "{:?}", report.root_warnings);
        assert!(report.root_warnings[0].contains("符号链接"));
    }

    #[cfg(unix)]
    #[test]
    fn broken_agents_skills_symlink_is_treated_as_missing() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("external-broken");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fixture_agent(&agent);
        let home = temp.path().join("home");
        fs::create_dir_all(home.join(".agents")).expect("创建 .agents");
        symlink("nowhere", home.join(".agents/skills")).expect("建断链");
        fs::create_dir_all(&sync_root).expect("创建同步目录");

        let report = export_with(&agent, Some(&home), &sync_root, SourceVersions::default())
            .expect("断链不应导致导出失败");
        assert_eq!(report.managed_roots, vec![RootId::Agent]);
        assert_eq!(report.root_warnings.len(), 1, "{:?}", report.root_warnings);
        assert!(report.root_warnings[0].contains("断链"));
        assert!(!sync_root.join("snapshot/external").exists());
    }

    #[test]
    fn sync_root_nested_with_agents_skills_is_rejected() {
        let temp = TempDir::new("external-nested");
        let agent = temp.path().join("agent");
        fixture_agent(&agent);
        let home = fixture_home(&temp, |_| {});
        let inside = home.join(".agents/skills/cloud");
        fs::create_dir_all(&inside).expect("创建嵌套同步目录");

        let failure = export_with(&agent, Some(&home), &inside, SourceVersions::default())
            .expect_err("嵌套应被拒绝");
        assert_eq!(failure.code, "sync_root_overlaps_external_root");
    }

    #[test]
    fn export_without_a_home_fails_instead_of_falling_back_to_agent_only() {
        let temp = TempDir::new("no-home");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fixture_agent(&agent);
        fs::create_dir_all(&sync_root).expect("创建同步目录");

        let failure = export_with(&agent, None, &sync_root, SourceVersions::default())
            .expect_err("解析不到 home 应导出失败");
        assert_eq!(failure.code, "home_dir_missing");
        assert!(
            !sync_root.join(SNAPSHOT_DIR_NAME).exists(),
            "失败不留下半套快照"
        );
    }

    #[cfg(unix)]
    #[test]
    fn agent_root_symlink_is_followed_and_reported() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("agent-root-link");
        let real = temp.path().join("real-agent");
        fixture_agent(&real);
        let agent = temp.path().join("agent");
        symlink(&real, &agent).expect("建根链接");
        let home = home_without_skills(&temp);
        let sync_root = temp.path().join("cloud");
        fs::create_dir_all(&sync_root).expect("创建同步目录");

        let report = export(&agent, &home, &sync_root).expect("导出应成功");
        // 根的类型不进快照：内容跟随链接读出来，但也要告警让人知道写入会落到链接目标。
        assert_eq!(report.root_warnings.len(), 1, "{:?}", report.root_warnings);
        assert!(report.root_warnings[0].contains("符号链接"));
        assert!(sync_root.join("snapshot/settings.json").is_file());
        assert!(
            read_manifest(&sync_root).symlinks.is_empty(),
            "根链接不进 manifest"
        );
    }

    #[test]
    fn manifest_paths_map_back_to_roots() {
        assert_eq!(
            manifest_path(RootId::Agent, Path::new("skills/antd/SKILL.md")),
            "snapshot/skills/antd/SKILL.md"
        );
        assert_eq!(
            manifest_path(RootId::AgentsSkills, Path::new("antd/SKILL.md")),
            "snapshot/external/agents-skills/antd/SKILL.md"
        );
        assert_eq!(
            split_manifest_path("snapshot/settings.json"),
            Some((RootId::Agent, PathBuf::from("settings.json")))
        );
        assert_eq!(
            split_manifest_path("snapshot/external/agents-skills/antd/SKILL.md"),
            Some((RootId::AgentsSkills, PathBuf::from("antd/SKILL.md")))
        );
        assert_eq!(split_manifest_path("manifest.json"), None);
        assert_eq!(split_manifest_path("snapshot/"), None);
        assert_eq!(
            split_manifest_path("snapshot/external/agents-skills/"),
            None
        );
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let manifest = Manifest {
            format_version: FORMAT_VERSION,
            profile_id: PROFILE_ID.to_string(),
            managed_roots: vec![RootId::Agent, RootId::AgentsSkills],
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
            "managedRoots",
            "createdAt",
            "fingerprint",
            "source",
            "requirements",
            "files",
            "symlinks",
            "requiredPackages",
        ] {
            assert!(value.get(key).is_some(), "manifest 缺少字段 {key}");
        }
        // 受管根按 id 序列化，前端按同一套拼写解析。
        assert_eq!(
            value["managedRoots"],
            serde_json::json!(["agent", "agents-skills"])
        );
        assert!(
            value.get("plaintextSecrets").is_none(),
            "false 时不写该字段"
        );
        assert_eq!(value["files"][0]["sha256"].as_str().map(str::len), Some(64));

        let parsed: Manifest = serde_json::from_str(&text).expect("反序列化");
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn manifest_without_managed_roots_still_deserializes() {
        // v2 快照没有 `managedRoots`。这个字段必须能缺省：`remote-state.json` 里存的是整份
        // manifest，升级后解析失败会让 WebDAV 上传/下载与定时上行全部停掉。
        // 快照本身依旧靠 `formatVersion` 拒绝旧格式（见 `plan::read_manifest`）。
        let text = r#"{
            "formatVersion": 2,
            "profileId": "default",
            "createdAt": "2026-11-18T11:06:40Z",
            "fingerprint": "a1b2c3d4e5f6",
            "source": { "app": "0.0.1", "os": "linux", "arch": "x64", "pi": null, "node": null },
            "requirements": { "node": ">=22.19.0", "pi": ">=1.5.0" },
            "files": [],
            "symlinks": [],
            "requiredPackages": []
        }"#;

        let manifest: Manifest = serde_json::from_str(text).expect("旧 manifest 应能解析");
        assert_eq!(manifest.format_version, 2);
        assert!(manifest.managed_roots.is_empty());
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

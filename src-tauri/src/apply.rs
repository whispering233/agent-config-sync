//! 应用快照：先备份再落地，删除需逐项确认。
//!
//! 规则来源：`docs/design/10-snapshot-sync.md` 第 6、7 节。
//! 顺序固定：重算计划 → 核对指纹与删除清单 → 备份 → 重建链接 → 写入/删除 → 清理旧备份。
//! 符号链接优先按 manifest 的 `symlinks` 重建；目标不存在（含断链）才退回物化写入。
//! 失败停在原地：写入中途失败即停，已完成的部分留在原地，备份目录保留（回滚是 T8），
//! 不自动回滚、不重试、不静默修复。核心 [`apply`] 不依赖 Tauri，可直接临时目录单测。

use crate::config;
use crate::doctor::{CommandRunner, ShellRunner};
use crate::plan;
use crate::scope;
use crate::snapshot::{ManifestSymlink, MANIFEST_FILE_NAME, SNAPSHOT_DIR_NAME};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

/// 备份目录名（位于应用数据目录下，不进云盘）。
pub const BACKUPS_DIR_NAME: &str = "backups";
/// 备份目录内存放原文件内容的子目录。
pub const BACKUP_FILES_DIR_NAME: &str = "files";
/// 备份说明文件名。
pub const RESTORE_FILE_NAME: &str = "restore.json";
/// `restore.json` 的格式版本。T8 按这个版本读取。
pub const RESTORE_VERSION: u32 = 1;
/// 保留最近几份备份。
pub const BACKUP_RETENTION: usize = 10;
/// 备份目录权限：只允许本用户读。
const BACKUP_DIR_MODE: u32 = 0o700;
/// 新增文件的权限；覆盖已有文件时沿用原 mode。
const NEW_FILE_MODE: u32 = 0o644;
/// 符号链接只在 Unix 上重建；其他平台退回物化写入并给出提示。
const SYMLINK_SUPPORTED: bool = cfg!(unix);

/// 一次应用的输出。失败时改由 [`ApplyFailure`] 返回，`failed_at` 因此总是 `None`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyReport {
    pub fingerprint: String,
    pub backup_dir: PathBuf,
    pub backup_id: String,
    /// 新增 + 修改的条数。
    pub written: usize,
    /// 实际删除的条数。
    pub deleted: usize,
    /// 删除候选里未勾选、因此保留下来的条数。
    pub skipped: usize,
    /// 按 manifest 记录重建的符号链接条数。
    pub linked: usize,
    /// 已勾选、但落在重建链接之下因此没有删除的条数。
    pub skipped_deletes: usize,
    /// 不阻断的提示：链接退回物化、链接下的删除未执行等。
    pub warnings: Vec<String>,
    pub failed_at: Option<String>,
}

/// 应用失败。失败码见 `docs/design/10-snapshot-sync.md` 第 9 节与任务卡。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyFailure {
    pub code: &'static str,
    pub message: String,
    /// 失败时已经落地的条目数（新增 + 修改 + 删除）。
    pub completed: usize,
    /// 失败时备份目录的位置；备份之前就失败则为 `None`。
    pub backup_dir: Option<PathBuf>,
}

impl ApplyFailure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            completed: 0,
            backup_dir: None,
        }
    }
}

/// 备份条目的类型。字段名与 `restore.json` 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreKind {
    File,
    Symlink,
    /// 本卡不产生目录条目：计划项都是文件，目标位置反而是目录时直接失败。
    Dir,
}

/// 备份里的一条：原始路径、类型、原权限、链接目标。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreEntry {
    /// 相对宿主目录的路径，`/` 分隔。
    pub path: String,
    pub kind: RestoreKind,
    /// 应用前该路径是否存在；新增文件为 false，此时没有内容备份。
    pub existed: bool,
    pub mode: Option<u32>,
    pub link_target: Option<String>,
}

/// 备份说明。T8 只读这个文件就能还原。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreManifest {
    pub version: u32,
    pub created_at: String,
    /// 本次应用所依据的快照指纹。
    pub fingerprint: String,
    pub entries: Vec<RestoreEntry>,
}

struct Backup {
    id: String,
    dir: PathBuf,
    created_at: String,
}

/// core：应用快照。`delete_paths` 是用户勾选确认的删除项，其余删除候选原样保留。
pub fn apply(
    agent_dir: &Path,
    sync_root: &Path,
    expected_fingerprint: &str,
    delete_paths: &[String],
    app_data_dir: &Path,
    runner: &dyn CommandRunner,
) -> Result<ApplyReport, ApplyFailure> {
    // 计划可能已经过时：用户看的是旧计划，落地前必须按当前磁盘重算一次。
    let report = plan::plan_snapshot(agent_dir, sync_root, runner).map_err(|failure| {
        ApplyFailure::new(
            "manifest_invalid",
            format!("快照校验未通过（{}）：{}", failure.code, failure.message),
        )
    })?;
    if report.manifest_info.fingerprint != expected_fingerprint {
        return Err(ApplyFailure::new(
            "plan_stale",
            format!(
                "快照指纹已变化：确认计划时是 {expected_fingerprint}，现在是 {}；请重新检查后再应用",
                report.manifest_info.fingerprint
            ),
        ));
    }
    let mut warnings: Vec<String> = Vec::new();

    let confirmed = confirm_deletes(agent_dir, &report.plan, delete_paths)?;
    // 导出时链接被物化成内容，落地时优先按 manifest 的记录重建。
    let recorded_links = manifest_links(sync_root)?;
    let links = rebuildable_links(agent_dir, &recorded_links, &mut warnings);
    let rebuilt: Vec<&str> = links.iter().map(|link| link.relative.as_str()).collect();
    // 删除会跟随祖先目录里的符号链接落到外部位置；将被重建的链接不算，其余必须在建备份前整次拒绝。
    reject_delete_through_symlink(agent_dir, &confirmed, &rebuilt)?;
    let skipped = report.plan.deleted.len() - confirmed.len();

    let backup = create_backup_dir(app_data_dir)?;
    write_backup(
        agent_dir,
        &report.plan,
        &report.manifest_info.fingerprint,
        &confirmed,
        &backup,
    )?;
    prune(&app_data_dir.join(BACKUPS_DIR_NAME));

    let mut linked = 0;
    for link in &links {
        rebuild_link(agent_dir, link)
            .map_err(|error| fail(&backup, linked, &link.relative, error))?;
        linked += 1;
    }

    let mut written = 0;
    for item in report.plan.added.iter().chain(&report.plan.modified) {
        // 链接之下的内容由链接目标提供，不再写真实文件（也不写穿链接）。
        if links.iter().any(|link| is_under(&item.path, &link.relative)) {
            continue;
        }
        write_snapshot_file(agent_dir, sync_root, &item.path)
            .map_err(|error| fail(&backup, linked + written, &item.path, error))?;
        written += 1;
    }

    let mut deleted = 0;
    let mut skipped_deletes = 0;
    for relative in &confirmed {
        if let Some(link) = links.iter().find(|link| is_under(relative, &link.relative)) {
            skipped_deletes += 1;
            warnings.push(format!(
                "{relative} 由链接 {} 的目标提供，未删除",
                link.relative
            ));
            continue;
        }
        delete_target(agent_dir, relative)
            .map_err(|error| fail(&backup, linked + written + deleted, relative, error))?;
        deleted += 1;
    }

    Ok(ApplyReport {
        fingerprint: report.manifest_info.fingerprint,
        backup_dir: backup.dir,
        backup_id: backup.id,
        written,
        deleted,
        skipped,
        linked,
        skipped_deletes,
        warnings,
        failed_at: None,
    })
}

fn fail(backup: &Backup, completed: usize, path: &str, error: io::Error) -> ApplyFailure {
    ApplyFailure {
        code: "apply_failed",
        message: format!(
            "落地 {path} 时失败：{error}；已立即停止，备份在 {}",
            backup.dir.display()
        ),
        completed,
        backup_dir: Some(backup.dir.clone()),
    }
}

/// 删除清单校验：每一项都必须是重算计划里的删除候选，且落在受管范围内。
/// 任何一项不合格就整次拒绝，不做部分执行。
fn confirm_deletes(
    agent_dir: &Path,
    plan: &plan::Plan,
    delete_paths: &[String],
) -> Result<Vec<String>, ApplyFailure> {
    let mut confirmed: Vec<String> = Vec::new();
    for path in delete_paths {
        // 计划里的删除候选只来自受管范围；`resolve_within` 再挡一次空路径、`..` 与绝对路径。
        let in_plan = plan.deleted.iter().any(|item| &item.path == path);
        if !in_plan || scope::resolve_within(agent_dir, Path::new(path)).is_err() {
            return Err(ApplyFailure::new(
                "delete_not_in_plan",
                format!("{path} 不在本次计划的删除候选里，或不在受管范围内；整次应用已拒绝"),
            ));
        }
        if !confirmed.contains(path) {
            confirmed.push(path.clone());
        }
    }
    Ok(confirmed)
}

/// 删除前置校验：待删路径的祖先目录（不含自身）是符号链接时整次拒绝。
/// 将被重建的 manifest 链接（`rebuilt`）不算：它们会指回原目标，链接下的删除改由跳过处理。
/// 此时还没有建备份、没有写任何文件，用户手动处理链接后可以原样重跑。
fn reject_delete_through_symlink(
    agent_dir: &Path,
    confirmed: &[String],
    rebuilt: &[&str],
) -> Result<(), ApplyFailure> {
    for relative in confirmed {
        let ancestor = symlink_ancestors(agent_dir, relative)
            .into_iter()
            .find(|ancestor| !rebuilt.contains(&ancestor.as_str()));
        if let Some(ancestor) = ancestor {
            return Err(ApplyFailure::new(
                "delete_through_symlink",
                format!(
                    "{relative} 的路径经过符号链接 {ancestor}，拒绝删除；请先手动处理该目录"
                ),
            ));
        }
    }
    Ok(())
}

/// 一条待重建的链接：宿主目录内的相对路径与 manifest 记下的原目标。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RebuildLink {
    relative: String,
    target: String,
}

/// manifest 里的 `symlinks`。导出端把链接物化成内容，同时把原目标记在这里。
#[derive(Debug, Deserialize)]
struct ManifestLinks {
    #[serde(default)]
    symlinks: Vec<ManifestSymlink>,
}

/// 读 manifest 的链接记录。校验已在计划阶段做过，这里只取落地需要的部分。
fn manifest_links(sync_root: &Path) -> Result<Vec<ManifestSymlink>, ApplyFailure> {
    let path = sync_root.join(MANIFEST_FILE_NAME);
    let text = fs::read_to_string(&path).map_err(|error| {
        ApplyFailure::new(
            "manifest_invalid",
            format!("读取 manifest 里的链接记录失败：{error}"),
        )
    })?;
    let manifest: ManifestLinks = serde_json::from_str(&text).map_err(|error| {
        ApplyFailure::new(
            "manifest_invalid",
            format!("manifest 里的链接记录无法解析：{error}"),
        )
    })?;
    Ok(manifest.symlinks)
}

/// 算出要重建的链接：路径在受管范围内且目标存在。
/// 目标不存在（含断链）就不重建，退回物化写入并记 warning。
fn rebuildable_links(
    agent_dir: &Path,
    links: &[ManifestSymlink],
    warnings: &mut Vec<String>,
) -> Vec<RebuildLink> {
    if links.is_empty() {
        return Vec::new();
    }
    if !SYMLINK_SUPPORTED {
        warnings.push(format!(
            "当前平台不支持符号链接：{} 个链接条目退回物化写入",
            links.len()
        ));
        return Vec::new();
    }

    let mut rebuilt: Vec<RebuildLink> = Vec::new();
    for link in links {
        let relative = link_relative(&link.path);
        let relative_path = Path::new(relative);
        // 被改过的 manifest 可能指向受管范围之外：不重建，免得删掉 auth.json 这类文件。
        if !is_managed_link(relative_path)
            || scope::resolve_within(agent_dir, relative_path).is_err()
        {
            warnings.push(format!(
                "{relative} 不在受管范围内，未重建链接"
            ));
            continue;
        }
        let Some(parent) = relative_path.parent() else {
            warnings.push(format!("{relative} 没有父目录，未重建链接"));
            continue;
        };
        // 链接目标相对链接所在目录解释；绝对目标直接决定位置。
        if !agent_dir.join(parent).join(&link.target).exists() {
            warnings.push(format!(
                "{relative} 的链接目标 {} 不存在，已退回写入快照内容",
                link.target
            ));
            continue;
        }
        if rebuilt.iter().any(|item| item.relative == relative) {
            continue;
        }
        rebuilt.push(RebuildLink {
            relative: relative.to_string(),
            target: link.target.clone(),
        });
    }
    // 外层链接先建，嵌套链接才能落到链接目标里。
    rebuilt.sort_by(|left, right| left.relative.cmp(&right.relative));
    rebuilt
}

/// 重建一条链接：父目录就位后清掉旧路径，再按原目标建立链接。
/// 旧路径是真目录时整棵删除：里面的内容按约定由链接目标提供，不保留本地副本。
fn rebuild_link(agent_dir: &Path, link: &RebuildLink) -> io::Result<()> {
    let path = scope::resolve_within(agent_dir, Path::new(&link.relative)).map_err(to_io_error)?;
    if let Some(parent) = path.parent() {
        // 父目录可以本身就是链接：跟随它，不换成真实目录。
        fs::create_dir_all(parent)?;
    }
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&path)?,
        Ok(_) => fs::remove_file(&path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    create_link(&link.target, &path)
}

#[cfg(unix)]
fn create_link(target: &str, path: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

/// 非 Unix 由 [`SYMLINK_SUPPORTED`] 提前挡下，走不到这里；保留实现只是为了让函数签名在两端一致。
#[cfg(not(unix))]
fn create_link(_target: &str, _path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "当前平台不支持符号链接",
    ))
}

/// manifest 里的路径形如 `snapshot/skills/antd`；落地时用相对宿主目录的路径。
fn link_relative(path: &str) -> &str {
    path.strip_prefix(SNAPSHOT_DIR_NAME)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(path)
}

/// `path` 是否就是 `ancestor` 或在它之下；按路径分段判，不把 `skills-extra` 当成 `skills` 的子项。
fn is_under(path: &str, ancestor: &str) -> bool {
    path == ancestor
        || path
            .strip_prefix(ancestor)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// 链接路径必须在受管范围内：白名单与排除规则同计划，但链接本身可以是目录，
/// 所以根级目录名也算在内。目的是不让被改过的 manifest 借链接删掉 `auth.json` 这类文件。
fn is_managed_link(relative: &Path) -> bool {
    let names: Vec<&str> = relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<&str>>>()
        .unwrap_or_default();

    if names.is_empty() || names.iter().any(|name| scope::is_excluded_name(name)) {
        return false;
    }
    if names.len() > 1 {
        return scope::INCLUDED_ROOT_DIRS.contains(&names[0]);
    }
    scope::INCLUDED_ROOT_FILES.contains(&names[0])
        || scope::INCLUDED_ROOT_DIRS.contains(&names[0])
        || Path::new(names[0])
            .extension()
            .is_some_and(|extension| extension == "json")
}

/// 建备份目录：`<app_data>/backups/<UTC 时间戳>/files/`，权限 0700。
fn create_backup_dir(app_data_dir: &Path) -> Result<Backup, ApplyFailure> {
    let root = app_data_dir.join(BACKUPS_DIR_NAME);
    let created_at = utc_timestamp(now_seconds());
    let base = created_at.replace(':', "-");

    // 同一秒内应用两次时加序号，避免两次备份混在一个目录里。
    let mut id = base.clone();
    let mut suffix = 1;
    while root.join(&id).exists() {
        id = format!("{base}-{suffix}");
        suffix += 1;
    }
    let dir = root.join(&id);

    DirBuilder::new()
        .recursive(true)
        .mode(BACKUP_DIR_MODE)
        .create(&dir)
        .and_then(|()| {
            DirBuilder::new()
                .recursive(true)
                .mode(BACKUP_DIR_MODE)
                .create(dir.join(BACKUP_FILES_DIR_NAME))
        })
        .map_err(|error| {
            ApplyFailure::new("backup_failed", format!("无法创建备份目录：{error}"))
        })?;

    Ok(Backup {
        id,
        dir,
        created_at,
    })
}

/// 备份将被覆盖或删除的现有文件，写下 `restore.json`。
fn write_backup(
    agent_dir: &Path,
    plan: &plan::Plan,
    fingerprint: &str,
    confirmed: &[String],
    backup: &Backup,
) -> Result<(), ApplyFailure> {
    let mut touched: Vec<&str> = plan
        .added
        .iter()
        .chain(&plan.modified)
        .map(|item| item.path.as_str())
        .collect();
    touched.extend(confirmed.iter().map(String::as_str));

    let files_dir = backup.dir.join(BACKUP_FILES_DIR_NAME);
    // 写入时 `ensure_dir` 会把祖先链接换成真实目录，链接本身只能在写入前记下来；
    // 这些条目放在文件条目之前，T8 先恢复链接再往里放文件。
    let mut entries = symlink_ancestors_of(agent_dir, &touched)
        .iter()
        .map(|relative| record(agent_dir, relative, &files_dir))
        .collect::<Result<Vec<RestoreEntry>, ApplyFailure>>()?;
    entries.extend(
        touched
            .iter()
            .map(|relative| record(agent_dir, relative, &files_dir))
            .collect::<Result<Vec<RestoreEntry>, ApplyFailure>>()?,
    );

    let manifest = RestoreManifest {
        version: RESTORE_VERSION,
        created_at: backup.created_at.clone(),
        fingerprint: fingerprint.to_string(),
        entries,
    };
    let mut text = serde_json::to_string_pretty(&manifest).map_err(|error| {
        ApplyFailure::new("backup_failed", format!("restore.json 序列化失败：{error}"))
    })?;
    text.push('\n');
    fs::write(backup.dir.join(RESTORE_FILE_NAME), text).map_err(|error| {
        ApplyFailure::new("backup_failed", format!("写入 restore.json 失败：{error}"))
    })
}

/// 若干相对路径的符号链接祖先，去重后按路径排序。
fn symlink_ancestors_of(agent_dir: &Path, relatives: &[&str]) -> Vec<String> {
    let mut ancestors: BTreeSet<String> = BTreeSet::new();
    for relative in relatives {
        ancestors.extend(symlink_ancestors(agent_dir, relative));
    }
    ancestors.into_iter().collect()
}

/// `relative` 的祖先目录里实际是符号链接的那些（不含自身），由外到内。
fn symlink_ancestors(agent_dir: &Path, relative: &str) -> Vec<String> {
    let mut current = agent_dir.to_path_buf();
    let mut ancestor = String::new();
    let mut found = Vec::new();
    let mut components = Path::new(relative).components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        current.push(component);
        if !ancestor.is_empty() {
            ancestor.push('/');
        }
        ancestor.push_str(&component.as_os_str().to_string_lossy());
        if fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            found.push(ancestor.clone());
        }
    }
    found
}

/// 备份单个路径：文件复制内容，符号链接只记目标，新增文件只记 `existed: false`。
fn record(agent_dir: &Path, relative: &str, files_dir: &Path) -> Result<RestoreEntry, ApplyFailure> {
    let live = scope::resolve_within(agent_dir, Path::new(relative)).map_err(|error| {
        ApplyFailure::new("backup_failed", format!("无法定位 {relative}：{error}"))
    })?;

    let metadata = match fs::symlink_metadata(&live) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RestoreEntry {
                path: relative.to_string(),
                kind: RestoreKind::File,
                existed: false,
                mode: None,
                link_target: None,
            })
        }
        Err(error) => {
            return Err(ApplyFailure::new(
                "backup_failed",
                format!("读取 {relative} 的信息失败：{error}"),
            ))
        }
    };

    // 计划项都是文件；目标位置此刻是目录说明计划期间磁盘变了，停下让人看，不覆盖。
    if metadata.is_dir() {
        return Err(ApplyFailure::new(
            "backup_failed",
            format!("{relative} 现在是目录，无法作为文件应用；请重新检查后再应用"),
        ));
    }

    let file_type = metadata.file_type();
    let mode = (!file_type.is_symlink()).then(|| metadata.permissions().mode() & 0o777);
    let link_target = if file_type.is_symlink() {
        let target = fs::read_link(&live).map_err(|error| {
            ApplyFailure::new("backup_failed", format!("读取 {relative} 的链接目标失败：{error}"))
        })?;
        Some(target.to_string_lossy().into_owned())
    } else {
        None
    };

    if link_target.is_none() {
        let destination = files_dir.join(relative);
        if let Some(parent) = destination.parent() {
            DirBuilder::new()
                .recursive(true)
                .mode(BACKUP_DIR_MODE)
                .create(parent)
                .map_err(|error| {
                    ApplyFailure::new("backup_failed", format!("创建备份子目录失败：{error}"))
                })?;
        }
        fs::copy(&live, &destination).map_err(|error| {
            ApplyFailure::new("backup_failed", format!("备份 {relative} 失败：{error}"))
        })?;
    }

    Ok(RestoreEntry {
        path: relative.to_string(),
        kind: if link_target.is_some() {
            RestoreKind::Symlink
        } else {
            RestoreKind::File
        },
        existed: true,
        mode,
        link_target,
    })
}

/// 只保留最近 [`BACKUP_RETENTION`] 份；目录名是 UTC 时间戳，按名字排序即按时间排序。
fn prune(backups: &Path) {
    let mut ids: Vec<String> = match fs::read_dir(backups) {
        Ok(read_dir) => read_dir
            .filter_map(|item| item.ok())
            .filter(|item| item.path().is_dir())
            .map(|item| item.file_name().to_string_lossy().into_owned())
            .collect(),
        // 清理失败不影响本次应用：旧备份多留一份，比中断落地安全。
        Err(_) => return,
    };
    ids.sort();
    let excess = ids.len().saturating_sub(BACKUP_RETENTION);
    for id in ids.into_iter().take(excess) {
        let _ = fs::remove_dir_all(backups.join(id));
    }
}

/// 写入一个快照文件：先写同目录临时文件再 rename，权限沿用原 mode。
fn write_snapshot_file(agent_dir: &Path, sync_root: &Path, relative: &str) -> io::Result<()> {
    let source = scope::resolve_within(&sync_root.join(SNAPSHOT_DIR_NAME), Path::new(relative))
        .map_err(to_io_error)?;
    let bytes = fs::read(&source)?;
    let target = scope::resolve_within(agent_dir, Path::new(relative)).map_err(to_io_error)?;

    // 覆盖已有普通文件时沿用原 mode；新增文件用 0644。
    let mode = match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.is_file() => metadata.permissions().mode() & 0o777,
        _ => NEW_FILE_MODE,
    };
    if let Some(parent) = target.parent() {
        ensure_dir(parent)?;
    }

    // 目标是符号链接（或目录）时先删掉再写真实文件，绝不写穿链接。
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&target)?,
        Ok(metadata) if metadata.file_type().is_symlink() => fs::remove_file(&target)?,
        _ => {}
    }

    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "目标路径没有文件名"))?
        .to_string_lossy()
        .into_owned();
    let temp = target.with_file_name(format!(".{name}.tmp-{}", now_nanos()));

    let written = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        // rename 覆盖已有文件在 Unix 上原子；Windows 会失败，届时改为先 unlink。
        fs::set_permissions(&temp, fs::Permissions::from_mode(mode))?;
        fs::rename(&temp, &target)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

/// 逐级建目录；遇到符号链接先 unlink 再建真目录，绝不写穿链接。
fn ensure_dir(dir: &Path) -> io::Result<()> {
    let mut current = PathBuf::new();
    for component in dir.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                fs::remove_file(&current)?;
                fs::create_dir(&current)?;
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} 已存在且不是目录", current.display()),
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// 删除一个已确认的条目：只删链接本身，不跟随链接；目录才整棵删除。
fn delete_target(agent_dir: &Path, relative: &str) -> io::Result<()> {
    // 上级组件是符号链接时，删除会落到链接指向的外部目录，一律不动。
    // 正常流程已在 [`reject_delete_through_symlink`] 挡住，这里挡住校验之后才出现的链接。
    if let Some(ancestor) = symlink_ancestors(agent_dir, relative).into_iter().next() {
        return Err(io::Error::other(format!(
            "{relative} 的路径经过符号链接 {ancestor}，删除会落到链接指向的外部位置，已拒绝"
        )));
    }
    let target = scope::resolve_within(agent_dir, Path::new(relative)).map_err(to_io_error)?;

    match fs::symlink_metadata(&target)? {
        metadata if metadata.is_dir() => fs::remove_dir_all(&target),
        _ => fs::remove_file(&target),
    }
}

fn to_io_error(error: scope::ScopeError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
}

/// UTC 时间戳，形如 `2026-11-20T05:30:00Z`（目录名里把冒号换成连字符）。
fn utc_timestamp(unix_seconds: u64) -> String {
    let days = (unix_seconds / 86_400) as i64;
    let seconds_of_day = unix_seconds % 86_400;

    // 公历换算（Howard Hinnant 的 civil_from_days），不引入时间库。
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_position + 2) / 5 + 1;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds_of_day / 3_600,
        seconds_of_day % 3_600 / 60,
        seconds_of_day % 60
    )
}

/// command 层入口：同步根目录与宿主目录取自应用配置，备份写进应用数据目录。
#[tauri::command]
pub async fn apply_snapshot(
    app: AppHandle,
    expected_fingerprint: String,
    delete_paths: Vec<String>,
) -> Result<ApplyReport, ApplyFailure> {
    tauri::async_runtime::spawn_blocking(move || {
        run(&app, &expected_fingerprint, &delete_paths)
    })
    .await
    .map_err(|error| ApplyFailure::new("apply_failed", format!("应用失败：{error}")))?
}

fn run(
    app: &AppHandle,
    expected_fingerprint: &str,
    delete_paths: &[String],
) -> Result<ApplyReport, ApplyFailure> {
    let config_dir = app.path().app_config_dir().map_err(|error| {
        ApplyFailure::new(
            "app_config_unavailable",
            format!("无法定位应用配置目录：{error}"),
        )
    })?;
    let app_data_dir = app.path().app_data_dir().map_err(|error| {
        ApplyFailure::new(
            "app_data_unavailable",
            format!("无法定位应用数据目录：{error}"),
        )
    })?;
    let config = config::load_config(&config_dir)
        .map_err(|error| ApplyFailure::new("app_config_unavailable", error.to_string()))?;

    let sync_root = config.sync_root.clone().ok_or_else(|| {
        ApplyFailure::new(
            "sync_root_missing",
            "还没有设置同步文件夹，请先在设置页选择",
        )
    })?;

    // 只读取这一个文档化的环境变量；错误信息里不回显其内容。
    let env_agent_dir = std::env::var_os(config::AGENT_DIR_ENV_VAR).map(PathBuf::from);
    let home_dir = app.path().home_dir().ok();
    let resolved = config::resolve_agent_dir(&config, env_agent_dir.as_deref(), home_dir.as_deref());
    let agent_dir = resolved
        .path
        .ok_or_else(|| ApplyFailure::new("agent_dir_missing", "无法确定宿主目录，请先在设置页填写"))?;

    apply(
        &agent_dir,
        &sync_root,
        expected_fingerprint,
        delete_paths,
        &app_data_dir,
        &ShellRunner::from_env(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::CommandOutcome;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// 临时目录夹具：Drop 时清理。测试全程不碰真实 HOME。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-apply-{}-{}-{}",
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

    /// 固定版本的假命令：计划里的版本探测不起真实进程。
    struct FakeRunner;

    impl FakeRunner {
        fn ok() -> Self {
            Self
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            _script: &str,
            _timeout: Duration,
            _env: &[(&str, String)],
        ) -> CommandOutcome {
            CommandOutcome::Ok {
                stdout: "node:22.23.0\npi:1.5.2\n".to_string(),
                stderr: String::new(),
            }
        }
    }

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("创建父目录");
        }
        fs::write(path, content).expect("写入夹具文件");
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path)
            .expect("读取文件信息")
            .permissions()
            .mode()
            & 0o777
    }

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("设置夹具权限");
    }

    /// 宿主目录 + 已导出的快照：先保证"无差异"是基线。
    fn fixture(temp: &TempDir) -> (PathBuf, PathBuf) {
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        write_file(&agent.join("settings.json"), "{\"theme\": \"dark\"}");
        write_file(&agent.join("AGENTS.md"), "# 指令\n");
        write_file(&agent.join("skills/antd/SKILL.md"), "# 技能\n");
        // 范围外：既不进快照，也不参与计划，永不被碰。
        write_file(&agent.join("auth.json"), "{\"token\": \"sk-test\"}");
        write_file(&agent.join("sessions/old.json"), "{}");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        crate::snapshot::export(&agent, &sync_root, false).expect("导出夹具快照");
        (agent, sync_root)
    }

    fn app_data(temp: &TempDir) -> PathBuf {
        let dir = temp.path().join("app-data");
        fs::create_dir_all(&dir).expect("创建应用数据目录");
        dir
    }

    fn fingerprint(agent: &Path, sync_root: &Path) -> String {
        plan::plan_snapshot(agent, sync_root, &FakeRunner::ok())
            .expect("生成夹具计划")
            .manifest_info
            .fingerprint
    }

    /// 整棵子树的路径与内容，用于证明"没有写任何文件"。
    fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut entries = Vec::new();
        for item in fs::read_dir(root).expect("读取目录") {
            let path = item.expect("目录项").path();
            let name = path.file_name().expect("文件名").to_string_lossy().into_owned();
            if path.is_dir() {
                for (relative, bytes) in tree(&path) {
                    entries.push((format!("{name}/{relative}"), bytes));
                }
            } else {
                entries.push((name, fs::read(&path).unwrap_or_default()));
            }
        }
        entries.sort();
        entries
    }

    fn backup_ids(app_data: &Path) -> Vec<String> {
        let backups = app_data.join(BACKUPS_DIR_NAME);
        let Ok(read_dir) = fs::read_dir(&backups) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = read_dir
            .filter_map(|item| item.ok())
            .filter(|item| item.path().is_dir())
            .map(|item| item.file_name().to_string_lossy().into_owned())
            .collect();
        ids.sort();
        ids
    }

    fn read_restore(backup_dir: &Path) -> RestoreManifest {
        let text = fs::read_to_string(backup_dir.join(RESTORE_FILE_NAME)).expect("读 restore.json");
        serde_json::from_str(&text).expect("restore.json 应是合法 JSON")
    }

    fn entry<'a>(restore: &'a RestoreManifest, path: &str) -> &'a RestoreEntry {
        restore
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("restore.json 缺少 {path}：{:?}", restore.entries))
    }

    #[test]
    fn applies_added_modified_and_only_confirmed_deletions() {
        let temp = TempDir::new("apply");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);

        write_file(&agent.join("settings.json"), "{\"theme\": \"light\"}");
        fs::remove_file(agent.join("skills/antd/SKILL.md")).expect("删除 live 文件");
        // 两个删除候选：只勾选其中一个。
        write_file(&agent.join("prompts/leftover.md"), "留下");
        write_file(&agent.join("themes/old.json"), "{}");

        let expected = fingerprint(&agent, &sync_root);
        let report = apply(
            &agent,
            &sync_root,
            &expected,
            &["themes/old.json".to_string()],
            &data,
            &FakeRunner::ok(),
        )
        .expect("应用应成功");

        assert_eq!(report.written, 2, "新增 1 + 修改 1");
        assert_eq!(report.deleted, 1);
        assert_eq!(report.skipped, 1, "未勾选的删除候选保留");
        assert_eq!(report.fingerprint, expected);
        assert_eq!(report.failed_at, None);
        assert_eq!(
            report.backup_id,
            report
                .backup_dir
                .file_name()
                .expect("备份目录名")
                .to_string_lossy()
        );
        assert!(report.backup_dir.is_dir());

        assert_eq!(
            fs::read_to_string(agent.join("settings.json")).expect("读 settings"),
            "{\"theme\": \"dark\"}"
        );
        assert_eq!(
            fs::read_to_string(agent.join("skills/antd/SKILL.md")).expect("读技能"),
            "# 技能\n"
        );
        assert!(agent.join("prompts/leftover.md").is_file(), "未勾选的删除项保留");
        assert!(!agent.join("themes/old.json").exists(), "勾选的删除项被删");
        // 范围外文件从未被碰。
        assert_eq!(
            fs::read_to_string(agent.join("auth.json")).expect("读 auth.json"),
            "{\"token\": \"sk-test\"}"
        );
        assert!(agent.join("sessions/old.json").is_file());

        let restore = read_restore(&report.backup_dir);
        assert_eq!(restore.fingerprint, expected);
        // 2 个写入 + 1 个删除；未勾选的删除候选不备份。
        assert_eq!(restore.entries.len(), 3);
        assert!(entry(&restore, "themes/old.json").existed);
    }

    #[test]
    fn stale_fingerprint_is_rejected_before_writing() {
        let temp = TempDir::new("stale");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);

        write_file(&agent.join("settings.json"), "{\"theme\": \"light\"}");
        let before = tree(&agent);

        let failure = apply(
            &agent,
            &sync_root,
            "000000000000",
            &[],
            &data,
            &FakeRunner::ok(),
        )
        .expect_err("指纹不符必须拒绝");

        assert_eq!(failure.code, "plan_stale");
        assert_eq!(failure.completed, 0);
        assert_eq!(failure.backup_dir, None);
        assert_eq!(tree(&agent), before, "拒绝时不得写任何文件");
        assert!(backup_ids(&data).is_empty(), "拒绝时不得建备份");
    }

    #[test]
    fn missing_manifest_and_hash_mismatch_are_manifest_invalid() {
        let temp = TempDir::new("invalid");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);
        let expected = fingerprint(&agent, &sync_root);

        fs::remove_file(sync_root.join(crate::snapshot::MANIFEST_FILE_NAME)).expect("删 manifest");
        let failure = apply(&agent, &sync_root, &expected, &[], &data, &FakeRunner::ok())
            .expect_err("manifest 缺失必须拒绝");
        assert_eq!(failure.code, "manifest_invalid", "{}", failure.message);

        // 云盘只传了一半：manifest 还在，但文件内容不符。
        crate::snapshot::export(&agent, &sync_root, false).expect("重新导出快照");
        write_file(&sync_root.join("snapshot/settings.json"), "{\"theme\": \"half\"}");
        let failure = apply(&agent, &sync_root, &expected, &[], &data, &FakeRunner::ok())
            .expect_err("哈希不符必须拒绝");
        assert_eq!(failure.code, "manifest_invalid", "{}", failure.message);
        assert!(failure.message.contains("hash_mismatch"), "{}", failure.message);
        assert!(backup_ids(&data).is_empty());
    }

    #[test]
    fn delete_paths_outside_the_plan_are_rejected() {
        let temp = TempDir::new("deletes");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);
        let expected = fingerprint(&agent, &sync_root);
        let before = tree(&agent);

        for path in ["../escape.json", "auth.json", "settings.json", "/etc/passwd"] {
            let failure = apply(
                &agent,
                &sync_root,
                &expected,
                &[path.to_string()],
                &data,
                &FakeRunner::ok(),
            )
            .expect_err("不在计划里的删除项必须整次拒绝");
            assert_eq!(failure.code, "delete_not_in_plan", "{path}");
        }

        assert_eq!(tree(&agent), before, "拒绝时不得写任何文件");
        assert!(backup_ids(&data).is_empty());
    }

    #[test]
    fn backup_records_modes_and_symlink_targets_without_copying_content() {
        let temp = TempDir::new("backup");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        let data = app_data(&temp);

        write_file(&agent.join("settings.json"), "{\"theme\": \"dark\"}");
        set_mode(&agent.join("settings.json"), 0o640);
        write_file(&temp.path().join("outside/real.md"), "原始内容\n");
        fs::create_dir_all(agent.join("skills/antd")).expect("创建技能目录");
        // 链接指向宿主目录之外：应用时不得写穿。
        symlink("../../../outside/real.md", agent.join("skills/antd/SKILL.md"))
            .expect("创建符号链接");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        crate::snapshot::export(&agent, &sync_root, false).expect("导出快照");

        // 链接目标内容变了：计划里 SKILL.md 变成"修改"；settings.json 也改一下，
        // 让备份同时覆盖"修改文件 + 修改链接"两种情况。
        write_file(&temp.path().join("outside/real.md"), "外部被改\n");
        write_file(&agent.join("settings.json"), "{\"theme\": \"light\"}");
        set_mode(&agent.join("settings.json"), 0o640);
        let expected = fingerprint(&agent, &sync_root);

        let report = apply(&agent, &sync_root, &expected, &[], &data, &FakeRunner::ok())
            .expect("应用应成功");

        // 链接按 manifest 记录重建，外部目标保持原样。
        let metadata = fs::symlink_metadata(agent.join("skills/antd/SKILL.md")).expect("读信息");
        assert!(metadata.file_type().is_symlink(), "链接按 manifest 记录重建");
        assert_eq!(
            fs::read_to_string(agent.join("skills/antd/SKILL.md")).expect("读技能"),
            "外部被改\n",
            "内容由链接目标提供"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("outside/real.md")).expect("读外部文件"),
            "外部被改\n",
            "不得写穿链接"
        );
        assert_eq!(report.linked, 1);
        assert_eq!(report.written, 1, "只有 settings.json 需要写");
        assert_eq!(
            fs::read_to_string(agent.join("settings.json")).expect("读 settings"),
            "{\"theme\": \"dark\"}"
        );
        assert_eq!(mode_of(&agent.join("settings.json")), 0o640, "覆盖后保留原 mode");

        // 备份目录 0700；restore.json 契约。
        assert_eq!(mode_of(&report.backup_dir), 0o700);
        let restore = read_restore(&report.backup_dir);
        assert_eq!(restore.version, RESTORE_VERSION);
        assert_eq!(restore.fingerprint, expected);
        assert!(restore.created_at.ends_with('Z'), "{}", restore.created_at);
        assert_eq!(restore.entries.len(), 2);

        let settings = entry(&restore, "settings.json");
        assert_eq!(settings.kind, RestoreKind::File);
        assert!(settings.existed);
        assert_eq!(settings.mode, Some(0o640));
        assert_eq!(settings.link_target, None);
        assert_eq!(
            fs::read_to_string(report.backup_dir.join("files/settings.json")).expect("读备份内容"),
            "{\"theme\": \"light\"}",
            "备份的是覆盖前的 live 内容"
        );

        let link = entry(&restore, "skills/antd/SKILL.md");
        assert_eq!(link.kind, RestoreKind::Symlink);
        assert!(link.existed);
        assert_eq!(link.mode, None);
        assert_eq!(link.link_target.as_deref(), Some("../../../outside/real.md"));
        assert!(
            !report.backup_dir.join("files/skills/antd/SKILL.md").exists(),
            "符号链接只记目标，不复制内容"
        );
    }

    #[test]
    fn added_file_is_backed_up_as_not_existed() {
        let temp = TempDir::new("added");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);

        fs::remove_file(agent.join("AGENTS.md")).expect("删除 live 文件");
        let expected = fingerprint(&agent, &sync_root);

        let report = apply(&agent, &sync_root, &expected, &[], &data, &FakeRunner::ok())
            .expect("应用应成功");
        assert_eq!(report.written, 1);

        let restore = read_restore(&report.backup_dir);
        let added = entry(&restore, "AGENTS.md");
        assert_eq!(added.kind, RestoreKind::File);
        assert!(!added.existed, "新增文件没有原内容可备份");
        assert_eq!(added.mode, None);
        assert!(!report.backup_dir.join("files/AGENTS.md").exists());
        assert_eq!(mode_of(&agent.join("AGENTS.md")), NEW_FILE_MODE);
    }

    #[test]
    fn write_failure_stops_and_backup_restores_completed_entries() {
        let temp = TempDir::new("fail");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);

        // 两个修改项：按路径排序先写 settings.json，再写 skills/antd/SKILL.md。
        write_file(&agent.join("settings.json"), "{\"theme\": \"light\"}");
        write_file(&agent.join("skills/antd/SKILL.md"), "# 新技能\n");
        let expected = fingerprint(&agent, &sync_root);

        // 技能目录只读：临时文件创建失败。
        set_mode(&agent.join("skills/antd"), 0o500);
        let failure = apply(&agent, &sync_root, &expected, &[], &data, &FakeRunner::ok())
            .expect_err("写入失败必须停下");
        set_mode(&agent.join("skills/antd"), 0o755);

        assert_eq!(failure.code, "apply_failed", "{}", failure.message);
        assert_eq!(failure.completed, 1, "只有 settings.json 落地");
        let backup_dir = failure.backup_dir.clone().expect("失败要附上备份目录");
        assert_eq!(
            fs::read_to_string(agent.join("settings.json")).expect("读 settings"),
            "{\"theme\": \"dark\"}",
            "已完成的条目留在原地"
        );
        assert_eq!(
            fs::read_to_string(agent.join("skills/antd/SKILL.md")).expect("读技能"),
            "# 新技能\n",
            "失败后的条目不再写"
        );

        // 已完成条目可据备份还原。
        let restore = read_restore(&backup_dir);
        assert_eq!(restore.fingerprint, expected);
        let settings = entry(&restore, "settings.json");
        assert!(settings.existed);
        let backed_up =
            fs::read_to_string(backup_dir.join("files/settings.json")).expect("读备份内容");
        assert_eq!(backed_up, "{\"theme\": \"light\"}");
        assert_eq!(mode_of(&backup_dir.join("files/settings.json")), settings.mode.unwrap());
    }

    #[test]
    fn backups_beyond_retention_are_pruned_oldest_first() {
        let temp = TempDir::new("retention");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);

        let backups = data.join(BACKUPS_DIR_NAME);
        let mut old = Vec::new();
        for day in 1..=11 {
            let id = format!("2000-01-{day:02}T00-00-00Z");
            fs::create_dir_all(backups.join(&id)).expect("创建旧备份");
            old.push(id);
        }
        assert_eq!(backup_ids(&data).len(), 11);

        let expected = fingerprint(&agent, &sync_root);
        let report = apply(&agent, &sync_root, &expected, &[], &data, &FakeRunner::ok())
            .expect("应用应成功");

        let ids = backup_ids(&data);
        assert_eq!(ids.len(), BACKUP_RETENTION);
        assert!(!ids.contains(&old[0]), "最旧的两份被清理");
        assert!(!ids.contains(&old[1]));
        assert!(ids.contains(&report.backup_id), "新备份保留");
    }

    /// 宿主目录里的 `skills` 是指向外部目录的链接；快照由该链接导出。
    fn symlinked_skills_fixture(temp: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        let external = temp.path().join("outside/skills-live");

        write_file(&external.join("keep.md"), "v1\n");
        fs::create_dir_all(&agent).expect("创建宿主目录");
        symlink("../outside/skills-live", agent.join("skills")).expect("创建目录链接");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        crate::snapshot::export(&agent, &sync_root, false).expect("导出夹具快照");
        (agent, sync_root, external)
    }

    #[test]
    fn rebuilds_manifest_links_and_skips_writes_under_them() {
        let temp = TempDir::new("rebuild-link");
        let (agent, sync_root, external) = symlinked_skills_fixture(&temp);
        let data = app_data(&temp);

        // 目标内容变了：计划里 skills/keep.md 是“修改”，物化就会在宿主目录里写出真实文件。
        write_file(&external.join("keep.md"), "v2\n");
        let expected = fingerprint(&agent, &sync_root);
        let external_before = tree(&external);

        let report = apply(&agent, &sync_root, &expected, &[], &data, &FakeRunner::ok())
            .expect("应用应成功");

        assert_eq!(report.linked, 1);
        assert_eq!(report.written, 0, "链接之下的写入交给链接目标");
        assert_eq!(report.skipped_deletes, 0);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        assert!(
            fs::symlink_metadata(agent.join("skills"))
                .expect("读 skills 信息")
                .file_type()
                .is_symlink(),
            "skills 仍是符号链接，没有被物化"
        );
        assert_eq!(
            fs::read_link(agent.join("skills")).expect("读链接目标"),
            PathBuf::from("../outside/skills-live")
        );
        assert_eq!(
            fs::read_to_string(agent.join("skills/keep.md")).expect("经链接读到目标内容"),
            "v2\n"
        );
        assert_eq!(tree(&external), external_before, "目标目录内容逐字节不变");

        // 重建前先记下原有链接：T8 可按 restore.json 还原。
        let restore = read_restore(&report.backup_dir);
        let skills = entry(&restore, "skills");
        assert_eq!(skills.kind, RestoreKind::Symlink);
        assert!(skills.existed);
        assert_eq!(skills.link_target.as_deref(), Some("../outside/skills-live"));
    }

    #[test]
    fn broken_link_target_falls_back_to_materializing() {
        let temp = TempDir::new("broken-link");
        let (agent, sync_root, external) = symlinked_skills_fixture(&temp);
        let data = app_data(&temp);

        // 目标整个消失：链接变成断链，只能把快照内容物化成真实文件。
        fs::remove_dir_all(&external).expect("删除链接目标");
        let expected = fingerprint(&agent, &sync_root);

        let report = apply(&agent, &sync_root, &expected, &[], &data, &FakeRunner::ok())
            .expect("应用应成功");

        assert_eq!(report.linked, 0, "目标不存在就不重建");
        assert_eq!(report.written, 1);
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(report.warnings[0].contains("skills"), "{:?}", report.warnings);

        let metadata = fs::symlink_metadata(agent.join("skills")).expect("读 skills 信息");
        assert!(!metadata.file_type().is_symlink(), "断链退回物化");
        assert!(metadata.is_dir());
        assert_eq!(
            fs::read_to_string(agent.join("skills/keep.md")).expect("读落地文件"),
            "v1\n"
        );
        assert!(!external.exists(), "不替用户重建目标目录");

        // 断链本身也在备份里：条目排在文件之前，T8 先恢复链接。
        let restore = read_restore(&report.backup_dir);
        let skills = entry(&restore, "skills");
        assert_eq!(skills.kind, RestoreKind::Symlink);
        assert!(skills.existed);
        assert_eq!(skills.mode, None);
        assert_eq!(skills.link_target.as_deref(), Some("../outside/skills-live"));
        assert_eq!(
            restore.entries.first().map(|entry| entry.path.as_str()),
            Some("skills"),
            "链接条目排在文件条目之前"
        );
    }

    #[test]
    fn tampered_manifest_link_outside_scope_is_not_rebuilt() {
        let temp = TempDir::new("link-out-of-scope");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);

        // 被改过的 manifest 想借链接接管 auth.json：范围外一律不重建。
        write_file(&temp.path().join("outside/real.md"), "外部\n");
        let manifest_path = sync_root.join(crate::snapshot::MANIFEST_FILE_NAME);
        let mut manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&manifest_path).expect("读 manifest"))
                .expect("解析 manifest");
        manifest["symlinks"] = serde_json::json!([
            { "path": "snapshot/auth.json", "target": "../outside/real.md" }
        ]);
        fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).expect("序列化 manifest"),
        )
        .expect("写 manifest");

        let expected = fingerprint(&agent, &sync_root);
        let report = apply(&agent, &sync_root, &expected, &[], &data, &FakeRunner::ok())
            .expect("应用应成功");

        assert_eq!(report.linked, 0);
        assert_eq!(report.written, 0);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("auth.json")),
            "{:?}",
            report.warnings
        );
        assert_eq!(
            fs::read_to_string(agent.join("auth.json")).expect("读 auth.json"),
            "{\"token\": \"sk-test\"}",
            "范围外文件不得被写入、删除或接管"
        );
    }

    #[test]
    fn deletes_under_rebuilt_link_are_skipped() {
        let temp = TempDir::new("skip-delete-under-link");
        let (agent, sync_root, external) = symlinked_skills_fixture(&temp);
        let data = app_data(&temp);

        // 链接目标里多出快照没有的文件：它是删除候选，物理位置在链接目标里。
        write_file(&external.join("extra.md"), "不要删\n");
        let expected = fingerprint(&agent, &sync_root);
        let external_before = tree(&external);

        let report = apply(
            &agent,
            &sync_root,
            &expected,
            &["skills/extra.md".to_string()],
            &data,
            &FakeRunner::ok(),
        )
        .expect("链接之下的删除跳过，应用仍成功");

        assert_eq!(report.linked, 1);
        assert_eq!(report.deleted, 0);
        assert_eq!(report.skipped_deletes, 1);
        assert_eq!(report.skipped, 0, "勾选的删除项不算“未勾选保留”");
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("skills/extra.md")),
            "{:?}",
            report.warnings
        );
        assert!(
            external.join("extra.md").is_file(),
            "链接目标里的文件不得被删"
        );
        assert_eq!(tree(&external), external_before);
        assert!(
            fs::symlink_metadata(agent.join("skills"))
                .expect("读 skills 信息")
                .file_type()
                .is_symlink(),
            "skills 仍是符号链接"
        );
    }

    #[test]
    fn deleting_under_symlink_ancestor_not_in_manifest_is_rejected() {
        let temp = TempDir::new("delete-through-local-link");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);

        // 导出后用户在本地把 skills 换成指向外部的链接：manifest 里没这条记录。
        fs::remove_dir_all(agent.join("skills")).expect("删除 skills 目录");
        write_file(&temp.path().join("outside/links/extra.md"), "不要删\n");
        symlink("../outside/links", agent.join("skills")).expect("创建本地链接");
        let expected = fingerprint(&agent, &sync_root);
        let outside = temp.path().join("outside/links");
        let outside_before = tree(&outside);
        let agent_before = tree(&agent);

        let failure = apply(
            &agent,
            &sync_root,
            &expected,
            &["skills/extra.md".to_string()],
            &data,
            &FakeRunner::ok(),
        )
        .expect_err("经符号链接删除必须整次拒绝");

        assert_eq!(failure.code, "delete_through_symlink", "{}", failure.message);
        assert!(failure.message.contains("skills"), "{}", failure.message);
        assert_eq!(failure.completed, 0);
        assert_eq!(failure.backup_dir, None);
        assert!(outside.join("extra.md").is_file(), "链接目标里的文件不得被删");
        assert_eq!(tree(&outside), outside_before);
        assert_eq!(tree(&agent), agent_before, "拒绝时不得写任何文件");
        assert!(backup_ids(&data).is_empty(), "拒绝时不得建备份");
    }

    #[test]
    fn plain_deletion_without_symlink_ancestors_still_succeeds() {
        let temp = TempDir::new("delete-plain");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);

        // 快照里没有的普通文件：删除候选，祖先全是真实目录。
        write_file(&agent.join("prompts/live-only.md"), "只在 live\n");
        let expected = fingerprint(&agent, &sync_root);

        let report = apply(
            &agent,
            &sync_root,
            &expected,
            &["prompts/live-only.md".to_string()],
            &data,
            &FakeRunner::ok(),
        )
        .expect("普通删除应成功");

        assert_eq!(report.written, 0);
        assert_eq!(report.deleted, 1);
        assert_eq!(report.skipped, 0);
        assert!(!agent.join("prompts/live-only.md").exists());

        let restore = read_restore(&report.backup_dir);
        let deleted = entry(&restore, "prompts/live-only.md");
        assert_eq!(deleted.kind, RestoreKind::File);
        assert!(deleted.existed);
        assert_eq!(
            fs::read_to_string(report.backup_dir.join("files/prompts/live-only.md"))
                .expect("读备份内容"),
            "只在 live\n"
        );
    }
}

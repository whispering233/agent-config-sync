//! 应用快照：先备份再落地，删除需逐项确认。
//!
//! 规则来源：`docs/design/10-snapshot-sync.md` 第 6、7 节。
//! 落地按 manifest 的 `managedRoots` 分根进行：`agent` 根写 `<agent_dir>`，`agents-skills`
//! 根写 `<home>/.agents/skills`。计划项、删除请求与备份条目都带 `root`，两个根里相同的
//! 相对路径因此不会串。
//! 顺序固定：重算计划 → 核对指纹与删除清单 → 备份 → 重建链接 → 写入/删除 → 清理旧备份。
//! 符号链接优先按 manifest 的 `symlinks` 重建；目标不存在（含断链）或平台不允许建链接时
//! 退回物化写入，后者要整棵子树写出：计划是按链接视角算的，只写新增/修改项会丢内容。
//! 失败停在原地：写入中途失败即停，已完成的部分留在原地，备份目录保留（回滚见 `rollback`），
//! 不自动回滚、不重试、不静默修复。核心 [`apply`] 不依赖 Tauri，可直接临时目录单测。

use crate::config;
use crate::doctor::{CommandRunner, ShellRunner};
use crate::plan;
use crate::platform::{self, HostOs};
use crate::scope::{self, ManagedRoots, RootId};
use crate::snapshot::{self, ManifestSymlink, MANIFEST_FILE_NAME};
use crate::webdav_transfer;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

/// 备份目录名（位于应用数据目录下，不进云盘）。
pub const BACKUPS_DIR_NAME: &str = "backups";
/// 备份目录内存放原文件内容的子目录。
pub const BACKUP_FILES_DIR_NAME: &str = "files";
/// 备份说明文件名。
pub const RESTORE_FILE_NAME: &str = "restore.json";
/// `restore.json` 的格式版本。回滚按这个版本读取。
///
/// 版本 2 给每个条目加了 `root`；版本 1 的旧备份没有这个字段，按 `agent` 根读取。
pub const RESTORE_VERSION: u32 = 2;
/// 回滚能读的版本。旧备份不升级、不改写，只按缺省根读取。
pub const SUPPORTED_RESTORE_VERSIONS: [u32; 2] = [1, RESTORE_VERSION];
/// 备份目录权限：只允许本用户读（Windows 没有权限位，建目录时不设）。
const BACKUP_DIR_MODE: u32 = 0o700;
/// 新增文件的权限；覆盖已有文件时沿用原 mode。
const NEW_FILE_MODE: u32 = 0o644;
/// 凭据文件的新建与覆盖权限。
const AUTH_FILE_MODE: u32 = 0o600;

/// 一次应用的输出。失败时改由 [`ApplyFailure`] 返回，`failed_at` 因此总是 `None`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyReport {
    pub fingerprint: String,
    pub backup_dir: PathBuf,
    pub backup_id: String,
    /// 实际写出的文件数：新增 + 修改的条数；链接建不成而整棵物化时，按该子树写出的文件数计。
    pub written: usize,
    /// 实际删除的条数。
    pub deleted: usize,
    /// 删除候选里未勾选、因此保留下来的条数。
    pub skipped: usize,
    /// 按 manifest 记录重建的符号链接条数。
    pub linked: usize,
    /// 已勾选、但落在链接路径之下因此没有删除的条数（含建不成而已物化的链接）。
    pub skipped_deletes: usize,
    /// 不阻断的提示：链接退回物化、链接下的删除未执行等。
    pub warnings: Vec<String>,
    pub failed_at: Option<String>,
}

/// 应用失败。失败码见 `docs/design/10-snapshot-sync.md` 第 9 节。
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
    /// 计划项都是文件，目标位置反而是目录时直接失败，所以备份里不会有目录条目。
    Dir,
}

/// 备份里的一条：所属根、原始路径、类型、原权限、链接目标。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreEntry {
    /// 所属受管根。版本 1 的旧备份没有这个字段，按 `agent` 根读取。
    #[serde(default)]
    pub root: RootId,
    /// 相对所属受管根的路径，`/` 分隔。
    pub path: String,
    pub kind: RestoreKind,
    /// 应用前该路径是否存在；新增文件为 false，此时没有内容备份。
    pub existed: bool,
    pub mode: Option<u32>,
    pub link_target: Option<String>,
}

/// 一条删除请求：所属受管根 + 根内相对路径。
///
/// 两个根可能有同一个相对路径（例如都叫 `settings.json`），裸路径分不清是哪一个，
/// 所以从前端到后端必须成对传递。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteRequest {
    pub root: RootId,
    pub path: String,
}

/// 备份说明。回滚只读这个文件就能还原。
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

/// 建链接这一步的平台原语。做成可注入，测试才能覆盖「平台不允许建链接」这条常规路径
/// （Windows 默认权限下 `ERROR_PRIVILEGE_NOT_HELD`），而不是靠改权限去碰运气。
pub trait LinkCreator {
    fn create_link(&self, target: &str, path: &Path, is_dir: bool) -> io::Result<()>;
}

/// 生产实现：直接调 [`platform::create_link`]。
pub struct PlatformLinkCreator;

impl LinkCreator for PlatformLinkCreator {
    fn create_link(&self, target: &str, path: &Path, is_dir: bool) -> io::Result<()> {
        platform::create_link(target, path, is_dir)
    }
}

/// 一次应用要落到的位置、平台接缝与备份保留数。
///
/// 参数收进结构体，免得 [`apply`] 的签名长到看不清每一项是什么。
#[derive(Clone, Copy)]
pub struct ApplyContext<'a> {
    pub agent_dir: &'a Path,
    /// 解析 `agents-skills` 根用的 home；声明了该根却解析不到就整次失败。
    pub home_dir: Option<&'a Path>,
    pub sync_root: &'a Path,
    pub app_data_dir: &'a Path,
    /// 本机备份（回滚点）保留份数，来自 `AppConfig::backup_retention`。
    pub backup_retention: usize,
    pub runner: &'a dyn CommandRunner,
    pub link_creator: &'a dyn LinkCreator,
}

/// core：应用快照。`delete_requests` 是用户逐项勾选确认的删除项（带所属根），
/// 其余删除候选原样保留。
///
/// 落地按 manifest 的 `managedRoots` 分根：每个写入、删除、链接、备份条目都用它的
/// `root` 找到本机根目录。
pub fn apply(
    context: &ApplyContext<'_>,
    expected_fingerprint: &str,
    delete_requests: &[DeleteRequest],
) -> Result<ApplyReport, ApplyFailure> {
    let ApplyContext {
        agent_dir,
        home_dir,
        sync_root,
        app_data_dir,
        backup_retention,
        runner,
        link_creator,
    } = *context;
    // 计划可能已经过时：用户看的是旧计划，落地前必须按当前磁盘重算一次。
    let report = plan::plan_snapshot(agent_dir, home_dir, sync_root, runner, HostOs::current())
        .map_err(|failure| {
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
    let recorded = read_manifest_records(sync_root)?;
    // 根的解析与计划同源：声明了哪个根就只落哪个根，解析不到 home 就停。
    let roots = plan::resolve_roots(agent_dir, home_dir, &recorded.managed_roots)
        .map_err(|failure| ApplyFailure::new(failure.code, failure.message))?;
    let mut warnings: Vec<String> = root_link_warnings(&roots);

    let confirmed = confirm_deletes(&roots, &report.plan, delete_requests)?;
    // 导出时链接被物化成内容，落地时优先按 manifest 的记录重建。
    let candidates = rebuildable_links(&roots, &recorded.symlinks, &mut warnings);
    // 删除会跟随祖先目录里的符号链接落到外部位置；将被重建的链接不算，其余必须在建备份前整次拒绝。
    reject_delete_through_symlink(&roots, &confirmed, &candidates)?;
    let skipped = report.plan.deleted.len() - confirmed.len();

    let backup = create_backup_dir(app_data_dir)?;
    write_backup(
        &roots,
        &report.plan,
        &report.manifest_info.fingerprint,
        &confirmed,
        &backup,
    )?;
    prune(&app_data_dir.join(BACKUPS_DIR_NAME), backup_retention);

    // 建链接失败就退回物化：Windows 默认权限下没有建链接权限（`ERROR_PRIVILEGE_NOT_HELD`）
    // 是常态，与「目标缺失」一样只告警、不中断。物化要把快照里该路径下的整棵子树写成真实文件：
    // 旧链接已经被清掉，而计划是按链接视角算的，只写新增/修改项会丢掉与快照一致、
    // 根本不在计划里的内容。
    let mut rebuilt: Vec<&RebuildLink> = Vec::new();
    let mut materialized: Vec<&RebuildLink> = Vec::new();
    let mut written = 0;
    for link in &candidates {
        let Some(root_dir) = roots.path(link.root) else {
            return Err(root_undeclared(&backup, rebuilt.len() + written, link.root));
        };
        match rebuild_link(root_dir, link, link_creator) {
            Ok(()) => rebuilt.push(link),
            Err(link_error) => {
                let files =
                    materialize_snapshot_subtree(root_dir, sync_root, link.root, &link.relative)
                        .map_err(|error| {
                            materialize_failed(
                                &backup,
                                rebuilt.len() + written,
                                link,
                                &link_error,
                                &error,
                            )
                        })?;
                written += files;
                warnings.push(format!(
                    "{} 无法重建为链接（{link_error}），已把快照里该目录的整棵内容写成真实文件",
                    link.root.describe(&link.relative)
                ));
                materialized.push(link);
            }
        }
    }
    let linked = rebuilt.len();

    for item in report.plan.added.iter().chain(&report.plan.modified) {
        // 链接之下的内容由链接目标提供；链接建不成时，那棵子树也已随物化整棵写出。
        if candidates
            .iter()
            .any(|link| link.root == item.root && is_under(&item.path, &link.relative))
        {
            continue;
        }
        let Some(root_dir) = roots.path(item.root) else {
            return Err(root_undeclared(&backup, linked + written, item.root));
        };
        write_snapshot_file(root_dir, item.root, sync_root, &item.path)
            .map_err(|error| fail(&backup, linked + written, item.root, &item.path, error))?;
        written += 1;
    }

    let mut deleted = 0;
    let mut deleted_external = 0;
    let mut skipped_deletes = 0;
    let mut skipped_under_materialized = 0;
    for request in &confirmed {
        let relative = request.path.as_str();
        // 删除一律以「候选链接」为准：计划是按链接视角算的，对着已经移走的位置执行会 ENOENT。
        if let Some(link) = candidates
            .iter()
            .find(|link| link.root == request.root && is_under(relative, &link.relative))
        {
            skipped_deletes += 1;
            if materialized
                .iter()
                .any(|item| item.root == link.root && item.relative == link.relative)
            {
                skipped_under_materialized += 1;
                // 链接换成物化目录时旧路径已经不在了：这条不是“保留着没删”，而是随目录替换从受管路径上消失。
                // 原内容只在链接目标与本次备份里。
                warnings.push(format!(
                    "{} 落在 {} 之下；该链接无法重建，已按快照整棵物化，它已随目录替换从受管路径上消失（链接目标里的原文件未动），原内容在本次备份中",
                    request.root.describe(relative),
                    link.relative
                ));
            } else {
                warnings.push(format!(
                    "{} 由链接 {} 的目标提供，未删除",
                    request.root.describe(relative),
                    link.relative
                ));
            }
            continue;
        }
        let Some(root_dir) = roots.path(request.root) else {
            return Err(root_undeclared(
                &backup,
                linked + written + deleted,
                request.root,
            ));
        };
        delete_target(root_dir, relative).map_err(|error| {
            fail(
                &backup,
                linked + written + deleted,
                request.root,
                relative,
                error,
            )
        })?;
        deleted += 1;
        if request.root == RootId::AgentsSkills {
            deleted_external += 1;
        }
    }
    if skipped_under_materialized > 0 {
        warnings.push(format!(
            "因链接无法重建，这 {skipped_under_materialized} 项勾选删除已随目录替换消失（不是单独删除），原内容在本次备份中"
        ));
    }
    // 共享目录里的删除是这次应用影响面最大的一步：完了再说一句，让人知道别处可能受影响。
    if deleted_external > 0 {
        warnings.push(format!(
            "已从共享目录 {} 删除 {deleted_external} 项：其它 agent 工具也可能使用这些技能；原内容已进入本次备份",
            RootId::AgentsSkills.label()
        ));
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

fn fail(
    backup: &Backup,
    completed: usize,
    root: RootId,
    path: &str,
    error: io::Error,
) -> ApplyFailure {
    ApplyFailure {
        code: "apply_failed",
        message: format!(
            "落地 {} 时失败：{error}；已立即停止，备份在 {}",
            root.describe(path),
            backup.dir.display()
        ),
        completed,
        backup_dir: Some(backup.dir.clone()),
    }
}

/// 计划或链接记录里的根没在 manifest 里声明：manifest 自相矛盾，停在原地。
fn root_undeclared(backup: &Backup, completed: usize, root: RootId) -> ApplyFailure {
    ApplyFailure {
        code: "apply_failed",
        message: format!(
            "快照没有声明 {} 根：拒绝落地它的条目；已立即停止，备份在 {}",
            root.label(),
            backup.dir.display()
        ),
        completed,
        backup_dir: Some(backup.dir.clone()),
    }
}

/// 建链接失败、改写快照内容也失败：两条原因都带上，停在原地等人工处理。
fn materialize_failed(
    backup: &Backup,
    completed: usize,
    link: &RebuildLink,
    link_error: &io::Error,
    error: &io::Error,
) -> ApplyFailure {
    ApplyFailure {
        code: "apply_failed",
        message: format!(
            "{} 重建链接失败（{link_error}），改成写入快照内容也失败（{error}）；已立即停止，备份在 {}",
            link.root.describe(&link.relative),
            backup.dir.display()
        ),
        completed,
        backup_dir: Some(backup.dir.clone()),
    }
}

/// 根的类型不进入快照：链接只跟随内容。导出侧已给过一条告警，落地这边再说一次，
/// 因为写入与删除会落到链接目标而不是根路径本身。
fn root_link_warnings(roots: &ManagedRoots) -> Vec<String> {
    roots
        .iter()
        .filter(|root| platform::is_link_like(&root.path))
        .map(|root| {
            format!(
                "{}（{}）是符号链接：只同步其内容，不记录根链接；写入与删除会落到链接目标",
                root.id.label(),
                root.path.display()
            )
        })
        .collect()
}

/// 删除清单校验：每一项都必须是重算计划里的删除候选（根 + 路径都要对上）。
/// 任何一项不合格就整次拒绝，不做部分执行。
fn confirm_deletes(
    roots: &ManagedRoots,
    plan: &plan::Plan,
    delete_requests: &[DeleteRequest],
) -> Result<Vec<DeleteRequest>, ApplyFailure> {
    let mut confirmed: Vec<DeleteRequest> = Vec::new();
    for request in delete_requests {
        if !plan
            .deleted
            .iter()
            .any(|item| item.root == request.root && item.path == request.path)
        {
            return Err(delete_not_in_plan(request.root, &request.path));
        }
        // 计划里的删除候选只来自受管范围；`resolve_within` 再挡一次空路径、`..` 与绝对路径。
        let root_dir = roots.path(request.root).ok_or_else(|| {
            ApplyFailure::new(
                "manifest_invalid",
                format!(
                    "快照没有声明 {} 根，却给出了它的删除候选；整次应用已拒绝",
                    request.root.label()
                ),
            )
        })?;
        if scope::resolve_within(root_dir, Path::new(&request.path)).is_err() {
            return Err(delete_not_in_plan(request.root, &request.path));
        }
        if !confirmed.contains(request) {
            confirmed.push(request.clone());
        }
    }
    Ok(confirmed)
}

fn delete_not_in_plan(root: RootId, path: &str) -> ApplyFailure {
    ApplyFailure::new(
        "delete_not_in_plan",
        format!(
            "{} 不在本次计划的删除候选里，或不在受管范围内；整次应用已拒绝",
            root.describe(path)
        ),
    )
}

/// 删除前置校验：待删路径的祖先目录（不含自身）是符号链接时整次拒绝。
/// 将被重建的 manifest 链接不算：它们会指回原目标，链接下的删除改由跳过处理。
/// 此时还没有建备份、没有写任何文件，用户手动处理链接后可以原样重跑。
fn reject_delete_through_symlink(
    roots: &ManagedRoots,
    confirmed: &[DeleteRequest],
    rebuilt: &[RebuildLink],
) -> Result<(), ApplyFailure> {
    for request in confirmed {
        let Some(root_dir) = roots.path(request.root) else {
            continue;
        };
        let ancestor = symlink_ancestors(root_dir, &request.path)
            .into_iter()
            .find(|ancestor| {
                !rebuilt
                    .iter()
                    .any(|link| link.root == request.root && link.relative == *ancestor)
            });
        if let Some(ancestor) = ancestor {
            return Err(ApplyFailure::new(
                "delete_through_symlink",
                format!(
                    "{} 的路径经过符号链接 {ancestor}，拒绝删除；请先手动处理该目录",
                    request.root.describe(&request.path)
                ),
            ));
        }
    }
    Ok(())
}

/// 一条待重建的链接：所属根、根内相对路径与 manifest 记下的原目标。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RebuildLink {
    root: RootId,
    relative: String,
    target: String,
    /// 目标是目录还是文件；Windows 只有这两种链接，Unix 建链接不分类型。
    is_dir: bool,
}

/// 落地时真正需要的 manifest 字段。完整校验已在计划阶段做过。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestRecords {
    #[serde(default)]
    managed_roots: Vec<RootId>,
    #[serde(default)]
    symlinks: Vec<ManifestSymlink>,
}

fn read_manifest_records(sync_root: &Path) -> Result<ManifestRecords, ApplyFailure> {
    let path = sync_root.join(MANIFEST_FILE_NAME);
    let text = fs::read_to_string(&path).map_err(|error| {
        ApplyFailure::new(
            "manifest_invalid",
            format!("读取 manifest 里的受管根与链接记录失败：{error}"),
        )
    })?;
    serde_json::from_str(&text).map_err(|error| {
        ApplyFailure::new(
            "manifest_invalid",
            format!("manifest 里的受管根与链接记录无法解析：{error}"),
        )
    })
}

/// 算出要重建的链接：所在根已声明、路径在受管范围内、目标存在。
/// 目标不存在（含断链）就不重建，退回物化写入并记 warning。
fn rebuildable_links(
    roots: &ManagedRoots,
    links: &[ManifestSymlink],
    warnings: &mut Vec<String>,
) -> Vec<RebuildLink> {
    let mut rebuilt: Vec<RebuildLink> = Vec::new();
    for link in links {
        let Some((root, relative_path)) = snapshot::split_manifest_path(&link.path)
            .filter(|(_, relative)| !relative.as_os_str().is_empty())
        else {
            warnings.push(format!("{} 不在快照目录之下，未重建链接", link.path));
            continue;
        };
        let relative = slash(&relative_path);
        let Some(root_dir) = roots.path(root) else {
            warnings.push(format!(
                "{} 所属的 {} 根不在本快照里，未重建链接",
                relative,
                root.label()
            ));
            continue;
        };
        // auth.json 只以普通文件落地，不按 manifest 重建链接，避免把凭据指向范围外。
        if is_credential(root, &relative) {
            warnings.push(format!("{} 是凭据文件，未按链接重建", relative));
            continue;
        }
        // 被改过的 manifest 可能指向受管范围之外：不重建。
        if !is_managed_link(root, &relative_path)
            || scope::resolve_within(root_dir, &relative_path).is_err()
        {
            warnings.push(format!("{} 不在受管范围内，未重建链接", relative));
            continue;
        }
        let Some(parent) = relative_path.parent() else {
            warnings.push(format!("{relative} 没有父目录，未重建链接"));
            continue;
        };
        // 链接目标相对链接所在目录解释；绝对目标直接决定位置。
        // Windows 建链接要按目标类型分开调，所以这里顺手记下目标是文件还是目录。
        let target_metadata = match fs::metadata(root_dir.join(parent).join(&link.target)) {
            Ok(metadata) => metadata,
            Err(_) => {
                warnings.push(format!(
                    "{relative} 的链接目标 {} 不存在，已退回写入快照内容",
                    link.target
                ));
                continue;
            }
        };
        if rebuilt
            .iter()
            .any(|item| item.root == root && item.relative == relative)
        {
            continue;
        }
        rebuilt.push(RebuildLink {
            root,
            relative,
            target: link.target.clone(),
            is_dir: target_metadata.is_dir(),
        });
    }
    // 外层链接先建，嵌套链接才能落到链接目标里；同根内按路径排序，外层自然在前。
    rebuilt.sort_by(|left, right| {
        left.root
            .cmp(&right.root)
            .then_with(|| left.relative.cmp(&right.relative))
    });
    rebuilt
}

/// 重建一条链接：父目录就位后清掉旧路径，再按原目标建立链接。
/// 旧路径是真目录时整棵删除：里面的内容按约定由链接目标提供，不保留本地副本。
/// 建链接失败时旧内容已经不在，调用方必须整棵物化快照内容补回。
fn rebuild_link(
    root_dir: &Path,
    link: &RebuildLink,
    link_creator: &dyn LinkCreator,
) -> io::Result<()> {
    let path = scope::resolve_within(root_dir, Path::new(&link.relative)).map_err(to_io_error)?;
    if let Some(parent) = path.parent() {
        // 父目录可以本身就是链接：跟随它，不换成真实目录。
        fs::create_dir_all(parent)?;
    }
    // 旧路径本身也是链接时只删链接：按目录删会连目标内容一起删掉。
    match fs::symlink_metadata(&path) {
        Ok(_) if platform::is_link_like(&path) => platform::remove_link(&path)?,
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&path)?,
        Ok(_) => fs::remove_file(&path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    link_creator.create_link(&link.target, &path, link.is_dir)
}

/// `path` 是否就是 `ancestor` 或在它之下；按路径分段判，不把 `skills-extra` 当成 `skills` 的子项。
fn is_under(path: &str, ancestor: &str) -> bool {
    path == ancestor
        || path
            .strip_prefix(ancestor)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// 链接路径必须在所属根的受管范围内：规则与计划同源（[`scope::is_managed_relative`]），
/// 目的是不让被改过的 manifest 借链接删掉范围外文件。
pub(crate) fn is_managed_link(root: RootId, relative: &Path) -> bool {
    scope::is_managed_relative(root, relative)
}

/// 凭据文件只认 `agent` 根的根级 `auth.json`：权限收紧到 0600，且不按链接重建。
fn is_credential(root: RootId, relative: &str) -> bool {
    root == RootId::Agent && relative == "auth.json"
}

/// 根内相对路径 → `/` 分隔的字符串，与计划项和备份条目里的写法一致。
fn slash(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// 建备份目录：`<app_data>/backups/<UTC 时间戳>/files/`，权限 0700。
fn create_backup_dir(app_data_dir: &Path) -> Result<Backup, ApplyFailure> {
    let root = app_data_dir.join(BACKUPS_DIR_NAME);
    let created_at = utc_timestamp(now_seconds());
    let base = created_at.replace(':', "-");

    // 同一秒内应用多次时加序号，避免两次备份混在一个目录里。序号只增不减：
    // 旧备份被保留数裁剪掉后从最小空位重排，新备份在字典序上就排得更旧，会被当次裁剪删掉。
    let suffix = fs::read_dir(&root)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| backup_suffix(&base, &entry.file_name().to_string_lossy()))
                .max()
                .map_or(0, |max| max + 1)
        })
        .unwrap_or(0);
    let id = if suffix == 0 {
        base.clone()
    } else {
        format!("{base}-{suffix}")
    };
    let dir = root.join(&id);

    platform::create_dir_all_mode(&dir, BACKUP_DIR_MODE)
        .and_then(|()| {
            platform::create_dir_all_mode(&dir.join(BACKUP_FILES_DIR_NAME), BACKUP_DIR_MODE)
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

/// 备份目录名 `base` 或 `base-<序号>` 里的序号；别的名字不算同一秒的备份。
fn backup_suffix(base: &str, name: &str) -> Option<usize> {
    match name.strip_prefix(base)? {
        "" => Some(0),
        rest => rest.strip_prefix('-')?.parse().ok(),
    }
}

/// 备份将被覆盖或删除的现有文件，写下 `restore.json`。
fn write_backup(
    roots: &ManagedRoots,
    plan: &plan::Plan,
    fingerprint: &str,
    confirmed: &[DeleteRequest],
    backup: &Backup,
) -> Result<(), ApplyFailure> {
    let mut touched: Vec<(RootId, &str)> = plan
        .added
        .iter()
        .chain(&plan.modified)
        .map(|item| (item.root, item.path.as_str()))
        .collect();
    touched.extend(
        confirmed
            .iter()
            .map(|request| (request.root, request.path.as_str())),
    );

    write_backup_entries(roots, &touched, fingerprint, backup, false)
}

/// 回滚前再备份一次：把即将被改动或删除的路径按同一格式写进新备份目录。
/// 返回备份目录名与位置；`fingerprint` 由调用方给（回滚时记被还原备份的指纹）。
pub(crate) fn back_up_paths(
    roots: &ManagedRoots,
    app_data_dir: &Path,
    touched: &[(RootId, &str)],
    fingerprint: &str,
) -> Result<(String, PathBuf), ApplyFailure> {
    let backup = create_backup_dir(app_data_dir)?;
    write_backup_entries(roots, touched, fingerprint, &backup, true)?;
    // 回滚是 core 入口、不加载应用配置：还原前的那份备份按默认份数清理，不上抛配置依赖。
    prune(
        &app_data_dir.join(BACKUPS_DIR_NAME),
        config::DEFAULT_BACKUP_RETENTION as usize,
    );

    Ok((backup.id, backup.dir))
}

/// `allow_dir` 为 true 时允许目录条目：回滚前的安全备份不能因为某个路径此刻是目录就整次失败。
fn write_backup_entries(
    roots: &ManagedRoots,
    touched: &[(RootId, &str)],
    fingerprint: &str,
    backup: &Backup,
    allow_dir: bool,
) -> Result<(), ApplyFailure> {
    let files_dir = backup.dir.join(BACKUP_FILES_DIR_NAME);
    let mut entries = Vec::new();
    // 写入时 `ensure_dir` 会把祖先链接换成真实目录，链接本身只能在写入前记下来；
    // 这些条目放在文件条目之前，回滚才能先恢复链接再往里放文件。
    for root in roots.iter() {
        let relatives: Vec<&str> = touched
            .iter()
            .filter(|(id, _)| *id == root.id)
            .map(|(_, relative)| *relative)
            .collect();
        if relatives.is_empty() {
            continue;
        }
        for ancestor in symlink_ancestors_of(&root.path, &relatives) {
            entries.push(record(
                root.id, &root.path, &ancestor, &files_dir, allow_dir,
            )?);
        }
    }
    for (root, relative) in touched {
        let Some(root_dir) = roots.path(*root) else {
            return Err(ApplyFailure::new(
                "backup_failed",
                format!("快照没有声明 {} 根，无法备份它的条目", root.label()),
            ));
        };
        entries.push(record(*root, root_dir, relative, &files_dir, allow_dir)?);
    }

    write_manifest(backup, fingerprint, entries)
}

fn write_manifest(
    backup: &Backup,
    fingerprint: &str,
    entries: Vec<RestoreEntry>,
) -> Result<(), ApplyFailure> {
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
pub(crate) fn symlink_ancestors(root_dir: &Path, relative: &str) -> Vec<String> {
    let mut current = root_dir.to_path_buf();
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
        if platform::is_link_like(&current) {
            found.push(ancestor.clone());
        }
    }
    found
}

/// 备份单个路径：文件复制内容，符号链接只记目标，新增文件只记 `existed: false`。
/// `allow_dir` 只给回滚前的备份用：此刻是目录就记成目录条目，不挡回滚。
fn record(
    root: RootId,
    root_dir: &Path,
    relative: &str,
    files_dir: &Path,
    allow_dir: bool,
) -> Result<RestoreEntry, ApplyFailure> {
    let live = scope::resolve_within(root_dir, Path::new(relative)).map_err(|error| {
        ApplyFailure::new(
            "backup_failed",
            format!("无法定位 {}：{error}", root.describe(relative)),
        )
    })?;

    let metadata = match fs::symlink_metadata(&live) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RestoreEntry {
                root,
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
                format!("读取 {} 的信息失败：{error}", root.describe(relative)),
            ))
        }
    };

    // 链接必须先判：Windows 的目录型链接同时带目录属性，当成目录就会误报「无法作为文件应用」。
    let link_target = if platform::is_link_like(&live) {
        let target = fs::read_link(&live).map_err(|error| {
            ApplyFailure::new(
                "backup_failed",
                format!("读取 {} 的链接目标失败：{error}", root.describe(relative)),
            )
        })?;
        Some(target.to_string_lossy().into_owned())
    } else {
        None
    };

    // 计划项都是文件；目标位置此刻是目录说明计划期间磁盘变了，停下让人看，不覆盖。
    if link_target.is_none() && metadata.is_dir() {
        if !allow_dir {
            return Err(ApplyFailure::new(
                "backup_failed",
                format!(
                    "{} 现在是目录，无法作为文件应用；请重新检查后再应用",
                    root.describe(relative)
                ),
            ));
        }
        // 目录内容不会被回滚动到，回滚遇到目录也拒绝，所以只记条目。
        return Ok(RestoreEntry {
            root,
            path: relative.to_string(),
            kind: RestoreKind::Dir,
            existed: true,
            mode: platform::mode_of(&metadata),
            link_target: None,
        });
    }
    // 链接没有有意义的权限位；Windows 上 `mode_of` 一律给 `None`。
    let mode = match link_target {
        Some(_) => None,
        None => platform::mode_of(&metadata),
    };

    if link_target.is_none() {
        // 备份内容按虚拟路径放：外部根落在 `external/agents-skills/...`，与快照布局一致。
        let destination = files_dir.join(snapshot::virtual_path(root, Path::new(relative)));
        if let Some(parent) = destination.parent() {
            platform::create_dir_all_mode(parent, BACKUP_DIR_MODE).map_err(|error| {
                ApplyFailure::new("backup_failed", format!("创建备份子目录失败：{error}"))
            })?;
        }
        fs::copy(&live, &destination).map_err(|error| {
            ApplyFailure::new(
                "backup_failed",
                format!("备份 {} 失败：{error}", root.describe(relative)),
            )
        })?;
    }

    Ok(RestoreEntry {
        root,
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

/// 只保留最近 `keep` 份；目录名是 UTC 时间戳，按名字排序即按时间排序。
fn prune(backups: &Path, keep: usize) {
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
    let excess = ids.len().saturating_sub(keep);
    for id in ids.into_iter().take(excess) {
        let _ = fs::remove_dir_all(backups.join(id));
    }
}

/// 写入一个快照文件：`<sync_root>/snapshot/<虚拟路径>` 落到所属根目录的同名位置。
fn write_snapshot_file(
    root_dir: &Path,
    root: RootId,
    sync_root: &Path,
    relative: &str,
) -> io::Result<()> {
    let source = snapshot_source(sync_root, root, relative)?;
    let target = scope::resolve_within(root_dir, Path::new(relative)).map_err(to_io_error)?;
    write_file_content(&source, &target, root_dir, is_credential(root, relative))
}

/// 快照目录里某个（根, 根内相对路径）的文件位置。
fn snapshot_source(sync_root: &Path, root: RootId, relative: &str) -> io::Result<PathBuf> {
    let manifest_path = snapshot::manifest_path(root, Path::new(relative));
    scope::resolve_within(sync_root, Path::new(&manifest_path)).map_err(to_io_error)
}

/// 把快照里某个链接路径下的整棵子树写成真实文件，返回写出的文件数。
/// 链接建不成时旧内容已经被清掉，只补计划里的新增/修改项会漏掉与快照一致的内容，所以整棵写出。
fn materialize_snapshot_subtree(
    root_dir: &Path,
    sync_root: &Path,
    root: RootId,
    relative: &str,
) -> io::Result<usize> {
    let source = snapshot_source(sync_root, root, relative)?;
    let target = scope::resolve_within(root_dir, Path::new(relative)).map_err(to_io_error)?;
    materialize_subtree(&source, &target, root_dir)
}

/// 递归物化：目录就地建出来（空目录也照样落地），普通文件逐个按快照内容写。
/// 回滚重建链接失败时也走这里，源换成链接目标。
/// 写出的都是链接子树里的普通内容：`auth.json` 的凭据特例不适用于这些位置。
/// `root` 是 `target` 所属的受管根：根本身是链接时跟随它，不换成真实目录。
pub(crate) fn materialize_subtree(source: &Path, target: &Path, root: &Path) -> io::Result<usize> {
    if fs::metadata(source)?.is_dir() {
        ensure_dir(root, target)?;
        let mut written = 0;
        for item in fs::read_dir(source)? {
            let item = item?;
            written += materialize_subtree(&item.path(), &target.join(item.file_name()), root)?;
        }
        Ok(written)
    } else {
        write_file_content(source, target, root, false)?;
        Ok(1)
    }
}

/// 先写同目录临时文件再替换，权限沿用原 mode。
/// `root` 是 `target` 所属的受管根，交给 [`ensure_dir`] 决定根链接是跟随还是替换。
/// `credential` 为 true 表示这是 `agent` 根级 `auth.json`：不论新建还是覆盖都收紧到 0600。
fn write_file_content(
    source: &Path,
    target: &Path,
    root: &Path,
    credential: bool,
) -> io::Result<()> {
    let bytes = fs::read(source)?;
    // 其他文件沿用原 mode，新增用 0644。
    // Windows 上 `set_mode` 不生效，保护来自用户目录的继承 ACL。
    let mode = if credential {
        AUTH_FILE_MODE
    } else {
        match fs::symlink_metadata(target) {
            Ok(metadata) if metadata.is_file() => {
                platform::mode_of(&metadata).unwrap_or(NEW_FILE_MODE)
            }
            _ => NEW_FILE_MODE,
        }
    };
    if let Some(parent) = target.parent() {
        ensure_dir(root, parent)?;
    }

    // 目标是链接（或目录）时先删掉再写真实文件，绝不写穿链接。
    // 链接必须先判：Windows 的目录型链接同时带目录属性，按目录删会连目标内容一起删。
    match fs::symlink_metadata(target) {
        Ok(_) if platform::is_link_like(target) => platform::remove_link(target)?,
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(target)?,
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
        // rename 覆盖已存在的文件在两个平台都成立（Unix 是 rename，Windows 是
        // MoveFileExW + MOVEFILE_REPLACE_EXISTING）；只有目标是目录才失败，
        // 而链接与目录已经在上面删掉了。
        platform::set_mode(&temp, mode)?;
        fs::rename(&temp, target)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

/// 逐级建目录；根之下的链接先删链接再建真目录，绝不写穿链接。
///
/// `root` 自己（及其上级）是链接时跟随它：根的类型不进快照，落地不重建根链接，
/// 写入与删除都落到链接目标。把它换成真实目录会让目标机莫名其妙多出一份副本。
fn ensure_dir(root: &Path, dir: &Path) -> io::Result<()> {
    let mut current = PathBuf::new();
    for component in dir.components() {
        current.push(component);
        // 链接必须先判：Windows 的目录型链接同时带目录属性，当成目录就是写穿。
        // 根自己不算：它下面才是受管内容，根本身按上面的规则跟随。
        if current != root && current.starts_with(root) && platform::is_link_like(&current) {
            platform::remove_link(&current)?;
            fs::create_dir(&current)?;
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            // 根（或根的上级）是链接：跟着它走，内容照样落在链接目标里。
            Ok(_) if platform::is_link_like(&current) => {}
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
fn delete_target(root_dir: &Path, relative: &str) -> io::Result<()> {
    // 上级组件是符号链接时，删除会落到链接指向的外部目录，一律不动。
    // 正常流程已在 [`reject_delete_through_symlink`] 挡住，这里挡住校验之后才出现的链接。
    if let Some(ancestor) = symlink_ancestors(root_dir, relative).into_iter().next() {
        return Err(io::Error::other(format!(
            "{relative} 的路径经过符号链接 {ancestor}，删除会落到链接指向的外部位置，已拒绝"
        )));
    }
    let target = scope::resolve_within(root_dir, Path::new(relative)).map_err(to_io_error)?;

    // 链接只删链接本身：Windows 的目录型链接按目录删会连目标内容一起删。
    match fs::symlink_metadata(&target)? {
        _ if platform::is_link_like(&target) => platform::remove_link(&target),
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
///
/// `root` 为 `Some` 时用它顶替同步根：WebDAV 下载把快照放在暂存目录里，应用也必须能
/// 指向那里。允许的根与计划用同一套判定（[`webdav_transfer::resolve_root`]）。
#[tauri::command]
pub async fn apply_snapshot(
    app: AppHandle,
    expected_fingerprint: String,
    delete_requests: Vec<DeleteRequest>,
    root: Option<String>,
) -> Result<ApplyReport, ApplyFailure> {
    tauri::async_runtime::spawn_blocking(move || {
        run(&app, &expected_fingerprint, &delete_requests, root)
    })
    .await
    .map_err(|error| ApplyFailure::new("apply_failed", format!("应用失败：{error}")))?
}

fn run(
    app: &AppHandle,
    expected_fingerprint: &str,
    delete_requests: &[DeleteRequest],
    root: Option<String>,
) -> Result<ApplyReport, ApplyFailure> {
    let _guard = crate::runtime::lock_operation();
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

    let sync_root = match root {
        Some(requested) => {
            webdav_transfer::resolve_root(config.sync_root.as_deref(), &app_data_dir, &requested)
                .map_err(|message| ApplyFailure::new("root_not_allowed", message))?
        }
        None => config.sync_root.clone().ok_or_else(|| {
            ApplyFailure::new(
                "sync_root_missing",
                "还没有设置同步文件夹，请先在设置页选择",
            )
        })?,
    };

    // 只读取这一个文档化的环境变量；错误信息里不回显其内容。
    let env_agent_dir = std::env::var_os(config::AGENT_DIR_ENV_VAR).map(PathBuf::from);
    let home_dir = app.path().home_dir().ok();
    let resolved =
        config::resolve_agent_dir(&config, env_agent_dir.as_deref(), home_dir.as_deref());
    let agent_dir = resolved.path.ok_or_else(|| {
        ApplyFailure::new("agent_dir_missing", "无法确定宿主目录，请先在设置页填写")
    })?;

    apply(
        &ApplyContext {
            agent_dir: &agent_dir,
            home_dir: home_dir.as_deref(),
            sync_root: &sync_root,
            app_data_dir: &app_data_dir,
            backup_retention: config.backup_retention as usize,
            runner: &ShellRunner::from_env(),
            link_creator: &PlatformLinkCreator,
        },
        expected_fingerprint,
        delete_requests,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::CommandOutcome;
    #[cfg(unix)]
    use std::os::unix::fs::{symlink, PermissionsExt};
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

    // 上下文按值借用接缝，测试夹具给它们一个与用例同寿命的位置。
    static FAKE_RUNNER: FakeRunner = FakeRunner;
    static PLATFORM_LINK_CREATOR: PlatformLinkCreator = PlatformLinkCreator;

    /// 默认应用上下文：假命令 + 真链接接缝 + 配置里的默认备份保留数。
    fn context<'a>(agent: &'a Path, sync_root: &'a Path, data: &'a Path) -> ApplyContext<'a> {
        apply_context(
            agent,
            sync_root,
            data,
            &PLATFORM_LINK_CREATOR,
            config::DEFAULT_BACKUP_RETENTION as usize,
        )
    }

    /// 与 [`context`] 同，但可以换链接接缝与备份保留数。
    fn apply_context<'a>(
        agent: &'a Path,
        sync_root: &'a Path,
        data: &'a Path,
        link_creator: &'a dyn LinkCreator,
        backup_retention: usize,
    ) -> ApplyContext<'a> {
        ApplyContext {
            agent_dir: agent,
            home_dir: None,
            sync_root,
            app_data_dir: data,
            backup_retention,
            runner: &FAKE_RUNNER,
            link_creator,
        }
    }

    /// 双根用例的上下文：`home_dir` 决定 `agents-skills` 根落到哪个目录。
    fn context_with_home<'a>(
        agent: &'a Path,
        home: &'a Path,
        sync_root: &'a Path,
        data: &'a Path,
    ) -> ApplyContext<'a> {
        context_with_home_and_links(agent, home, sync_root, data, &PLATFORM_LINK_CREATOR)
    }

    /// 与 [`context_with_home`] 同，但可以换链接接缝：双根用例也要覆盖"链接建不成"。
    fn context_with_home_and_links<'a>(
        agent: &'a Path,
        home: &'a Path,
        sync_root: &'a Path,
        data: &'a Path,
        link_creator: &'a dyn LinkCreator,
    ) -> ApplyContext<'a> {
        ApplyContext {
            home_dir: Some(home),
            ..apply_context(
                agent,
                sync_root,
                data,
                link_creator,
                config::DEFAULT_BACKUP_RETENTION as usize,
            )
        }
    }

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("创建父目录");
        }
        fs::write(path, content).expect("写入夹具文件");
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path)
            .expect("读取文件信息")
            .permissions()
            .mode()
            & 0o777
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("设置夹具权限");
    }

    /// 删除请求：根 + 根内相对路径。两个根的同名路径因此不会串。
    fn delete(root: RootId, path: &str) -> DeleteRequest {
        DeleteRequest {
            root,
            path: path.to_string(),
        }
    }

    /// 双根夹具：快照声明了 `~/.agents/skills`，目标机的外部根有一处被改、
    /// 还有一项快照里没有的技能（删除候选）。返回宿主目录、同步目录与目标机 home。
    fn external_root_fixture(temp: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
        let agent = temp.path().join("agent");
        let source_home = temp.path().join("source-home");
        let sync_root = temp.path().join("cloud");
        write_file(&agent.join("settings.json"), "{\"theme\": \"dark\"}");
        write_file(
            &source_home.join(".agents/skills/shared/SKILL.md"),
            "# 共享技能\n",
        );
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        crate::snapshot::export_fixture_with(
            &agent,
            Some(&source_home),
            &sync_root,
            crate::snapshot::SourceVersions::default(),
        )
        .expect("导出夹具快照");

        let target_home = temp.path().join("target-home");
        write_file(
            &target_home.join(".agents/skills/shared/SKILL.md"),
            "# 共享技能（本机改过）\n",
        );
        write_file(
            &target_home.join(".agents/skills/only-here/SKILL.md"),
            "# 只在目标机\n",
        );
        (agent, sync_root, target_home)
    }

    /// 宿主目录 + 已导出的快照：先保证"无差异"是基线。
    fn fixture(temp: &TempDir) -> (PathBuf, PathBuf) {
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        write_file(&agent.join("settings.json"), "{\"theme\": \"dark\"}");
        write_file(&agent.join("AGENTS.md"), "# 指令\n");
        write_file(&agent.join("skills/antd/SKILL.md"), "# 技能\n");
        // auth.json 进快照；sessions 在范围外，永不参与计划。
        write_file(&agent.join("auth.json"), "{\"token\": \"sk-test\"}");
        write_file(&agent.join("sessions/old.json"), "{}");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        crate::snapshot::export_fixture(&agent, &sync_root).expect("导出夹具快照");
        (agent, sync_root)
    }

    fn app_data(temp: &TempDir) -> PathBuf {
        let dir = temp.path().join("app-data");
        fs::create_dir_all(&dir).expect("创建应用数据目录");
        dir
    }

    fn fingerprint(agent: &Path, sync_root: &Path) -> String {
        plan::plan_snapshot(agent, None, sync_root, &FakeRunner::ok(), HostOs::Posix)
            .expect("生成夹具计划")
            .manifest_info
            .fingerprint
    }

    /// 双根夹具的指纹：目标机的 home 参与外部根解析，必须同一个 home。
    fn fingerprint_for(agent: &Path, home: &Path, sync_root: &Path) -> String {
        plan::plan_snapshot(
            agent,
            Some(home),
            sync_root,
            &FakeRunner::ok(),
            HostOs::Posix,
        )
        .expect("生成夹具计划")
        .manifest_info
        .fingerprint
    }

    /// 整棵子树的路径与内容，用于证明"没有写任何文件"。
    fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut entries = Vec::new();
        for item in fs::read_dir(root).expect("读取目录") {
            let path = item.expect("目录项").path();
            let name = path
                .file_name()
                .expect("文件名")
                .to_string_lossy()
                .into_owned();
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
            &context(&agent, &sync_root, &data),
            &expected,
            &[delete(RootId::Agent, "themes/old.json")],
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
        assert!(
            agent.join("prompts/leftover.md").is_file(),
            "未勾选的删除项保留"
        );
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
    fn external_root_is_written_to_the_shared_directory() {
        let temp = TempDir::new("apply-external");
        let data = app_data(&temp);
        let (agent, sync_root, target_home) = external_root_fixture(&temp);

        let expected = fingerprint_for(&agent, &target_home, &sync_root);
        let report = apply(
            &context_with_home(&agent, &target_home, &sync_root, &data),
            &expected,
            &[],
        )
        .expect("应用应成功");

        // 外部根条目落到 `<home>/.agents/skills`，不往 agent 根写。
        assert_eq!(report.written, 1, "只修改了外部根里的那个技能");
        assert_eq!(report.deleted, 0, "未勾选的删除候选保留");
        assert_eq!(report.skipped, 1);
        assert_eq!(
            fs::read_to_string(target_home.join(".agents/skills/shared/SKILL.md"))
                .expect("读外部技能"),
            "# 共享技能\n",
            "外部根按快照落地"
        );
        assert!(target_home
            .join(".agents/skills/only-here/SKILL.md")
            .is_file());
        assert!(!agent.join("shared/SKILL.md").exists(), "不得写到 agent 根");

        // 备份按虚拟路径存，条目标明所属根。
        assert_eq!(
            fs::read_to_string(
                report
                    .backup_dir
                    .join("files/external/agents-skills/shared/SKILL.md")
            )
            .expect("读备份内容"),
            "# 共享技能（本机改过）\n"
        );
        let restore = read_restore(&report.backup_dir);
        assert_eq!(restore.version, RESTORE_VERSION);
        let backed_up = restore
            .entries
            .iter()
            .find(|entry| entry.root == RootId::AgentsSkills && entry.path == "shared/SKILL.md")
            .unwrap_or_else(|| panic!("备份缺少外部根条目：{:?}", restore.entries));
        assert!(backed_up.existed);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn external_root_deletes_land_only_when_confirmed_with_that_root() {
        let temp = TempDir::new("apply-external-delete");
        let data = app_data(&temp);
        let (agent, sync_root, target_home) = external_root_fixture(&temp);
        let expected = fingerprint_for(&agent, &target_home, &sync_root);

        // 计划外的路径照样整次拒绝。
        let failure = apply(
            &context_with_home(&agent, &target_home, &sync_root, &data),
            &expected,
            &[delete(RootId::AgentsSkills, "only-here/SKILL.md.bak")],
        )
        .expect_err("不在计划里的删除项必须整次拒绝");
        assert_eq!(failure.code, "delete_not_in_plan", "{}", failure.message);
        assert!(
            failure.message.contains("~/.agents/skills"),
            "{}",
            failure.message
        );
        assert!(backup_ids(&data).is_empty());

        // 同一个相对路径存在于 agent 根、不在外部根时，拿外部根的请求也不得删 agent 根的文件。
        write_file(&agent.join("only-here.md"), "共享路径\n");
        write_file(&agent.join("sessions/old.json"), "{}");

        let external_before = tree(&target_home.join(".agents/skills/only-here"));
        let report = apply(
            &context_with_home(&agent, &target_home, &sync_root, &data),
            &expected,
            &[delete(RootId::AgentsSkills, "only-here/SKILL.md")],
        )
        .expect("逐项勾选的外部根删除应成功");

        assert_eq!(report.deleted, 1);
        assert_ne!(
            tree(&target_home.join(".agents/skills/only-here")),
            external_before
        );
        assert!(
            !target_home
                .join(".agents/skills/only-here/SKILL.md")
                .exists(),
            "勾选后外部根的文件被删"
        );
        // 删除进备份：回滚能把它拿回来。
        assert_eq!(
            fs::read_to_string(
                report
                    .backup_dir
                    .join("files/external/agents-skills/only-here/SKILL.md")
            )
            .expect("读备份内容"),
            "# 只在目标机\n"
        );
        let warnings = report.warnings.join("\n");
        assert!(
            warnings.contains("共享目录") && warnings.contains("删除 1 项"),
            "外部根删除后要有汇总告警：{warnings}"
        );
    }

    #[test]
    fn delete_request_with_the_other_root_does_not_cross_roots() {
        let temp = TempDir::new("apply-cross-root-delete");
        let data = app_data(&temp);
        // 两个根里都有同名相对路径 `only-here/SKILL.md`，但只有 agent 根里那一条是删除候选。
        let agent = temp.path().join("agent");
        let source_home = temp.path().join("source-home");
        let sync_root = temp.path().join("cloud");
        write_file(&agent.join("settings.json"), "{}");
        write_file(
            &source_home.join(".agents/skills/shared/SKILL.md"),
            "# 共享技能\n",
        );
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        crate::snapshot::export_fixture_with(
            &agent,
            Some(&source_home),
            &sync_root,
            crate::snapshot::SourceVersions::default(),
        )
        .expect("导出夹具快照");

        let target_home = temp.path().join("target-home");
        write_file(
            &target_home.join(".agents/skills/shared/SKILL.md"),
            "# 共享技能\n",
        );
        write_file(&agent.join("skills/only-here/SKILL.md"), "# 只在宿主目录\n");
        let expected = fingerprint_for(&agent, &target_home, &sync_root);

        // 用外部根去删 agent 根的候选：整次拒绝，一个字节都不动。
        let failure = apply(
            &context_with_home(&agent, &target_home, &sync_root, &data),
            &expected,
            &[delete(RootId::AgentsSkills, "skills/only-here/SKILL.md")],
        )
        .expect_err("根对不上就不是本次计划的删除候选");
        assert_eq!(failure.code, "delete_not_in_plan");
        assert!(agent.join("skills/only-here/SKILL.md").is_file());
        assert!(backup_ids(&data).is_empty());

        let report = apply(
            &context_with_home(&agent, &target_home, &sync_root, &data),
            &expected,
            &[delete(RootId::Agent, "skills/only-here/SKILL.md")],
        )
        .expect("按 agent 根勾选后应成功");

        assert_eq!(report.deleted, 1);
        assert!(!agent.join("skills/only-here/SKILL.md").exists());
        assert!(
            target_home.join(".agents/skills/shared/SKILL.md").is_file(),
            "外部根不得受影响"
        );
    }

    #[test]
    fn stale_fingerprint_is_rejected_before_writing() {
        let temp = TempDir::new("stale");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);

        write_file(&agent.join("settings.json"), "{\"theme\": \"light\"}");
        let before = tree(&agent);

        let failure = apply(&context(&agent, &sync_root, &data), "000000000000", &[])
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
        let failure = apply(&context(&agent, &sync_root, &data), &expected, &[])
            .expect_err("manifest 缺失必须拒绝");
        assert_eq!(failure.code, "manifest_invalid", "{}", failure.message);

        // 云盘只传了一半：manifest 还在，但文件内容不符。
        crate::snapshot::export_fixture(&agent, &sync_root).expect("重新导出快照");
        write_file(
            &sync_root.join("snapshot/settings.json"),
            "{\"theme\": \"half\"}",
        );
        let failure = apply(&context(&agent, &sync_root, &data), &expected, &[])
            .expect_err("哈希不符必须拒绝");
        assert_eq!(failure.code, "manifest_invalid", "{}", failure.message);
        assert!(
            failure.message.contains("hash_mismatch"),
            "{}",
            failure.message
        );
        assert!(backup_ids(&data).is_empty());
    }

    #[test]
    fn delete_paths_outside_the_plan_are_rejected() {
        let temp = TempDir::new("deletes");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);
        let expected = fingerprint(&agent, &sync_root);
        let before = tree(&agent);

        for path in [
            "../escape.json",
            "auth.json",
            "settings.json",
            "/etc/passwd",
        ] {
            let failure = apply(
                &context(&agent, &sync_root, &data),
                &expected,
                &[delete(RootId::Agent, path)],
            )
            .expect_err("不在计划里的删除项必须整次拒绝");
            assert_eq!(failure.code, "delete_not_in_plan", "{path}");
        }

        assert_eq!(tree(&agent), before, "拒绝时不得写任何文件");
        assert!(backup_ids(&data).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn undeclared_link_is_backed_up_and_deleted_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("undeclared-link");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        let data = app_data(&temp);
        write_file(&agent.join("settings.json"), "{\"theme\": \"dark\"}");
        write_file(&temp.path().join("outside/keep.txt"), "留在这里\n");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        crate::snapshot::export_fixture(&agent, &sync_root).expect("导出快照");
        // 导出之后才出现的链接：快照里没有它，只能删链接本身。
        fs::create_dir_all(agent.join("skills")).expect("创建 skills");
        symlink(temp.path().join("outside"), agent.join("skills/extra")).expect("创建目录链接");

        let expected = fingerprint(&agent, &sync_root);
        let report = apply(
            &context(&agent, &sync_root, &data),
            &expected,
            &[delete(RootId::Agent, "skills/extra")],
        )
        .expect("应用应成功");

        assert_eq!(report.deleted, 1);
        assert!(
            fs::symlink_metadata(agent.join("skills/extra")).is_err(),
            "链接本身被删掉"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("outside/keep.txt")).expect("读目标"),
            "留在这里\n",
            "链接目标不受影响"
        );

        let restore = crate::rollback::restore(&agent, None, &data, &report.backup_id, true)
            .expect("回滚应成功");
        assert!(restore.warnings.is_empty(), "{:?}", restore.warnings);
        assert!(
            fs::symlink_metadata(agent.join("skills/extra"))
                .expect("链接回来了")
                .file_type()
                .is_symlink(),
            "回滚按记录重建链接"
        );
        assert_eq!(
            fs::read_link(agent.join("skills/extra")).expect("读链接"),
            temp.path().join("outside"),
            "链接目标不变"
        );
    }

    #[cfg(unix)]
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
        symlink(
            "../../../outside/real.md",
            agent.join("skills/antd/SKILL.md"),
        )
        .expect("创建符号链接");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        crate::snapshot::export_fixture(&agent, &sync_root).expect("导出快照");

        // 链接目标内容变了：计划里 SKILL.md 变成"修改"；settings.json 也改一下，
        // 让备份同时覆盖"修改文件 + 修改链接"两种情况。
        write_file(&temp.path().join("outside/real.md"), "外部被改\n");
        write_file(&agent.join("settings.json"), "{\"theme\": \"light\"}");
        set_mode(&agent.join("settings.json"), 0o640);
        let expected = fingerprint(&agent, &sync_root);

        let report =
            apply(&context(&agent, &sync_root, &data), &expected, &[]).expect("应用应成功");

        // 链接按 manifest 记录重建，外部目标保持原样。
        let metadata = fs::symlink_metadata(agent.join("skills/antd/SKILL.md")).expect("读信息");
        assert!(
            metadata.file_type().is_symlink(),
            "链接按 manifest 记录重建"
        );
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
        assert_eq!(
            mode_of(&agent.join("settings.json")),
            0o640,
            "覆盖后保留原 mode"
        );

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
        assert_eq!(
            link.link_target.as_deref(),
            Some("../../../outside/real.md")
        );
        assert!(
            !report
                .backup_dir
                .join("files/skills/antd/SKILL.md")
                .exists(),
            "符号链接只记目标，不复制内容"
        );
    }

    #[cfg(unix)]
    #[test]
    fn auth_file_is_written_with_0600() {
        let temp = TempDir::new("auth-mode");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);

        fs::remove_file(agent.join("auth.json")).expect("删除 live auth");
        let expected = fingerprint(&agent, &sync_root);
        apply(&context(&agent, &sync_root, &data), &expected, &[]).expect("应用应成功");
        assert_eq!(mode_of(&agent.join("auth.json")), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_agent_root_is_written_through_with_a_warning() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("root-link");
        let real = temp.path().join("real-agent");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        let data = app_data(&temp);
        write_file(&real.join("settings.json"), "{\"theme\": \"dark\"}");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        crate::snapshot::export_fixture(&real, &sync_root).expect("导出夹具快照");
        symlink(&real, &agent).expect("建根链接");

        write_file(&real.join("settings.json"), "{\"theme\": \"light\"}");
        let expected = fingerprint(&agent, &sync_root);
        let report =
            apply(&context(&agent, &sync_root, &data), &expected, &[]).expect("应用应成功");

        assert_eq!(report.written, 1);
        // 根的类型不进快照：写入落到链接目标，根链接本身原样保留。
        assert!(platform::is_link_like(&agent));
        assert_eq!(
            fs::read_to_string(real.join("settings.json")).expect("读链接目标"),
            "{\"theme\": \"dark\"}"
        );
        let warnings = report.warnings.join("\n");
        assert!(warnings.contains("符号链接"), "{warnings}");
    }

    #[test]
    fn added_file_is_backed_up_as_not_existed() {
        let temp = TempDir::new("added");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);

        fs::remove_file(agent.join("AGENTS.md")).expect("删除 live 文件");
        let expected = fingerprint(&agent, &sync_root);

        let report =
            apply(&context(&agent, &sync_root, &data), &expected, &[]).expect("应用应成功");
        assert_eq!(report.written, 1);

        let restore = read_restore(&report.backup_dir);
        let added = entry(&restore, "AGENTS.md");
        assert_eq!(added.kind, RestoreKind::File);
        assert!(!added.existed, "新增文件没有原内容可备份");
        assert_eq!(added.mode, None);
        assert!(!report.backup_dir.join("files/AGENTS.md").exists());
        #[cfg(unix)]
        assert_eq!(mode_of(&agent.join("AGENTS.md")), NEW_FILE_MODE);
    }

    #[cfg(unix)]
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
        let failure = apply(&context(&agent, &sync_root, &data), &expected, &[])
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
        assert_eq!(
            mode_of(&backup_dir.join("files/settings.json")),
            settings.mode.unwrap()
        );
    }

    #[test]
    fn backups_keep_only_the_configured_newest_entries() {
        let temp = TempDir::new("retention");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);
        let expected = fingerprint(&agent, &sync_root);

        let mut created = Vec::new();
        for _ in 0..5 {
            let report = apply(
                &apply_context(&agent, &sync_root, &data, &PLATFORM_LINK_CREATOR, 3),
                &expected,
                &[],
            )
            .expect("应用应成功");
            created.push(report.backup_id);
        }

        // 备份目录名是时间戳（同秒时再加序号），字典序即时间序：倒序就是最新在前。
        created.sort();
        created.reverse();
        let mut ids = backup_ids(&data);
        ids.reverse();
        assert_eq!(ids, created[..3], "保留配置里的 3 份，且是最新的 3 份");
        assert!(!ids.contains(&created[3]), "第 4 新的已被清理");
        assert!(!ids.contains(&created[4]), "最旧的已被清理");
    }

    #[test]
    fn retention_of_one_keeps_only_the_newest_backup() {
        let temp = TempDir::new("retention-one");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);
        let expected = fingerprint(&agent, &sync_root);

        let mut newest = String::new();
        for _ in 0..3 {
            newest = apply(
                &apply_context(&agent, &sync_root, &data, &PLATFORM_LINK_CREATOR, 1),
                &expected,
                &[],
            )
            .expect("应用应成功")
            .backup_id;
        }

        assert_eq!(backup_ids(&data), vec![newest], "保留数 1 只留最新一份");
    }

    /// 宿主目录里的 `skills` 是指向外部目录的链接；快照由该链接导出。
    #[cfg(unix)]
    fn symlinked_skills_fixture(temp: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        let external = temp.path().join("outside/skills-live");

        write_file(&external.join("keep.md"), "v1\n");
        fs::create_dir_all(&agent).expect("创建宿主目录");
        symlink("../outside/skills-live", agent.join("skills")).expect("创建目录链接");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        crate::snapshot::export_fixture(&agent, &sync_root).expect("导出夹具快照");
        (agent, sync_root, external)
    }

    #[test]
    fn plain_directories_are_not_symlink_ancestors() {
        let temp = TempDir::new("plain-ancestors");
        let agent = temp.path().join("agent");
        write_file(&agent.join("skills/antd/SKILL.md"), "# 技能\n");

        assert!(symlink_ancestors(&agent, "skills/antd/SKILL.md").is_empty());
        assert!(!platform::is_link_like(&agent.join("skills")));
        assert!(!platform::is_link_like(&agent.join("skills/antd/SKILL.md")));
    }

    #[cfg(unix)]
    #[test]
    fn linked_ancestors_are_reported() {
        let temp = TempDir::new("linked-ancestors");
        let agent = temp.path().join("agent");
        write_file(&temp.path().join("outside/keep.md"), "v1\n");
        fs::create_dir_all(&agent).expect("创建宿主目录");
        symlink("../outside", agent.join("skills")).expect("创建目录链接");
        write_file(&agent.join("skills/keep.md"), "v1\n");

        assert_eq!(
            symlink_ancestors(&agent, "skills/keep.md"),
            vec!["skills".to_string()]
        );
        // 链接之下的普通文件本身不是链接。
        assert!(!platform::is_link_like(&agent.join("skills/keep.md")));
    }

    #[cfg(unix)]
    #[test]
    fn rebuilds_manifest_links_and_skips_writes_under_them() {
        let temp = TempDir::new("rebuild-link");
        let (agent, sync_root, external) = symlinked_skills_fixture(&temp);
        let data = app_data(&temp);

        // 目标内容变了：计划里 skills/keep.md 是“修改”，物化就会在宿主目录里写出真实文件。
        write_file(&external.join("keep.md"), "v2\n");
        let expected = fingerprint(&agent, &sync_root);
        let external_before = tree(&external);

        let report =
            apply(&context(&agent, &sync_root, &data), &expected, &[]).expect("应用应成功");

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

        // 重建前先记下原有链接：回滚按 restore.json 还原。
        let restore = read_restore(&report.backup_dir);
        let skills = entry(&restore, "skills");
        assert_eq!(skills.kind, RestoreKind::Symlink);
        assert!(skills.existed);
        assert_eq!(
            skills.link_target.as_deref(),
            Some("../outside/skills-live")
        );
    }

    #[cfg(unix)]
    #[test]
    fn broken_link_target_falls_back_to_materializing() {
        let temp = TempDir::new("broken-link");
        let (agent, sync_root, external) = symlinked_skills_fixture(&temp);
        let data = app_data(&temp);

        // 目标整个消失：链接变成断链，只能把快照内容物化成真实文件。
        fs::remove_dir_all(&external).expect("删除链接目标");
        let expected = fingerprint(&agent, &sync_root);

        let report =
            apply(&context(&agent, &sync_root, &data), &expected, &[]).expect("应用应成功");

        assert_eq!(report.linked, 0, "目标不存在就不重建");
        assert_eq!(report.written, 1);
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(
            report.warnings[0].contains("skills"),
            "{:?}",
            report.warnings
        );

        let metadata = fs::symlink_metadata(agent.join("skills")).expect("读 skills 信息");
        assert!(!metadata.file_type().is_symlink(), "断链退回物化");
        assert!(metadata.is_dir());
        assert_eq!(
            fs::read_to_string(agent.join("skills/keep.md")).expect("读落地文件"),
            "v1\n"
        );
        assert!(!external.exists(), "不替用户重建目标目录");

        // 断链本身也在备份里：条目排在文件之前，回滚先恢复链接。
        let restore = read_restore(&report.backup_dir);
        let skills = entry(&restore, "skills");
        assert_eq!(skills.kind, RestoreKind::Symlink);
        assert!(skills.existed);
        assert_eq!(skills.mode, None);
        assert_eq!(
            skills.link_target.as_deref(),
            Some("../outside/skills-live")
        );
        assert_eq!(
            restore.entries.first().map(|entry| entry.path.as_str()),
            Some("skills"),
            "链接条目排在文件条目之前"
        );
    }

    /// 平台拒绝建链接：Windows 默认权限下 `ERROR_PRIVILEGE_NOT_HELD` 的替身。
    #[cfg(unix)]
    struct RefusingLinks;

    #[cfg(unix)]
    impl LinkCreator for RefusingLinks {
        fn create_link(&self, _target: &str, _path: &Path, _is_dir: bool) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "建链接被拒绝（os error 1314）",
            ))
        }
    }

    /// 宿主目录里的 `skills` 是指向外部目录的链接，目标下有两个文件；快照由该链接导出。
    #[cfg(unix)]
    fn nested_symlinked_skills_fixture(temp: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        let external = temp.path().join("outside/skills-live");

        write_file(&external.join("keep.md"), "v1\n");
        write_file(&external.join("antd/SKILL.md"), "# 技能\n");
        fs::create_dir_all(&agent).expect("创建宿主目录");
        symlink("../outside/skills-live", agent.join("skills")).expect("创建目录链接");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        crate::snapshot::export_fixture(&agent, &sync_root).expect("导出夹具快照");
        (agent, sync_root, external)
    }

    /// 链接下的内容与快照完全一致：计划里没有任何条目，建链接失败后仍要把整棵子树写回。
    #[cfg(unix)]
    #[test]
    fn failed_link_rebuild_materializes_consistent_subtree() {
        let temp = TempDir::new("refused-link-consistent");
        let (agent, sync_root, external) = nested_symlinked_skills_fixture(&temp);
        let data = app_data(&temp);

        let expected = fingerprint(&agent, &sync_root);
        let external_before = tree(&external);
        let report = apply(
            &apply_context(
                &agent,
                &sync_root,
                &data,
                &RefusingLinks,
                config::DEFAULT_BACKUP_RETENTION as usize,
            ),
            &expected,
            &[],
        )
        .expect("退回物化后应用应成功");

        assert_eq!(report.linked, 0, "一条链接也没建成");
        assert_eq!(report.deleted, 0);
        assert_eq!(report.skipped_deletes, 0);
        assert_eq!(report.written, 2, "整棵子树的 2 个文件都写回");
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(
            report.warnings[0].contains("skills"),
            "{:?}",
            report.warnings
        );
        assert!(
            report.warnings[0].contains("1314"),
            "告警要带 OS 错误原文：{:?}",
            report.warnings
        );
        assert!(
            report.warnings[0].contains("整棵"),
            "告警要说明写出了整棵内容：{:?}",
            report.warnings
        );

        let metadata = fs::symlink_metadata(agent.join("skills")).expect("读 skills 信息");
        assert!(!metadata.file_type().is_symlink(), "退回物化");
        assert!(metadata.is_dir());
        // 计划里一条都没有，这两个文件仍然必须存在且内容正确。
        assert_eq!(
            fs::read_to_string(agent.join("skills/keep.md")).expect("读 keep.md"),
            "v1\n"
        );
        assert_eq!(
            fs::read_to_string(agent.join("skills/antd/SKILL.md")).expect("读 SKILL.md"),
            "# 技能\n"
        );
        assert_eq!(tree(&external), external_before, "链接目标不得被改");
    }

    /// 链接下的内容只有一部分改动：计划只列出改动项，退回物化仍要写出整棵子树。
    #[cfg(unix)]
    #[test]
    fn failed_link_rebuild_materializes_partially_modified_subtree() {
        let temp = TempDir::new("refused-link-modified");
        let (agent, sync_root, external) = nested_symlinked_skills_fixture(&temp);
        let data = app_data(&temp);

        // 未变的那棵子树不改进；改动的那个文件在计划里只是“修改”。
        write_file(&external.join("keep.md"), "v2\n");
        let expected = fingerprint(&agent, &sync_root);

        let report = apply(
            &apply_context(
                &agent,
                &sync_root,
                &data,
                &RefusingLinks,
                config::DEFAULT_BACKUP_RETENTION as usize,
            ),
            &expected,
            &[],
        )
        .expect("退回物化后应用应成功");

        assert_eq!(report.linked, 0);
        assert_eq!(report.written, 2, "修改项的邻居也必须写回");
        assert_eq!(report.deleted, 0);
        assert_eq!(
            fs::read_to_string(agent.join("skills/keep.md")).expect("读 keep.md"),
            "v1\n",
            "按快照写回"
        );
        assert_eq!(
            fs::read_to_string(agent.join("skills/antd/SKILL.md")).expect("读 SKILL.md"),
            "# 技能\n"
        );
        assert_eq!(
            fs::read_to_string(external.join("keep.md")).expect("读链接目标"),
            "v2\n",
            "链接目标本身不写，也不删"
        );
    }

    /// 已勾选的删除落在建不成的链接下：跳过删除本身并说明它已随目录替换消失。
    #[cfg(unix)]
    #[test]
    fn deletes_under_materialized_link_are_skipped_with_warning() {
        let temp = TempDir::new("skip-delete-under-materialized");
        let (agent, sync_root, external) = symlinked_skills_fixture(&temp);
        let data = app_data(&temp);

        write_file(&external.join("extra.md"), "不要删\n");
        let expected = fingerprint(&agent, &sync_root);

        let report = apply(
            &apply_context(
                &agent,
                &sync_root,
                &data,
                &RefusingLinks,
                config::DEFAULT_BACKUP_RETENTION as usize,
            ),
            &expected,
            &[delete(RootId::Agent, "skills/extra.md")],
        )
        .expect("链接之下的删除跳过，应用仍成功");

        assert_eq!(report.linked, 0);
        assert_eq!(report.written, 1, "快照里的 keep.md 写回");
        assert_eq!(report.deleted, 0, "没执行删除：链接目标里的原文件还在");
        assert_eq!(report.skipped_deletes, 1);
        assert_eq!(report.skipped, 0, "勾选的删除项不算“未勾选保留”");
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("skills/extra.md")
                    && warning.contains("已随目录替换从受管路径上消失")
                    && warning.contains("原内容在本次备份中")),
            "告警要说清楚它已经不在了，而不是“未删除”：{:?}",
            report.warnings
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("这 1 项勾选删除已随目录替换消失")),
            "{:?}",
            report.warnings
        );
        assert!(agent.join("skills/keep.md").is_file());
        assert!(
            !agent.join("skills/extra.md").exists(),
            "物化只写快照内容，不再带着删除候选"
        );
        assert!(external.join("extra.md").is_file(), "链接目标不得被删");
        // 告警说“原内容在本次备份中”，那就得真的在那儿。
        assert_eq!(
            fs::read_to_string(report.backup_dir.join("files/skills/extra.md"))
                .expect("读备份内容"),
            "不要删\n"
        );
    }

    /// 外部根自己是指向别处的链接：根的类型不进快照，写入落到链接目标，根链接原样保留。
    #[cfg(unix)]
    #[test]
    fn symlinked_external_root_is_written_through_with_a_warning() {
        let temp = TempDir::new("external-root-link");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        let source_home = temp.path().join("source-home");
        let data = app_data(&temp);
        let shared = temp.path().join("outside/shared-skills");

        write_file(&agent.join("settings.json"), "{}");
        write_file(&shared.join("shared/SKILL.md"), "# 本机旧内容\n");
        write_file(
            &source_home.join(".agents/skills/shared/SKILL.md"),
            "# 共享技能\n",
        );
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        crate::snapshot::export_fixture_with(
            &agent,
            Some(&source_home),
            &sync_root,
            crate::snapshot::SourceVersions::default(),
        )
        .expect("导出夹具快照");

        let target_home = temp.path().join("target-home");
        fs::create_dir_all(target_home.join(".agents")).expect("创建目标机 .agents");
        symlink(&shared, target_home.join(".agents/skills")).expect("把目标机外部根做成链接");

        let expected = fingerprint_for(&agent, &target_home, &sync_root);
        let report = apply(
            &context_with_home(&agent, &target_home, &sync_root, &data),
            &expected,
            &[],
        )
        .expect("应用应成功");

        assert_eq!(report.written, 1, "只修改了外部根里的那个技能");
        assert_eq!(report.deleted, 0);
        // 根的类型不进快照：写入落到链接目标，根链接本身原样保留。
        let root_dir = target_home.join(".agents/skills");
        assert!(
            platform::is_link_like(&root_dir),
            "根链接不能被换成真实目录"
        );
        assert_eq!(fs::read_link(&root_dir).expect("读根链接目标"), shared);
        assert_eq!(
            fs::read_to_string(shared.join("shared/SKILL.md")).expect("读链接目标里的技能"),
            "# 共享技能\n"
        );
        // 只有外部根是链接，告警必须指着它，而不是扚主目录。
        let warnings = report.warnings.join("\n");
        assert!(
            warnings.contains("~/.agents/skills") && warnings.contains("符号链接"),
            "{warnings}"
        );
    }

    /// 目标机的外部根：`.agents/skills` 是真目录，里面的技能是指向外部目录的链接。
    /// 快照由源机的同名链接导出，manifest 因此同时记下文件与链接。
    #[cfg(unix)]
    fn external_root_link_fixture(temp: &TempDir) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        let source_home = temp.path().join("source-home");
        let shared = temp.path().join("outside/shared-live");

        write_file(&agent.join("settings.json"), "{}");
        write_file(&shared.join("SKILL.md"), "# 共享技能\n");
        write_file(&shared.join("notes.md"), "说明\n");
        fs::create_dir_all(source_home.join(".agents/skills")).expect("创建源机外部根");
        // 绝对目标：源机与目标机是同一个临时目录里的两处布局，按同一条路径重建。
        symlink(&shared, source_home.join(".agents/skills/shared-link")).expect("建源机链接");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        crate::snapshot::export_fixture_with(
            &agent,
            Some(&source_home),
            &sync_root,
            crate::snapshot::SourceVersions::default(),
        )
        .expect("导出夹具快照");

        let target_home = temp.path().join("target-home");
        fs::create_dir_all(target_home.join(".agents/skills")).expect("创建目标机外部根");
        symlink(&shared, target_home.join(".agents/skills/shared-link")).expect("建目标机链接");
        (agent, sync_root, target_home, shared)
    }

    /// 外部根里的条目是链接：manifest 记过就按记录重建，链接之下的写入与删除都只能跳过。
    #[cfg(unix)]
    #[test]
    fn external_root_entry_link_is_rebuilt_and_hides_its_contents() {
        let temp = TempDir::new("external-entry-link");
        let data = app_data(&temp);
        let (agent, sync_root, target_home, shared) = external_root_link_fixture(&temp);
        let root_dir = target_home.join(".agents/skills");

        // 链接目标在目标机上被改过，还多了一个快照里没有的文件。
        write_file(&shared.join("SKILL.md"), "# 共享技能（本机改过）\n");
        write_file(&shared.join("extra.md"), "只在目标机\n");
        let expected = fingerprint_for(&agent, &target_home, &sync_root);

        let report = apply(
            &context_with_home(&agent, &target_home, &sync_root, &data),
            &expected,
            &[delete(RootId::AgentsSkills, "shared-link/extra.md")],
        )
        .expect("应用应成功");

        assert_eq!(report.linked, 1, "按 manifest 记录重建外部根的链接");
        assert_eq!(report.written, 0, "链接之下的写入交给链接目标");
        assert_eq!(report.deleted, 0);
        assert_eq!(report.skipped_deletes, 1, "链接之下的删除只跳过并说明");
        assert_eq!(report.skipped, 0);
        assert_eq!(
            fs::read_link(root_dir.join("shared-link")).expect("读链接目标"),
            shared
        );
        assert_eq!(
            fs::read_to_string(shared.join("SKILL.md")).expect("读链接目标"),
            "# 共享技能（本机改过）\n"
        );
        assert!(shared.join("extra.md").is_file(), "链接目标不得被删");
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("由链接 shared-link 的目标提供")),
            "{:?}",
            report.warnings
        );

        // 备份按外部根的虚拟路径记链接本身与待删条目，回滚才能反做。
        let restore = read_restore(&report.backup_dir);
        let link = restore
            .entries
            .iter()
            .find(|entry| entry.root == RootId::AgentsSkills && entry.path == "shared-link")
            .unwrap_or_else(|| panic!("备份缺少外部根的链接条目：{:?}", restore.entries));
        assert_eq!(link.kind, RestoreKind::Symlink);
        assert_eq!(
            link.link_target.as_deref(),
            Some(shared.to_string_lossy().as_ref())
        );
    }

    /// 外部根的链接建不成：从快照 `snapshot/external/agents-skills/…` 整棵物化，
    /// 落地内容是快照里的版本，不是链接目标里的本机改动。
    #[cfg(unix)]
    #[test]
    fn external_root_entry_link_falls_back_to_the_external_snapshot_prefix() {
        let temp = TempDir::new("external-entry-link-refused");
        let data = app_data(&temp);
        let (agent, sync_root, target_home, shared) = external_root_link_fixture(&temp);
        let root_dir = target_home.join(".agents/skills");

        write_file(&shared.join("SKILL.md"), "# 共享技能（本机改过）\n");
        write_file(&shared.join("extra.md"), "只在目标机\n");
        let expected = fingerprint_for(&agent, &target_home, &sync_root);

        let report = apply(
            &context_with_home_and_links(&agent, &target_home, &sync_root, &data, &RefusingLinks),
            &expected,
            &[delete(RootId::AgentsSkills, "shared-link/extra.md")],
        )
        .expect("退回物化后应用应成功");

        assert_eq!(report.linked, 0, "一条链接也没建成");
        assert_eq!(report.written, 2, "快照里那棵子树的 2 个文件都写回");
        assert_eq!(report.deleted, 0);
        assert_eq!(report.skipped_deletes, 1);
        let metadata = fs::symlink_metadata(root_dir.join("shared-link")).expect("读条目信息");
        assert!(!metadata.file_type().is_symlink(), "退回物化");
        assert!(metadata.is_dir());
        assert_eq!(
            fs::read_to_string(root_dir.join("shared-link/SKILL.md")).expect("读物化后的技能"),
            "# 共享技能\n",
            "内容来自 snapshot/external/agents-skills/，不是链接目标"
        );
        assert_eq!(
            fs::read_to_string(root_dir.join("shared-link/notes.md")).expect("读物化后的文件"),
            "说明\n"
        );
        assert!(
            !root_dir.join("shared-link/extra.md").exists(),
            "链接目标里多出来的内容不跟着物化"
        );
        // 链接目标本身不动，勾选的删除项还在那儿。
        assert_eq!(
            fs::read_to_string(shared.join("SKILL.md")).expect("读链接目标"),
            "# 共享技能（本机改过）\n"
        );
        assert!(shared.join("extra.md").is_file(), "链接目标不得被删");
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("shared-link/extra.md")
                    && warning.contains("已随目录替换从受管路径上消失")),
            "{:?}",
            report.warnings
        );
        // 它在受管路径上没了，原内容在两处：链接目标，以及本次备份。
        assert_eq!(
            fs::read_to_string(
                report
                    .backup_dir
                    .join("files/external/agents-skills/shared-link/extra.md")
            )
            .expect("读备份内容"),
            "只在目标机\n"
        );
    }

    /// 建链接失败、物化也失败：整次应用失败，两条原因都带上，备份可回滚。
    #[cfg(unix)]
    #[test]
    fn failed_materialization_stops_with_apply_failed() {
        let temp = TempDir::new("refused-link-materialize-failed");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        let data = app_data(&temp);

        write_file(&temp.path().join("outside/shared-live/note.md"), "方式\n");
        fs::create_dir_all(agent.join("agents")).expect("创建 agents 目录");
        symlink(
            "../../outside/shared-live",
            agent.join("agents/link-to-shared"),
        )
        .expect("创建目录链接");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        crate::snapshot::export_fixture(&agent, &sync_root).expect("导出夹具快照");

        // 链接没了，父目录又不可写：建链接失败之后连物化都落不下去。
        fs::remove_file(agent.join("agents/link-to-shared")).expect("删除夹具链接");
        set_mode(&agent.join("agents"), 0o500);
        let expected = fingerprint(&agent, &sync_root);

        let failure = apply(
            &apply_context(
                &agent,
                &sync_root,
                &data,
                &RefusingLinks,
                config::DEFAULT_BACKUP_RETENTION as usize,
            ),
            &expected,
            &[],
        )
        .expect_err("物化失败必须停在原地");
        set_mode(&agent.join("agents"), 0o755);

        assert_eq!(failure.code, "apply_failed", "{}", failure.message);
        assert!(
            failure
                .message
                .contains("agents/link-to-shared 重建链接失败"),
            "{}",
            failure.message
        );
        assert!(
            failure.message.contains("1314"),
            "要带上建链接失败的原因：{}",
            failure.message
        );
        assert!(
            failure.message.contains("改成写入快照内容也失败"),
            "要带上物化失败的原因：{}",
            failure.message
        );
        assert_eq!(failure.completed, 0);

        // 失败前已经做完的备份留着：链接与写入都能回滚。
        let backup_dir = failure.backup_dir.clone().expect("失败要附上备份目录");
        assert!(backup_dir.is_dir());
        let restore = read_restore(&backup_dir);
        assert_eq!(restore.fingerprint, expected);
        assert!(!entry(&restore, "agents/link-to-shared/note.md").existed);
        assert!(
            !agent.join("agents/link-to-shared").exists(),
            "既没建成链接，也没留下写了一半的内容"
        );
        assert!(
            temp.path().join("outside/shared-live/note.md").is_file(),
            "链接目标不得被改"
        );
    }

    #[test]
    fn tampered_manifest_link_outside_scope_is_not_rebuilt() {
        let temp = TempDir::new("link-out-of-scope");
        let (agent, sync_root) = fixture(&temp);
        let data = app_data(&temp);

        // 被改过的 manifest 想借链接接管 auth.json：凭据文件不按链接重建。
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
        let report =
            apply(&context(&agent, &sync_root, &data), &expected, &[]).expect("应用应成功");

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
            "auth.json 不得被链接接管"
        );
    }

    #[cfg(unix)]
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
            &context(&agent, &sync_root, &data),
            &expected,
            &[delete(RootId::Agent, "skills/extra.md")],
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

    #[cfg(unix)]
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
            &context(&agent, &sync_root, &data),
            &expected,
            &[delete(RootId::Agent, "skills/extra.md")],
        )
        .expect_err("经符号链接删除必须整次拒绝");

        assert_eq!(
            failure.code, "delete_through_symlink",
            "{}",
            failure.message
        );
        assert!(failure.message.contains("skills"), "{}", failure.message);
        assert_eq!(failure.completed, 0);
        assert_eq!(failure.backup_dir, None);
        assert!(
            outside.join("extra.md").is_file(),
            "链接目标里的文件不得被删"
        );
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
            &context(&agent, &sync_root, &data),
            &expected,
            &[delete(RootId::Agent, "prompts/live-only.md")],
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

//! 快照校验与导入计划（只读）。
//!
//! 规则来源：`docs/design/10-snapshot-sync.md` 第 4、6 节。
//! 只读：读 manifest、读快照文件、读 live 文件，不写任何文件；落地由 apply 负责。
//! 核心 [`plan_snapshot`] 不依赖 Tauri，子进程通过 [`CommandRunner`] 注入，可直接临时目录单测。
//!
//! 失败停在原地：manifest 缺失/版本不支持/损坏、任一文件哈希不符都直接返回失败，不给半套计划。

use crate::config;
use crate::doctor::{self, CommandOutcome, CommandRunner, ShellRunner, Version, PROBE_TIMEOUT};
use crate::platform::{HostOs, ProgramPath};
use crate::runtime;
use crate::scope::{self, EntryKind, ManagedRoot, ManagedRoots, RootId};
use crate::snapshot::{
    self, split_manifest_path, Manifest, ManifestFile, Requirements, SourceInfo, FORMAT_VERSION,
    MANIFEST_FILE_NAME,
};
use crate::webdav_transfer;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

/// 哈希不符明细的列出上限；超出部分只计数。
pub const MAX_LISTED_MISMATCHES: usize = 20;
/// 版本探测脚本：`node -p process.versions.node` 与 `pi --version`，失败留空不阻断。
const VERSION_PROBE_SCRIPT: &str = r#"echo "node:$(node -p 'process.versions.node' 2>/dev/null)"
echo "pi:$(pi --version 2>/dev/null | head -n 1)""#;

/// 计划里一项属于哪一类差异。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanCategory {
    /// 快照有、live 无：应用时新增。
    Added,
    /// 两边都有但哈希不同：应用时覆盖。
    Modified,
    /// live 有、快照无：应用时删除（逐项确认）。
    Deleted,
}

/// 计划项在磁盘上的形态。删除链接只删链接本身，不碰目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanKind {
    File,
    Link,
}

/// 计划项。`path` 相对所属受管根：两个根可能包含相同的相对路径，靠 `root` 区分。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanItem {
    /// 所属受管根。
    pub root: RootId,
    /// 相对所属受管根的路径，`/` 分隔。
    pub path: String,
    pub category: PlanCategory,
    pub kind: PlanKind,
    /// 快照侧字节数；`deleted` 时是 live 侧字节数，链接记 0。
    pub size: u64,
    pub snapshot_sha256: Option<String>,
    pub live_sha256: Option<String>,
}

/// 三组计划项，组内按路径升序。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub added: Vec<PlanItem>,
    pub modified: Vec<PlanItem>,
    pub deleted: Vec<PlanItem>,
}

/// 计划计数。`unchanged` 只计数，不出现在 [`Plan`] 里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanTotals {
    pub snapshot_files: usize,
    pub live_files: usize,
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub unchanged: usize,
}

/// 两份 manifest 的文件级差异。三组路径都是相对宿主目录的形式，组内升序。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestDiff {
    /// 右边有、左边没有。
    pub added: Vec<String>,
    /// 两边都有但哈希不同。
    pub modified: Vec<String>,
    /// 左边有、右边没有。
    pub removed: Vec<String>,
}

/// manifest 摘要，供 UI 显示"是不是同一份快照"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestInfo {
    pub profile_id: String,
    pub created_at: String,
    pub fingerprint: String,
    /// manifest.files 的条数（与 `totals.snapshotFiles` 同值，方便单看摘要）。
    pub files: usize,
    pub source: SourceInfo,
    pub requirements: Requirements,
}

/// 逐文件校验结果。走到这里一定是全部通过，不匹配会以 [`PlanFailure`] 返回。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verification {
    pub checked: usize,
}

/// 一条宿主版本要求。`satisfied` 为 `None` 表示探测不到，不阻断。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequirementCheck {
    pub tool: &'static str,
    pub required: String,
    pub found: Option<String>,
    pub satisfied: Option<bool>,
}

/// 一次检查的完整输出。只读，可直接序列化给 UI。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanReport {
    pub manifest_info: ManifestInfo,
    pub verification: Verification,
    pub plan: Plan,
    pub totals: PlanTotals,
    pub requirements: Vec<RequirementCheck>,
}

/// 单个文件与 manifest 不一致的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MismatchReason {
    /// 快照里没有这个文件。
    Missing,
    /// 读不出来（权限等）。
    Unreadable,
    /// 字节数不同。
    Size,
    /// 字节数相同但哈希不同。
    Hash,
    /// manifest 里的路径越出同步根目录。
    OutOfBounds,
}

/// 一处哈希不符。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HashMismatch {
    /// manifest 里的路径，形如 `snapshot/settings.json`。
    pub path: String,
    pub reason: MismatchReason,
    pub expected_sha256: String,
    pub actual_sha256: Option<String>,
}

/// 检查失败。失败码见 `docs/design/10-snapshot-sync.md` 第 9 节。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanFailure {
    pub code: &'static str,
    pub message: String,
    /// 仅 `hash_mismatch` 会带上，最多 [`MAX_LISTED_MISMATCHES`] 条。
    pub mismatches: Vec<HashMismatch>,
}

impl PlanFailure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            mismatches: Vec::new(),
        }
    }
}

/// core：校验快照并生成计划。`agent_dir` 是 live 宿主目录，`home_dir` 用来解析
/// `~/.agents/skills`（快照声明了它但本机解析不到 home 时报 `home_dir_missing`）；
/// `host` 只影响宿主版本探测的命令形态（生产传 [`HostOs::current`]）。
pub fn plan_snapshot(
    agent_dir: &Path,
    home_dir: Option<&Path>,
    sync_root: &Path,
    runner: &dyn CommandRunner,
    host: HostOs,
) -> Result<PlanReport, PlanFailure> {
    plan_with(agent_dir, home_dir, sync_root, Some((runner, host)))
}

/// 同一套校验与计划，但不探测宿主版本：对比表只关心差异计数与删除路径。
///
/// 一份候选包起两个子进程（各最多 5 秒）的代价换不来表里任何一列，所以对比走这里。
pub fn plan_snapshot_without_probe(
    agent_dir: &Path,
    home_dir: Option<&Path>,
    sync_root: &Path,
) -> Result<PlanReport, PlanFailure> {
    plan_with(agent_dir, home_dir, sync_root, None)
}

fn plan_with(
    agent_dir: &Path,
    home_dir: Option<&Path>,
    sync_root: &Path,
    probe: Option<(&dyn CommandRunner, HostOs)>,
) -> Result<PlanReport, PlanFailure> {
    let manifest = read_manifest(sync_root)?;

    let (mismatches, mismatch_total) = verify_manifest_files(sync_root, &manifest.files);
    if mismatch_total > 0 {
        return Err(PlanFailure {
            code: "hash_mismatch",
            message: format!(
                "快照与 manifest 不一致：{mismatch_total} 个文件不匹配，云盘可能还没传完；本次不生成计划"
            ),
            mismatches,
        });
    }

    let roots = resolve_roots(agent_dir, home_dir, &manifest.managed_roots)?;
    let live = live_scan(&roots, &manifest)?;
    let (plan, totals) = build_plan(&manifest, &live);

    Ok(PlanReport {
        manifest_info: ManifestInfo {
            profile_id: manifest.profile_id.clone(),
            created_at: manifest.created_at.clone(),
            fingerprint: manifest.fingerprint.clone(),
            files: manifest.files.len(),
            source: manifest.source.clone(),
            requirements: manifest.requirements.clone(),
        },
        verification: Verification {
            checked: manifest.files.len(),
        },
        plan,
        totals,
        requirements: match probe {
            Some((runner, host)) => {
                requirement_checks(&manifest.requirements, agent_dir, runner, host)
            }
            None => Vec::new(),
        },
    })
}

/// 逐文件比较两份 manifest：只比 `files`（链接与声明包不进对比表）。
///
/// `BTreeMap` 的键序就是路径升序，不必再排一次。
pub fn diff_manifests(left: &Manifest, right: &Manifest) -> ManifestDiff {
    let (left_files, right_files) = (manifest_hashes(left), manifest_hashes(right));

    let mut diff = ManifestDiff::default();
    for (path, sha256) in &right_files {
        match left_files.get(path) {
            None => diff.added.push(path.clone()),
            Some(before) if before != sha256 => diff.modified.push(path.clone()),
            Some(_) => {}
        }
    }
    for path in left_files.keys() {
        if !right_files.contains_key(path) {
            diff.removed.push(path.clone());
        }
    }
    diff
}

fn manifest_hashes(manifest: &Manifest) -> BTreeMap<String, &str> {
    manifest
        .files
        .iter()
        .filter_map(|file| {
            // 键用虚拟路径：两个根里相同的相对路径必须区分为两项。
            let (root, relative) = split_manifest_path(&file.path)?;
            Some((
                snapshot::virtual_path(root, &relative),
                file.sha256.as_str(),
            ))
        })
        .collect()
}

/// 读 manifest：不存在 → `manifest_missing`；版本不认识 → `manifest_unsupported`；其余 → `manifest_corrupt`。
pub(crate) fn read_manifest(sync_root: &Path) -> Result<Manifest, PlanFailure> {
    let path = sync_root.join(MANIFEST_FILE_NAME);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(PlanFailure::new(
                "manifest_missing",
                "同步文件夹里没有 manifest.json：云盘可能还没传完，或同步文件夹选错了",
            ))
        }
        Err(error) => {
            return Err(PlanFailure::new(
                "manifest_corrupt",
                format!("读取 manifest 失败：{error}"),
            ))
        }
    };

    // 先单独看 formatVersion：结构不认识时也要报"版本不支持"，而不是"文件损坏"。
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        PlanFailure::new(
            "manifest_corrupt",
            format!("manifest 不是合法 JSON：{error}"),
        )
    })?;
    match value
        .get("formatVersion")
        .and_then(serde_json::Value::as_u64)
    {
        Some(version) if version == u64::from(FORMAT_VERSION) => {}
        Some(version) => {
            return Err(PlanFailure::new(
                "manifest_unsupported",
                format!("manifest 的 formatVersion 是 {version}，本应用只支持 {FORMAT_VERSION}"),
            ))
        }
        None => {
            return Err(PlanFailure::new(
                "manifest_corrupt",
                "manifest 缺少 formatVersion 字段",
            ))
        }
    }

    // 受管根 id 不认识就是格式不支持：先于反序列化检查，才能与「结构损坏」分开。
    match value.get("managedRoots") {
        // 缺字段：结构上能解析（`serde(default)`），但 agent 根必须在，后面按损坏报。
        None => {}
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                let Some(id) = item.as_str() else {
                    return Err(PlanFailure::new(
                        "manifest_corrupt",
                        "manifest 的 managedRoots 不是字符串数组",
                    ));
                };
                if RootId::parse(id).is_none() {
                    return Err(PlanFailure::new(
                        "manifest_unsupported",
                        format!("manifest 声明了本应用不认识的受管根 {id}"),
                    ));
                }
            }
        }
        Some(_) => {
            return Err(PlanFailure::new(
                "manifest_corrupt",
                "manifest 的 managedRoots 不是字符串数组",
            ))
        }
    }

    let manifest: Manifest = serde_json::from_value(value).map_err(|error| {
        PlanFailure::new("manifest_corrupt", format!("manifest 字段不完整：{error}"))
    })?;
    // agent 根是所有快照的前提，缺了就当损坏而不是「不声明」。
    if !manifest.managed_roots.contains(&RootId::Agent) {
        return Err(PlanFailure::new(
            "manifest_corrupt",
            "manifest 的 managedRoots 缺少 agent 根",
        ));
    }
    Ok(manifest)
}

/// 逐文件重算大小与哈希。返回（最多 [`MAX_LISTED_MISMATCHES`] 条明细, 不符总数）。
fn verify_manifest_files(sync_root: &Path, files: &[ManifestFile]) -> (Vec<HashMismatch>, usize) {
    let mut mismatches: Vec<HashMismatch> = Vec::new();
    let mut total = 0;

    for file in files {
        let mut record = |reason: MismatchReason, actual_sha256: Option<String>| {
            total += 1;
            if mismatches.len() < MAX_LISTED_MISMATCHES {
                mismatches.push(HashMismatch {
                    path: file.path.clone(),
                    reason,
                    expected_sha256: file.sha256.clone(),
                    actual_sha256,
                });
            }
        };

        let Ok(target) = scope::resolve_within(sync_root, Path::new(&file.path)) else {
            record(MismatchReason::OutOfBounds, None);
            continue;
        };
        let bytes = match fs::read(&target) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                record(MismatchReason::Missing, None);
                continue;
            }
            Err(_) => {
                record(MismatchReason::Unreadable, None);
                continue;
            }
        };

        let actual = snapshot::sha256_hex(&bytes);
        if bytes.len() as u64 != file.size {
            record(MismatchReason::Size, Some(actual));
        } else if actual != file.sha256 {
            record(MismatchReason::Hash, Some(actual));
        }
    }

    (mismatches, total)
}

/// live 侧的一个文件。哈希读不出来时留空，该文件按"有差异"处理。
struct LiveFile {
    size: u64,
    sha256: Option<String>,
}

/// live 侧扫描结果。键是（所属根, 根内相对路径）：两个根可能有相同相对路径，不能串。
struct LiveScan {
    files: BTreeMap<(RootId, String), LiveFile>,
    /// 快照没声明的链接：只能删链接本身，所以只记路径，目标内容永不读取。
    undeclared_links: Vec<(RootId, String)>,
}

/// 按 manifest 声明的根解析本机路径。未声明的根不出现：目标机完全不读、不写、不删。
pub(crate) fn resolve_roots(
    agent_dir: &Path,
    home_dir: Option<&Path>,
    managed_roots: &[RootId],
) -> Result<ManagedRoots, PlanFailure> {
    let mut roots = vec![ManagedRoot::agent(agent_dir)];
    if managed_roots.contains(&RootId::AgentsSkills) {
        let home_dir = home_dir.ok_or_else(|| {
            PlanFailure::new(
                "home_dir_missing",
                "快照声明了 ~/.agents/skills，但无法确定本机 home 目录，已停止生成计划",
            )
        })?;
        roots.push(ManagedRoot::agents_skills(home_dir));
    }
    Ok(ManagedRoots::new(roots))
}

/// 外部根在本机的状态：`Ok(false)` = 还没建（按空目录处理）。
/// 目标机多出的技能要能列为删除候选，缺失则快照里的内容全算新增。
fn external_root_usable(path: &Path) -> Result<bool, PlanFailure> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(PlanFailure::new(
            "external_root_invalid",
            format!("{} 存在但不是目录，无法落地快照", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(PlanFailure::new(
            "external_root_unreadable",
            format!("无法访问 {}：{error}", path.display()),
        )),
    }
}

/// 按受管范围扫 live 并算哈希。范围外文件永不读取。
fn live_scan(roots: &ManagedRoots, manifest: &Manifest) -> Result<LiveScan, PlanFailure> {
    let mut scan = LiveScan {
        files: BTreeMap::new(),
        undeclared_links: Vec::new(),
    };

    for root in roots.iter() {
        if root.id == RootId::AgentsSkills && !external_root_usable(&root.path)? {
            continue;
        }

        let scanned = scope::scan_root(root).map_err(|error| {
            let (code, what) = match root.id {
                RootId::Agent => ("agent_dir_unreadable", "宿主目录"),
                RootId::AgentsSkills => ("external_root_unreadable", "~/.agents/skills"),
            };
            PlanFailure::new(
                code,
                format!("无法遍历{what} {}：{error}", root.path.display()),
            )
        })?;

        // manifest 提到过的路径都算受管：链接本身在 `symlinks`，链接下的内容在 `files`。
        let declared_links: Vec<String> = manifest
            .symlinks
            .iter()
            .filter_map(|link| split_manifest_path(&link.path))
            .filter(|(id, _)| *id == root.id)
            .map(|(_, relative)| slash(&relative))
            .collect();
        let mut declared: Vec<String> = manifest
            .files
            .iter()
            .filter_map(|file| split_manifest_path(&file.path))
            .filter(|(id, _)| *id == root.id)
            .map(|(_, relative)| slash(&relative))
            .chain(declared_links.iter().cloned())
            .collect();
        declared.sort();

        let links: Vec<String> = scanned
            .entries
            .iter()
            .filter(|entry| entry.link_target.is_some())
            .map(|entry| slash(&entry.relative_path))
            .collect();
        let undeclared = undeclared_links(&links, &declared);

        for entry in scanned
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::File)
        {
            let path = slash(&entry.relative_path);
            if undeclared.iter().any(|link| is_under(&path, link)) {
                continue;
            }
            let sha256 = fs::read(root.path.join(&entry.relative_path))
                .ok()
                .map(|bytes| snapshot::sha256_hex(&bytes));
            scan.files.insert(
                (root.id, path),
                LiveFile {
                    size: entry.size,
                    sha256,
                },
            );
        }
        scan.undeclared_links
            .extend(undeclared.into_iter().map(|path| (root.id, path)));
    }

    Ok(scan)
}

/// 没被 manifest 提到的链接是陈旧残留：不进去读，列为可删除项（只删链接本身）。
/// 下有 manifest 内容的链接、以及嵌在另一个待删链接里的链接都不算：前者那部分内容
/// 本来就该按受管内容比较，后者删外层就一起没了。
fn undeclared_links(links: &[String], declared: &[String]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for path in links {
        if declared.contains(path)
            || declared.iter().any(|item| is_under(item, path))
            || found.iter().any(|link| is_under(path, link))
        {
            continue;
        }
        found.push(path.clone());
    }
    found
}

/// 路径是否在某个链接目录之下（不含链接本身）。
fn is_under(path: &str, link: &str) -> bool {
    path.strip_prefix(link)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// 比较 manifest 与 live：快照有 live 无 → 新增；都有但哈希不同 → 修改；live 有快照无 → 删除。
fn build_plan(manifest: &Manifest, live: &LiveScan) -> (Plan, PlanTotals) {
    let mut plan = Plan::default();
    let mut unchanged = 0;
    let mut in_manifest: BTreeSet<(RootId, String)> = BTreeSet::new();

    for file in &manifest.files {
        let Some((root, relative)) = split_manifest_path(&file.path) else {
            continue;
        };
        // manifest 里出现未声明的根或范围外路径（例如被改过的 manifest）：永不进入计划。
        if !manifest.managed_roots.contains(&root) || !scope::is_managed_relative(root, &relative) {
            continue;
        }
        let key = (root, slash(&relative));
        in_manifest.insert(key.clone());

        match live.files.get(&key) {
            None => plan.added.push(PlanItem {
                root,
                path: key.1,
                category: PlanCategory::Added,
                kind: PlanKind::File,
                size: file.size,
                snapshot_sha256: Some(file.sha256.clone()),
                live_sha256: None,
            }),
            Some(live_file) if live_file.sha256.as_deref() == Some(file.sha256.as_str()) => {
                unchanged += 1
            }
            Some(live_file) => plan.modified.push(PlanItem {
                root,
                path: key.1,
                category: PlanCategory::Modified,
                kind: PlanKind::File,
                size: file.size,
                snapshot_sha256: Some(file.sha256.clone()),
                live_sha256: live_file.sha256.clone(),
            }),
        }
    }

    for (key, live_file) in &live.files {
        if !in_manifest.contains(key) {
            plan.deleted.push(PlanItem {
                root: key.0,
                path: key.1.clone(),
                category: PlanCategory::Deleted,
                kind: PlanKind::File,
                size: live_file.size,
                snapshot_sha256: None,
                live_sha256: live_file.sha256.clone(),
            });
        }
    }

    for (root, path) in &live.undeclared_links {
        plan.deleted.push(PlanItem {
            root: *root,
            path: path.clone(),
            category: PlanCategory::Deleted,
            kind: PlanKind::Link,
            size: 0,
            snapshot_sha256: None,
            live_sha256: None,
        });
    }

    for group in [&mut plan.added, &mut plan.modified, &mut plan.deleted] {
        group.sort_by(|left, right| {
            left.root
                .cmp(&right.root)
                .then_with(|| left.path.cmp(&right.path))
        });
    }

    let totals = PlanTotals {
        snapshot_files: manifest.files.len(),
        live_files: live.files.len(),
        added: plan.added.len(),
        modified: plan.modified.len(),
        deleted: plan.deleted.len(),
        unchanged,
    };
    (plan, totals)
}

fn slash(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// 宿主版本探测：POSIX 一条登录 shell 脚本（node 用 `node -p process.versions.node`，
/// pi 用 `pi --version`，共用 5s 超时）；Windows 逐命令跑，与医生、安装页同一套入口解析。
fn probe_versions(
    agent_dir: &Path,
    runner: &dyn CommandRunner,
    host: HostOs,
) -> (Option<Version>, Option<Version>) {
    if host == HostOs::Windows {
        return probe_versions_windows(agent_dir, runner, &ProgramPath::from_env());
    }

    let stdout = match runner.run(VERSION_PROBE_SCRIPT, PROBE_TIMEOUT, &[]) {
        CommandOutcome::Ok { stdout, .. } | CommandOutcome::Failed { stdout, .. } => stdout,
        // 探测不到就记未知，不阻断计划。
        CommandOutcome::Timeout | CommandOutcome::Unavailable(_) => return (None, None),
    };

    let field = |key: &str| -> Option<Version> {
        let line = stdout
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}:")))?;
        doctor::parse_version(line)
    };
    (field("node"), field("pi"))
}

/// Windows 的逐命令版本探测：程序按 PATH 与 PATHEXT 解析，`pi` 再兜底 `<agent_dir>/bin`
/// 与 npm 全局前缀；解析不到或执行失败都记未知，不阻断计划。
fn probe_versions_windows(
    agent_dir: &Path,
    runner: &dyn CommandRunner,
    search: &ProgramPath,
) -> (Option<Version>, Option<Version>) {
    let node = search
        .find("node")
        .and_then(|program| program_version(runner, &program, &["-p", "process.versions.node"]));
    let npm_prefix = search
        .find("npm")
        .and_then(|npm| doctor::npm_global_prefix(runner, &npm));
    let pi = doctor::find_pi(search, Some(agent_dir), npm_prefix.as_deref())
        .and_then(|program| program_version(runner, &program, &["--version"]));
    (node, pi)
}

fn program_version(runner: &dyn CommandRunner, program: &Path, args: &[&str]) -> Option<Version> {
    match doctor::probe_program(runner, program, args) {
        CommandOutcome::Ok { stdout, .. } | CommandOutcome::Failed { stdout, .. } => {
            doctor::parse_version(&doctor::first_line(&stdout))
        }
        CommandOutcome::Timeout | CommandOutcome::Unavailable(_) => None,
    }
}

fn requirement_checks(
    requirements: &Requirements,
    agent_dir: &Path,
    runner: &dyn CommandRunner,
    host: HostOs,
) -> Vec<RequirementCheck> {
    let (node, pi) = probe_versions(agent_dir, runner, host);
    [
        ("node", requirements.node.as_str(), node),
        ("pi", requirements.pi.as_str(), pi),
    ]
    .into_iter()
    .map(|(tool, required, found)| RequirementCheck {
        tool,
        required: required.to_string(),
        found: found.map(format_version),
        satisfied: satisfies(required, found),
    })
    .collect()
}

/// 只有 `>=x.y.z` 这种写法参与比较；其他写法记未知，不误报。
fn satisfies(required: &str, found: Option<Version>) -> Option<bool> {
    let found = found?;
    let minimum = doctor::parse_version(required.trim().strip_prefix(">=")?)?;
    Some(found >= minimum)
}

fn format_version(version: Version) -> String {
    format!("{}.{}.{}", version.0, version.1, version.2)
}

/// command 层入口：同步根目录与宿主目录取自应用配置，然后跑一次只读检查。
///
/// `root` 为 `Some` 时用它顶替同步根：WebDAV 下载把快照放在暂存目录里，校验与计划
/// 必须能指向那里。允许的根由 [`webdav_transfer::resolve_root`] 统一判定。
fn probe(app: &AppHandle, root: Option<String>) -> Result<PlanReport, PlanFailure> {
    let config_dir = app.path().app_config_dir().map_err(|error| {
        PlanFailure::new(
            "app_config_unavailable",
            format!("无法定位应用配置目录：{error}"),
        )
    })?;
    let config = config::load_config(&config_dir)
        .map_err(|error| PlanFailure::new("app_config_unavailable", error.to_string()))?;

    let sync_root = match root {
        Some(requested) => {
            let app_data = app.path().app_data_dir().map_err(|error| {
                PlanFailure::new(
                    "app_data_unavailable",
                    format!("无法定位应用数据目录：{error}"),
                )
            })?;
            webdav_transfer::resolve_root(config.sync_root.as_deref(), &app_data, &requested)
                .map_err(|message| PlanFailure::new("root_not_allowed", message))?
        }
        None => config.sync_root.clone().ok_or_else(|| {
            PlanFailure::new(
                "sync_root_missing",
                "还没有设置同步文件夹，请先在设置页选择",
            )
        })?,
    };

    let agent_dir = agent_dir(app, &config)?;
    let home_dir = app.path().home_dir().ok();

    plan_snapshot(
        &agent_dir,
        home_dir.as_deref(),
        &sync_root,
        &ShellRunner::from_env(),
        HostOs::current(),
    )
}

/// 宿主目录：配置 + 环境变量 + 家目录兜底。对比与检查都要它，抽出来免得两处各解析一遍。
pub(crate) fn agent_dir(
    app: &AppHandle,
    config: &config::AppConfig,
) -> Result<PathBuf, PlanFailure> {
    // 只读取这一个文档化的环境变量；错误信息里不回显其内容。
    let env_agent_dir = std::env::var_os(config::AGENT_DIR_ENV_VAR).map(PathBuf::from);
    let home_dir = app.path().home_dir().ok();
    let resolved = config::resolve_agent_dir(config, env_agent_dir.as_deref(), home_dir.as_deref());

    resolved
        .path
        .ok_or_else(|| PlanFailure::new("agent_dir_missing", "无法确定宿主目录，请先在设置页填写"))
}

/// 子进程会阻塞最多 5 秒，放阻塞线程池，别占住 UI 线程。
#[tauri::command]
pub async fn verify_and_plan(
    app: AppHandle,
    root: Option<String>,
) -> Result<PlanReport, PlanFailure> {
    tauri::async_runtime::spawn_blocking(move || {
        // 与上传/下载/应用/回滚共用一把锁：计划要读暂存目录，而取包成功后会清掉旧暂存。
        let _guard = runtime::lock_operation();
        probe(&app, root)
    })
    .await
    .map_err(|error| PlanFailure::new("plan_failed", format!("检查失败：{error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// 临时目录夹具：Drop 时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-plan-{}-{}-{}",
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

    /// 固定版本的假命令：只回 node / pi 两行，不真的起进程。
    struct FakeRunner {
        node: &'static str,
        pi: &'static str,
    }

    impl FakeRunner {
        fn ok() -> Self {
            Self {
                node: "22.23.0",
                pi: "1.5.2",
            }
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
                stdout: format!("node:{}\npi:{}\n", self.node, self.pi),
                stderr: String::new(),
            }
        }
    }

    struct TimeoutRunner;

    impl CommandRunner for TimeoutRunner {
        fn run(
            &self,
            _script: &str,
            _timeout: Duration,
            _env: &[(&str, String)],
        ) -> CommandOutcome {
            CommandOutcome::Timeout
        }
    }

    /// 单测统一按 POSIX 形态检查：版本探测走登录 shell 替身，
    /// 同一组用例在 Windows 上跑也走同一条路。
    fn plan_snapshot_posix(
        agent_dir: &Path,
        sync_root: &Path,
        runner: &dyn CommandRunner,
    ) -> Result<PlanReport, PlanFailure> {
        plan_snapshot(agent_dir, None, sync_root, runner, HostOs::Posix)
    }

    /// Windows 逐命令探测的替身：按顺序回放结果，并记下每个 argv。
    struct FakePrograms {
        outcomes: RefCell<Vec<CommandOutcome>>,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl FakePrograms {
        fn new(outcomes: Vec<CommandOutcome>) -> Self {
            Self {
                outcomes: RefCell::new(outcomes),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn lines(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(|argv| crate::platform::cmd_line_of(argv))
                .collect()
        }
    }

    impl CommandRunner for FakePrograms {
        fn run(
            &self,
            _script: &str,
            _timeout: Duration,
            _env: &[(&str, String)],
        ) -> CommandOutcome {
            CommandOutcome::Unavailable("Windows 探测不该走脚本入口".to_string())
        }

        fn run_program(
            &self,
            argv: &[String],
            _timeout: Duration,
            _env: &[(&str, String)],
        ) -> CommandOutcome {
            self.calls.borrow_mut().push(argv.to_vec());
            let mut outcomes = self.outcomes.borrow_mut();
            if outcomes.is_empty() {
                CommandOutcome::Timeout
            } else {
                outcomes.remove(0)
            }
        }
    }

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("创建父目录");
        }
        fs::write(path, content).expect("写入夹具文件");
    }

    /// 导出过的快照 + 同一份 live：先保证"无差异"是基线。
    fn fixture(temp: &TempDir) -> (PathBuf, PathBuf) {
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        write_file(&agent.join("settings.json"), "{\"theme\": \"dark\"}");
        write_file(&agent.join("AGENTS.md"), "# 指令\n");
        write_file(&agent.join("skills/antd/SKILL.md"), "# 技能\n");
        // auth.json 进快照；sessions 在范围外，永不进计划。
        write_file(&agent.join("auth.json"), "{}");
        write_file(&agent.join("sessions/session.json"), "{}");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        snapshot::export_fixture(&agent, &sync_root).expect("导出夹具快照");
        (agent, sync_root)
    }

    #[test]
    fn only_links_outside_the_manifest_become_delete_candidates() {
        let declared = vec![
            "settings.json".to_string(),
            "skills/managed".to_string(),
            "skills/holder/deep/SKILL.md".to_string(),
        ];
        let links = vec![
            "skills/managed".to_string(),
            "skills/holder".to_string(),
            "stale".to_string(),
            "stale/nested".to_string(),
            "old".to_string(),
        ];
        assert_eq!(
            undeclared_links(&links, &declared),
            vec!["stale".to_string(), "old".to_string()],
            "已声明链接、含已声明内容的链接、嵌在待删链接里的链接都不列"
        );
    }

    #[cfg(unix)]
    #[test]
    fn undeclared_symlink_is_a_deletable_item_and_its_contents_are_never_compared() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("undeclared-link");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        let outside = temp.path().join("outside");
        write_file(&agent.join("settings.json"), "{\"theme\": \"dark\"}");
        write_file(&outside.join("keep.txt"), "keep");
        fs::create_dir_all(&sync_root).expect("建同步目录");
        snapshot::export_fixture(&agent, &sync_root).expect("导出夹具快照");
        // 导出之后才出现的链接：快照里没有它，只能删链接本身。
        fs::create_dir_all(agent.join("skills")).expect("建 skills");
        symlink(&outside, agent.join("skills/extra")).expect("建目录链接");

        let report = plan_snapshot(&agent, None, &sync_root, &FakeRunner::ok(), HostOs::Posix)
            .expect("生成计划");

        assert_eq!(paths(&report.plan.deleted), vec!["skills/extra"]);
        assert_eq!(report.plan.deleted[0].kind, PlanKind::Link);
        assert_eq!(report.plan.deleted[0].size, 0);
        assert_eq!(report.totals.live_files, 1, "链接目标内容不进受管清单");
        assert!(report.plan.added.is_empty(), "目标内容不能被当成新增");
    }

    #[cfg(unix)]
    #[test]
    fn declared_symlink_contents_are_compared_and_the_link_is_not_a_candidate() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("declared-link");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        let outside = temp.path().join("outside");
        write_file(&agent.join("settings.json"), "{\"theme\": \"dark\"}");
        fs::create_dir_all(&outside).expect("建外部目录");
        write_file(&outside.join("keep.txt"), "keep");
        fs::create_dir_all(agent.join("skills")).expect("建 skills");
        fs::create_dir_all(&sync_root).expect("建同步目录");
        symlink(&outside, agent.join("skills/extra")).expect("建目录链接");
        snapshot::export_fixture(&agent, &sync_root).expect("导出夹具快照");

        let report = plan_snapshot(&agent, None, &sync_root, &FakeRunner::ok(), HostOs::Posix)
            .expect("生成计划");

        assert!(report.plan.deleted.is_empty(), "已声明链接不是删除候选");
        assert!(report.plan.added.is_empty(), "链接下的内容按受管内容比较");
        assert_eq!(report.totals.live_files, 2);
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

    /// 整棵子树的路径与内容，用于证明“没有写任何文件”。
    fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut entries = Vec::new();
        for item in fs::read_dir(root).expect("读取目录") {
            let path = item.expect("目录项").path();
            let relative = slash(path.strip_prefix(root).expect("相对路径"));
            if path.is_dir() {
                entries.extend(tree(&path));
            } else {
                entries.push((relative, fs::read(&path).unwrap_or_default()));
            }
        }
        entries.sort();
        entries
    }

    fn paths(items: &[PlanItem]) -> Vec<String> {
        items.iter().map(|item| item.path.clone()).collect()
    }

    #[test]
    fn identical_live_has_no_differences_and_out_of_scope_never_shows_up() {
        let temp = TempDir::new("identical");
        let (agent, sync_root) = fixture(&temp);

        // live 里多出范围外的文件：永远不出现为删除候选。
        write_file(&agent.join("sessions/late.json"), "{}");

        let report =
            plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok()).expect("应生成计划");
        assert!(report.plan.added.is_empty(), "{:?}", report.plan);
        assert!(report.plan.modified.is_empty(), "{:?}", report.plan);
        assert!(report.plan.deleted.is_empty(), "{:?}", report.plan);
        assert_eq!(report.totals.unchanged, 4);
        assert_eq!(report.totals.snapshot_files, 4);
        assert_eq!(report.totals.live_files, 4, "范围外文件不参与比较");
        assert_eq!(report.verification.checked, 4);
        assert_eq!(report.manifest_info.files, 4);
        assert_eq!(report.manifest_info.fingerprint.len(), 12);
        assert_eq!(report.manifest_info.source.os, std::env::consts::OS);
        assert_eq!(report.manifest_info.requirements.node, ">=22.19.0");

        // 版本满足要求时不产生任何警告。
        assert_eq!(
            report.requirements,
            vec![
                RequirementCheck {
                    tool: "node",
                    required: ">=22.19.0".to_string(),
                    found: Some("22.23.0".to_string()),
                    satisfied: Some(true),
                },
                RequirementCheck {
                    tool: "pi",
                    required: ">=1.5.0".to_string(),
                    found: Some("1.5.2".to_string()),
                    satisfied: Some(true),
                },
            ]
        );
    }

    #[test]
    fn added_modified_deleted_are_classified() {
        let temp = TempDir::new("classify");
        let (agent, sync_root) = fixture(&temp);

        // 快照有 live 无 → 新增；两边都有且哈希不同 → 修改；live 有快照无 → 删除。
        fs::remove_file(agent.join("skills/antd/SKILL.md")).expect("删除 live 文件");
        write_file(&agent.join("settings.json"), "{\"theme\": \"light\"}");
        write_file(&agent.join("web-search.json"), "{\"provider\": \"exa\"}");

        let report =
            plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok()).expect("应生成计划");

        assert_eq!(paths(&report.plan.added), vec!["skills/antd/SKILL.md"]);
        assert_eq!(paths(&report.plan.modified), vec!["settings.json"]);
        assert_eq!(paths(&report.plan.deleted), vec!["web-search.json"]);
        assert_eq!(report.totals.unchanged, 2, "AGENTS.md 与 auth.json 未改动");
        assert_eq!(
            (
                report.totals.added,
                report.totals.modified,
                report.totals.deleted
            ),
            (1, 1, 1)
        );

        let added = &report.plan.added[0];
        assert_eq!(added.category, PlanCategory::Added);
        assert!(added.snapshot_sha256.is_some());
        assert!(added.live_sha256.is_none());
        assert!(added.size > 0);

        let modified = &report.plan.modified[0];
        assert_eq!(modified.category, PlanCategory::Modified);
        assert_ne!(modified.snapshot_sha256, modified.live_sha256);
        assert!(modified.live_sha256.is_some());

        let deleted = &report.plan.deleted[0];
        assert_eq!(deleted.category, PlanCategory::Deleted);
        assert!(deleted.snapshot_sha256.is_none());
        assert!(deleted.live_sha256.is_some());
    }

    #[test]
    fn tampered_snapshot_file_fails_with_hash_mismatch_and_writes_nothing() {
        let temp = TempDir::new("tampered");
        let (agent, sync_root) = fixture(&temp);
        let sync_before = dir_entries(&sync_root);
        let live_before = tree(&agent);
        let manifest_before = fs::read(sync_root.join(MANIFEST_FILE_NAME)).expect("读 manifest");

        let target = sync_root.join("snapshot/settings.json");
        let mut bytes = fs::read(&target).expect("读取快照文件");
        bytes.push(b' ');
        fs::write(&target, &bytes).expect("改写快照文件");

        let failure =
            plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok()).expect_err("应报哈希不符");
        assert_eq!(failure.code, "hash_mismatch");
        assert!(
            failure.message.contains("1 个文件不匹配"),
            "{}",
            failure.message
        );
        assert_eq!(failure.mismatches.len(), 1);
        assert_eq!(failure.mismatches[0].path, "snapshot/settings.json");
        assert_eq!(failure.mismatches[0].reason, MismatchReason::Size);
        assert_eq!(failure.mismatches[0].expected_sha256.len(), 64);
        assert!(failure.mismatches[0].actual_sha256.is_some());

        // 只读：live 目录逐字节不变，同步目录没有新增/删除任何东西。
        assert_eq!(tree(&agent), live_before);
        assert_eq!(dir_entries(&sync_root), sync_before);
        assert_eq!(
            fs::read(sync_root.join(MANIFEST_FILE_NAME)).expect("读 manifest"),
            manifest_before
        );
    }

    #[test]
    fn missing_snapshot_file_counts_as_mismatch() {
        let temp = TempDir::new("missing-file");
        let (agent, sync_root) = fixture(&temp);
        fs::remove_file(sync_root.join("snapshot/AGENTS.md")).expect("删除快照文件");

        let failure =
            plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok()).expect_err("应报哈希不符");
        assert_eq!(failure.code, "hash_mismatch");
        assert_eq!(failure.mismatches.len(), 1);
        assert_eq!(failure.mismatches[0].reason, MismatchReason::Missing);
        assert!(failure.mismatches[0].actual_sha256.is_none());
    }

    #[test]
    fn mismatches_are_capped_at_twenty() {
        let temp = TempDir::new("cap");
        let (agent, sync_root) = fixture(&temp);
        for index in 0..25 {
            write_file(
                &sync_root.join(format!("snapshot/skills/extra/{index}.md")),
                "x",
            );
        }
        // manifest 里补上这 25 个文件，然后全部篡改。
        let manifest_path = sync_root.join(MANIFEST_FILE_NAME);
        let mut manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&manifest_path).expect("读 manifest"))
                .expect("解析 manifest");
        let files = manifest["files"].as_array_mut().expect("files 数组");
        for index in 0..25 {
            let relative = format!("snapshot/skills/extra/{index}.md");
            files.push(serde_json::json!({
                "path": relative,
                "sha256": "ab".repeat(32),
                "size": 1,
                "kind": "markdown",
            }));
        }
        fs::write(&manifest_path, manifest.to_string()).expect("写回 manifest");

        let failure = plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok())
            .expect_err("25 条不符应中止");
        assert_eq!(failure.code, "hash_mismatch");
        assert_eq!(failure.mismatches.len(), MAX_LISTED_MISMATCHES);
        assert!(
            failure.message.contains("25 个文件不匹配"),
            "{}",
            failure.message
        );
    }

    /// 只造 `files`，其余字段固定：对比只看文件路径与哈希。
    fn manifest_of(files: &[(&str, &str)]) -> Manifest {
        let files: Vec<ManifestFile> = files
            .iter()
            .map(|(path, content)| ManifestFile {
                path: snapshot::manifest_path(RootId::Agent, Path::new(path)),
                sha256: snapshot::sha256_hex(content.as_bytes()),
                size: content.len() as u64,
                kind: "text".to_string(),
            })
            .collect();

        Manifest {
            format_version: FORMAT_VERSION,
            profile_id: snapshot::PROFILE_ID.to_string(),
            managed_roots: vec![RootId::Agent],
            created_at: "2026-11-22T10:52:30Z".to_string(),
            fingerprint: snapshot::fingerprint(FORMAT_VERSION, &[RootId::Agent], &files),
            source: SourceInfo {
                app: "0.1.0".to_string(),
                os: "linux".to_string(),
                arch: "x64".to_string(),
                pi: None,
                node: None,
            },
            requirements: Requirements {
                node: ">=22.19.0".to_string(),
                pi: ">=1.5.0".to_string(),
            },
            files,
            symlinks: Vec::new(),
            required_packages: Vec::new(),
            plaintext_secrets: false,
        }
    }

    #[test]
    fn diff_manifests_ignores_files_that_match() {
        let left = manifest_of(&[("settings.json", "{}"), ("skills/a.md", "a")]);
        let right = manifest_of(&[("skills/a.md", "a"), ("settings.json", "{}")]);

        assert_eq!(diff_manifests(&left, &right), ManifestDiff::default());
    }

    #[test]
    fn diff_manifests_reports_files_only_one_side_has() {
        let left = manifest_of(&[("mcp.json", "{}"), ("skills/only-left.md", "a")]);
        let right = manifest_of(&[("mcp.json", "{}"), ("skills/only-right.md", "b")]);

        let diff = diff_manifests(&left, &right);
        assert_eq!(diff.added, vec!["skills/only-right.md".to_string()]);
        assert_eq!(diff.removed, vec!["skills/only-left.md".to_string()]);
        assert!(diff.modified.is_empty());
    }

    #[test]
    fn diff_manifests_reports_hash_changes() {
        let left = manifest_of(&[("settings.json", "{\"v\":1}")]);
        let right = manifest_of(&[("settings.json", "{\"v\":2}")]);

        let diff = diff_manifests(&left, &right);
        assert_eq!(diff.modified, vec!["settings.json".to_string()]);
        assert!(diff.added.is_empty() && diff.removed.is_empty());
    }

    #[test]
    fn diff_manifests_against_an_empty_side_is_sorted() {
        let empty = manifest_of(&[]);
        let full = manifest_of(&[("skills/b.md", "b"), ("AGENTS.md", "#")]);

        assert_eq!(
            diff_manifests(&empty, &full).added,
            vec!["AGENTS.md".to_string(), "skills/b.md".to_string()]
        );
        assert_eq!(diff_manifests(&full, &empty).removed.len(), 2);
        assert!(diff_manifests(&empty, &empty).added.is_empty());
    }

    #[test]
    fn manifest_missing_unsupported_and_corrupt_are_distinguished() {
        let temp = TempDir::new("manifest");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        write_file(&agent.join("settings.json"), "{}");
        fs::create_dir_all(&sync_root).expect("创建同步目录");

        let failure = plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok())
            .expect_err("缺 manifest 应失败");
        assert_eq!(failure.code, "manifest_missing");

        fs::write(
            sync_root.join(MANIFEST_FILE_NAME),
            "{\"formatVersion\": 99}",
        )
        .expect("写 manifest");
        let failure = plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok())
            .expect_err("版本不支持应失败");
        assert_eq!(failure.code, "manifest_unsupported");
        assert!(failure.message.contains("99"), "{}", failure.message);

        fs::write(sync_root.join(MANIFEST_FILE_NAME), "not json").expect("写坏 manifest");
        let failure =
            plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok()).expect_err("损坏应失败");
        assert_eq!(failure.code, "manifest_corrupt");

        // 合法 JSON 但缺 formatVersion：同样是损坏。
        fs::write(
            sync_root.join(MANIFEST_FILE_NAME),
            "{\"profileId\": \"default\"}",
        )
        .expect("写缺字段 manifest");
        let failure =
            plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok()).expect_err("缺字段应失败");
        assert_eq!(failure.code, "manifest_corrupt");

        // 旧版本同样按"不支持"拒绝，不尝试兼容。
        fs::write(
            sync_root.join(MANIFEST_FILE_NAME),
            "{\"formatVersion\": 1, \"profileId\": \"default\"}",
        )
        .expect("写旧版 manifest");
        let failure =
            plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok()).expect_err("旧版本应失败");
        assert_eq!(failure.code, "manifest_unsupported");

        // 版本对了但字段不全：结构损坏，不是"版本不支持"。
        fs::write(
            sync_root.join(MANIFEST_FILE_NAME),
            format!("{{\"formatVersion\": {FORMAT_VERSION}, \"profileId\": \"default\"}}"),
        )
        .expect("写不完整 manifest");
        let failure =
            plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok()).expect_err("结构不全应失败");
        assert_eq!(failure.code, "manifest_corrupt");
    }

    #[test]
    fn out_of_scope_manifest_entry_never_becomes_a_plan_item() {
        let temp = TempDir::new("scope-guard");
        let (agent, sync_root) = fixture(&temp);

        // 手改 manifest，把范围外的 sessions/auth.json 塞进去（哈希与大小都对得上）。
        let extra = sync_root.join("snapshot/sessions/auth.json");
        let bytes = b"{\"openai\": {}}";
        fs::create_dir_all(extra.parent().unwrap()).expect("建范围外目录");
        fs::write(&extra, bytes).expect("写范围外快照文件");
        let manifest_path = sync_root.join(MANIFEST_FILE_NAME);
        let mut manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&manifest_path).expect("读 manifest"))
                .expect("解析 manifest");
        manifest["files"]
            .as_array_mut()
            .expect("files 数组")
            .push(serde_json::json!({
                "path": "snapshot/sessions/auth.json",
                "sha256": snapshot::sha256_hex(bytes),
                "size": bytes.len() as u64,
                "kind": "json",
            }));
        fs::write(&manifest_path, manifest.to_string()).expect("写回 manifest");

        // 计划里不得出现范围外的 sessions/auth.json：它既不是新增也不参与比较。
        let report =
            plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok()).expect("应生成计划");
        assert!(report.plan.added.is_empty(), "{:?}", report.plan.added);
        assert!(report.plan.modified.is_empty());
        assert!(report.plan.deleted.is_empty());
        assert_eq!(report.totals.unchanged, 4);
        assert_eq!(report.totals.snapshot_files, 5);

        assert!(!paths(&report.plan.deleted)
            .iter()
            .any(|path| path == "sessions/auth.json"));
    }

    #[test]
    fn plan_order_is_deterministic() {
        let temp = TempDir::new("order");
        let (agent, sync_root) = fixture(&temp);
        for index in [3, 1, 2] {
            write_file(&agent.join(format!("skills/z/{index}.md")), "z");
        }
        for name in ["mcp.json", "keybindings.json"] {
            write_file(&agent.join(name), "{}");
        }
        fs::remove_file(agent.join("AGENTS.md")).expect("删除 live 指令文件");
        write_file(&agent.join("settings.json"), "{\"theme\": \"light\"}");

        let first = plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok()).expect("第一次");
        let second = plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok()).expect("第二次");
        assert_eq!(first, second);

        for group in [&first.plan.added, &first.plan.modified, &first.plan.deleted] {
            let mut sorted: Vec<String> = paths(group);
            sorted.sort();
            assert_eq!(paths(group), sorted, "组内必须按路径升序");
        }
        assert_eq!(paths(&first.plan.added), vec!["AGENTS.md"]);
        assert_eq!(paths(&first.plan.modified), vec!["settings.json"]);
        assert_eq!(
            paths(&first.plan.deleted),
            vec![
                "keybindings.json",
                "mcp.json",
                "skills/z/1.md",
                "skills/z/2.md",
                "skills/z/3.md",
            ]
        );
    }

    #[test]
    fn unsatisfied_or_unknown_versions_only_warn() {
        let temp = TempDir::new("versions");
        let (agent, sync_root) = fixture(&temp);

        let old = FakeRunner {
            node: "22.18.0",
            pi: "1.4.9",
        };
        let report = plan_snapshot_posix(&agent, &sync_root, &old).expect("版本不满足也要出计划");
        assert_eq!(report.requirements[0].satisfied, Some(false));
        assert_eq!(report.requirements[0].found.as_deref(), Some("22.18.0"));
        assert_eq!(report.requirements[1].satisfied, Some(false));
        assert_eq!(report.totals.unchanged, 4);

        // 探测失败：记未知，不误报为不满足。
        let report =
            plan_snapshot_posix(&agent, &sync_root, &TimeoutRunner).expect("探测失败也要出计划");
        assert_eq!(report.requirements[0].satisfied, None);
        assert!(report.requirements[0].found.is_none());
        assert_eq!(report.requirements[1].satisfied, None);
    }

    #[test]
    fn windows_version_probe_runs_one_command_each() {
        let temp = TempDir::new("windows-versions");
        let bin = temp.path().join("bin");
        fs::create_dir_all(&bin).expect("建目录");
        fs::write(bin.join("node.exe"), "").expect("写文件");
        fs::write(bin.join("pi.cmd"), "").expect("写文件");
        let search = ProgramPath::new(vec![bin.clone()], vec![".exe".into(), ".cmd".into()]);

        let runner = FakePrograms::new(vec![
            CommandOutcome::Ok {
                stdout: "22.23.0\n".to_string(),
                stderr: String::new(),
            },
            CommandOutcome::Ok {
                stdout: "1.5.2\n".to_string(),
                stderr: String::new(),
            },
        ]);
        let (node, pi) = probe_versions_windows(temp.path(), &runner, &search);

        assert_eq!(node, Some((22, 23, 0)));
        assert_eq!(pi, Some((1, 5, 2)));
        let node_program = bin.join("node.exe").display().to_string();
        let pi_program = bin.join("pi.cmd").display().to_string();
        assert_eq!(
            runner.lines(),
            vec![
                crate::platform::cmd_line(&node_program, &["-p", "process.versions.node"]),
                crate::platform::cmd_line(&pi_program, &["--version"]),
            ]
        );

        // 解析不到程序就不起进程：版本记未知，不误报为不满足。
        let empty = ProgramPath::new(Vec::new(), vec![".exe".into()]);
        let runner = FakePrograms::new(Vec::new());
        assert_eq!(
            probe_versions_windows(temp.path(), &runner, &empty),
            (None, None)
        );
        assert!(runner.calls.borrow().is_empty());
    }

    fn rooted(items: &[PlanItem]) -> Vec<(RootId, String)> {
        items
            .iter()
            .map(|item| (item.root, item.path.clone()))
            .collect()
    }

    /// 两个根的夹具：导出后两端 live 与快照一致，作为“无差异”基线。
    fn fixture_multi_root(temp: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
        let agent = temp.path().join("agent");
        let home = temp.path().join("home");
        let sync_root = temp.path().join("cloud");
        write_file(&agent.join("settings.json"), "{\"theme\": \"dark\"}");
        // 两个根放同一条相对路径：计划不能串。
        write_file(&agent.join("skills/shared/SKILL.md"), "# agent 技能\n");
        write_file(
            &home.join(".agents/skills/skills/shared/SKILL.md"),
            "# 外部技能\n",
        );
        write_file(
            &home.join(".agents/skills/extra/SKILL.md"),
            "# 只在外部的技能\n",
        );
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        snapshot::export_fixture_with(
            &agent,
            Some(&home),
            &sync_root,
            snapshot::SourceVersions::default(),
        )
        .expect("导出夹具快照");
        (agent, home, sync_root)
    }

    #[test]
    fn v2_manifest_is_rejected_as_unsupported() {
        let temp = TempDir::new("v2");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        write_file(&agent.join("settings.json"), "{}");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        fs::write(
            sync_root.join(MANIFEST_FILE_NAME),
            "{\"formatVersion\": 2, \"profileId\": \"default\"}",
        )
        .expect("写 v2 manifest");

        let failure = plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok())
            .expect_err("v2 快照必须被拒绍");
        assert_eq!(failure.code, "manifest_unsupported");
        assert!(failure.message.contains('2'), "{}", failure.message);
    }

    #[test]
    fn unknown_or_missing_agent_root_is_rejected() {
        let temp = TempDir::new("managed-roots");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        write_file(&agent.join("settings.json"), "{}");
        fs::create_dir_all(&sync_root).expect("创建同步目录");

        // 不认识的根 id：格式不支持，不尝试兼容。
        let mut manifest = serde_json::json!({
            "formatVersion": FORMAT_VERSION,
            "profileId": "default",
            "managedRoots": ["agent", "profiles"],
            "createdAt": "2026-11-20T05:30:00Z",
            "fingerprint": "000000000000",
            "source": { "app": "0.1.0", "os": "linux", "arch": "x64", "pi": null, "node": null },
            "requirements": { "node": ">=22.19.0", "pi": ">=1.5.0" },
            "files": [],
            "symlinks": [],
            "requiredPackages": []
        });
        fs::write(sync_root.join(MANIFEST_FILE_NAME), manifest.to_string()).expect("写 manifest");
        let failure = plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok())
            .expect_err("未知根 id 应失败");
        assert_eq!(failure.code, "manifest_unsupported");
        assert!(failure.message.contains("profiles"), "{}", failure.message);

        // 缺 agent 根：当损坏处理，不当作“只声明了外部根”。
        manifest["managedRoots"] = serde_json::json!(["agents-skills"]);
        fs::write(sync_root.join(MANIFEST_FILE_NAME), manifest.to_string()).expect("写 manifest");
        let failure = plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok())
            .expect_err("缺 agent 根应失败");
        assert_eq!(failure.code, "manifest_corrupt");

        // 字段整个缺失：为了让旧 `remote-state.json` 能解析，结构上允许缺省，
        // 但 v3 快照缺它就缺 agent 根，同样按损坏拒绝，不当成 agent-only。
        manifest
            .as_object_mut()
            .expect("manifest 是对象")
            .remove("managedRoots");
        fs::write(sync_root.join(MANIFEST_FILE_NAME), manifest.to_string()).expect("写 manifest");
        let failure = plan_snapshot_posix(&agent, &sync_root, &FakeRunner::ok())
            .expect_err("缺 managedRoots 应失败");
        assert_eq!(failure.code, "manifest_corrupt");
    }

    #[test]
    fn both_roots_are_planned_and_same_relative_paths_do_not_mix() {
        let temp = TempDir::new("multi-root");
        let (agent, home, sync_root) = fixture_multi_root(&temp);

        // 无差异基线。
        let report = plan_snapshot(
            &agent,
            Some(&home),
            &sync_root,
            &FakeRunner::ok(),
            HostOs::Posix,
        )
        .expect("应生成计划");
        assert!(report.plan.added.is_empty(), "{:?}", report.plan.added);
        assert!(
            report.plan.modified.is_empty(),
            "{:?}",
            report.plan.modified
        );
        assert!(report.plan.deleted.is_empty(), "{:?}", report.plan.deleted);
        assert_eq!(report.totals.unchanged, 4);
        assert_eq!(report.totals.snapshot_files, 4);
        assert_eq!(report.totals.live_files, 4);

        // 只改外部根的同名相对路径：agent 根的同名文件不能被误判为修改。
        write_file(
            &home.join(".agents/skills/skills/shared/SKILL.md"),
            "# 外部技能（改）\n",
        );
        let report = plan_snapshot(
            &agent,
            Some(&home),
            &sync_root,
            &FakeRunner::ok(),
            HostOs::Posix,
        )
        .expect("应生成计划");
        assert_eq!(
            rooted(&report.plan.modified),
            vec![(RootId::AgentsSkills, "skills/shared/SKILL.md".to_string())]
        );

        // 外部根的新增与删除也按根归属：live 少一项是新增，live 多一项是删除候选。
        write_file(
            &home.join(".agents/skills/new/SKILL.md"),
            "# 新技能
",
        );
        fs::remove_file(home.join(".agents/skills/extra/SKILL.md")).expect("删除外部技能");
        let report = plan_snapshot(
            &agent,
            Some(&home),
            &sync_root,
            &FakeRunner::ok(),
            HostOs::Posix,
        )
        .expect("应生成计划");
        assert_eq!(
            rooted(&report.plan.added),
            vec![(RootId::AgentsSkills, "extra/SKILL.md".to_string())]
        );
        assert_eq!(
            rooted(&report.plan.deleted),
            vec![(RootId::AgentsSkills, "new/SKILL.md".to_string())]
        );
        // 计划项的根按 id 序列化，前端据此区分两个根。
        let json = serde_json::to_value(&report.plan.deleted[0]).expect("序列化计划项");
        assert_eq!(json["root"], "agents-skills");
        assert_eq!(json["path"], "new/SKILL.md");
    }

    #[test]
    fn missing_target_external_root_lists_everything_as_added() {
        let temp = TempDir::new("multi-root-target");
        let (agent, _home, sync_root) = fixture_multi_root(&temp);
        // 目标机还没有 ~/.agents/skills：快照里的外部内容全部是新增。
        let other_home = temp.path().join("other-home");
        fs::create_dir_all(&other_home).expect("创建目标 home");

        let report = plan_snapshot(
            &agent,
            Some(&other_home),
            &sync_root,
            &FakeRunner::ok(),
            HostOs::Posix,
        )
        .expect("应生成计划");
        assert_eq!(
            rooted(&report.plan.added),
            vec![
                (RootId::AgentsSkills, "extra/SKILL.md".to_string()),
                (RootId::AgentsSkills, "skills/shared/SKILL.md".to_string()),
            ]
        );
        assert!(report.plan.deleted.is_empty(), "{:?}", report.plan.deleted);
    }

    #[test]
    fn extra_skills_on_the_target_root_become_delete_candidates() {
        let temp = TempDir::new("multi-root-extra");
        let (agent, _home, sync_root) = fixture_multi_root(&temp);
        let other_home = temp.path().join("other-home");
        write_file(
            &other_home.join(".agents/skills/only-here/SKILL.md"),
            "# 只在目标机的技能\n",
        );

        let report = plan_snapshot(
            &agent,
            Some(&other_home),
            &sync_root,
            &FakeRunner::ok(),
            HostOs::Posix,
        )
        .expect("应生成计划");
        assert_eq!(
            rooted(&report.plan.deleted),
            vec![(RootId::AgentsSkills, "only-here/SKILL.md".to_string())]
        );
    }

    #[test]
    fn empty_source_root_lists_target_only_skills_as_deleted() {
        let temp = TempDir::new("multi-root-empty-source");
        let agent = temp.path().join("agent");
        let home = temp.path().join("home");
        let sync_root = temp.path().join("cloud");
        write_file(&agent.join("settings.json"), "{\"theme\": \"dark\"}");
        // 源机外部根存在但为空：照样声明，目标机多出的技能全是删除候选。
        fs::create_dir_all(home.join(".agents/skills")).expect("创建空外部根");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        snapshot::export_fixture_with(
            &agent,
            Some(&home),
            &sync_root,
            snapshot::SourceVersions::default(),
        )
        .expect("导出夹具快照");

        let other_home = temp.path().join("other-home");
        write_file(
            &other_home.join(".agents/skills/only-here/SKILL.md"),
            "# 只在目标机的技能\n",
        );

        let report = plan_snapshot(
            &agent,
            Some(&other_home),
            &sync_root,
            &FakeRunner::ok(),
            HostOs::Posix,
        )
        .expect("应生成计划");
        assert!(report.plan.added.is_empty(), "{:?}", report.plan.added);
        assert_eq!(
            rooted(&report.plan.deleted),
            vec![(RootId::AgentsSkills, "only-here/SKILL.md".to_string())]
        );
    }

    #[test]
    fn external_root_that_is_not_a_directory_fails_the_plan() {
        let temp = TempDir::new("multi-root-file");
        let (agent, _home, sync_root) = fixture_multi_root(&temp);
        let other_home = temp.path().join("other-home");
        write_file(&other_home.join(".agents/skills"), "not a dir");

        let failure = plan_snapshot(
            &agent,
            Some(&other_home),
            &sync_root,
            &FakeRunner::ok(),
            HostOs::Posix,
        )
        .expect_err("外部根不是目录应失败");
        assert_eq!(failure.code, "external_root_invalid");
    }

    #[test]
    fn declared_agents_skills_without_a_home_stops_the_plan() {
        let temp = TempDir::new("multi-root-no-home");
        let (agent, _home, sync_root) = fixture_multi_root(&temp);

        let failure = plan_snapshot(&agent, None, &sync_root, &FakeRunner::ok(), HostOs::Posix)
            .expect_err("声明了外部根却解析不到 home 应失败");
        assert_eq!(failure.code, "home_dir_missing");
    }
}

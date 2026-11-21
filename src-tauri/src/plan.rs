//! 快照校验与导入计划（只读）。
//!
//! 规则来源：`docs/design/10-snapshot-sync.md` 第 4、6 节。
//! 只读：读 manifest、读快照文件、读 live 文件，不写任何文件；落地由 apply 负责。
//! 核心 [`plan_snapshot`] 不依赖 Tauri，子进程通过 [`CommandRunner`] 注入，可直接临时目录单测。
//!
//! 失败停在原地：manifest 缺失/版本不支持/损坏、任一文件哈希不符都直接返回失败，不给半套计划。

use crate::config;
use crate::doctor::{self, CommandOutcome, CommandRunner, ShellRunner, Version, PROBE_TIMEOUT};
use crate::scope::{self, EntryKind};
use crate::snapshot::{
    self, Manifest, ManifestFile, Requirements, SourceInfo, FORMAT_VERSION, MANIFEST_FILE_NAME,
    SNAPSHOT_DIR_NAME,
};
use crate::webdav_transfer;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
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

/// 计划项。路径相对宿主目录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanItem {
    /// 相对宿主目录的路径，`/` 分隔。
    pub path: String,
    pub category: PlanCategory,
    /// 快照侧字节数；`deleted` 时是 live 侧字节数。
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

/// core：校验快照并生成计划。`agent_dir` 是 live 宿主目录，`sync_root` 是云盘同步文件夹。
pub fn plan_snapshot(
    agent_dir: &Path,
    sync_root: &Path,
    runner: &dyn CommandRunner,
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

    let live = live_files(agent_dir)?;
    let (plan, totals) = build_plan(&manifest.files, &live);

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
        requirements: requirement_checks(&manifest.requirements, runner),
    })
}

/// 读 manifest：不存在 → `manifest_missing`；版本不认识 → `manifest_unsupported`；其余 → `manifest_corrupt`。
fn read_manifest(sync_root: &Path) -> Result<Manifest, PlanFailure> {
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

    serde_json::from_value(value).map_err(|error| {
        PlanFailure::new("manifest_corrupt", format!("manifest 字段不完整：{error}"))
    })
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

/// 按受管范围扫 live 并算哈希。范围外文件永不读取。
fn live_files(agent_dir: &Path) -> Result<BTreeMap<String, LiveFile>, PlanFailure> {
    let scanned = scope::scan(agent_dir).map_err(|error| {
        PlanFailure::new(
            "agent_dir_unreadable",
            format!("无法遍历宿主目录 {}：{error}", agent_dir.display()),
        )
    })?;

    let mut files = BTreeMap::new();
    for entry in scanned
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::File)
    {
        let sha256 = fs::read(agent_dir.join(&entry.relative_path))
            .ok()
            .map(|bytes| snapshot::sha256_hex(&bytes));
        files.insert(
            slash(&entry.relative_path),
            LiveFile {
                size: entry.size,
                sha256,
            },
        );
    }
    Ok(files)
}

/// 比较 manifest 与 live：快照有 live 无 → 新增；都有但哈希不同 → 修改；live 有快照无 → 删除。
fn build_plan(
    manifest_files: &[ManifestFile],
    live: &BTreeMap<String, LiveFile>,
) -> (Plan, PlanTotals) {
    let mut plan = Plan::default();
    let mut unchanged = 0;
    let mut in_manifest: Vec<String> = Vec::new();

    for file in manifest_files {
        let relative = live_relative(&file.path);
        // manifest 里出现范围外路径（例如被改过的 manifest）：永不进入计划。
        if !is_managed_relative(Path::new(relative)) {
            continue;
        }
        in_manifest.push(relative.to_string());

        match live.get(relative) {
            None => plan.added.push(PlanItem {
                path: relative.to_string(),
                category: PlanCategory::Added,
                size: file.size,
                snapshot_sha256: Some(file.sha256.clone()),
                live_sha256: None,
            }),
            Some(live_file) if live_file.sha256.as_deref() == Some(file.sha256.as_str()) => {
                unchanged += 1
            }
            Some(live_file) => plan.modified.push(PlanItem {
                path: relative.to_string(),
                category: PlanCategory::Modified,
                size: file.size,
                snapshot_sha256: Some(file.sha256.clone()),
                live_sha256: live_file.sha256.clone(),
            }),
        }
    }

    for (path, live_file) in live {
        if !in_manifest.contains(path) {
            plan.deleted.push(PlanItem {
                path: path.clone(),
                category: PlanCategory::Deleted,
                size: live_file.size,
                snapshot_sha256: None,
                live_sha256: live_file.sha256.clone(),
            });
        }
    }

    for group in [&mut plan.added, &mut plan.modified, &mut plan.deleted] {
        group.sort_by(|left, right| left.path.cmp(&right.path));
    }

    let totals = PlanTotals {
        snapshot_files: manifest_files.len(),
        live_files: live.len(),
        added: plan.added.len(),
        modified: plan.modified.len(),
        deleted: plan.deleted.len(),
        unchanged,
    };
    (plan, totals)
}

/// manifest 路径形如 `snapshot/settings.json`；计划里用相对宿主目录的路径。
fn live_relative(path: &str) -> &str {
    path.strip_prefix(SNAPSHOT_DIR_NAME)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(path)
}

/// 路径必须落在受管范围内：越界组件或排除项（`auth.json` 等）永不进入计划。
///
/// 快照由导出写入，正常情况下全在范围内；这里再挡一次被改过的 manifest，
/// 复用 `scope` 的常量而不是复制规则表。
fn is_managed_relative(relative: &Path) -> bool {
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
        || Path::new(names[0])
            .extension()
            .is_some_and(|extension| extension == "json")
}

fn slash(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// 宿主版本探测：node 用 `node -p process.versions.node`，pi 用 `pi --version`，共用一个 5s 超时。
fn probe_versions(runner: &dyn CommandRunner) -> (Option<Version>, Option<Version>) {
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

fn requirement_checks(
    requirements: &Requirements,
    runner: &dyn CommandRunner,
) -> Vec<RequirementCheck> {
    let (node, pi) = probe_versions(runner);
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

    // 只读取这一个文档化的环境变量；错误信息里不回显其内容。
    let env_agent_dir = std::env::var_os(config::AGENT_DIR_ENV_VAR).map(PathBuf::from);
    let home_dir = app.path().home_dir().ok();
    let resolved =
        config::resolve_agent_dir(&config, env_agent_dir.as_deref(), home_dir.as_deref());
    let agent_dir = resolved.path.ok_or_else(|| {
        PlanFailure::new("agent_dir_missing", "无法确定宿主目录，请先在设置页填写")
    })?;

    plan_snapshot(&agent_dir, &sync_root, &ShellRunner::from_env())
}

/// 子进程会阻塞最多 5 秒，放阻塞线程池，别占住 UI 线程。
#[tauri::command]
pub async fn verify_and_plan(
    app: AppHandle,
    root: Option<String>,
) -> Result<PlanReport, PlanFailure> {
    tauri::async_runtime::spawn_blocking(move || probe(&app, root))
        .await
        .map_err(|error| PlanFailure::new("plan_failed", format!("检查失败：{error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
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
        // 范围外：既不进快照，也永远不进计划。
        write_file(&agent.join("auth.json"), "{}");
        write_file(&agent.join("sessions/session.json"), "{}");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        snapshot::export(&agent, &sync_root, false).expect("导出夹具快照");
        (agent, sync_root)
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
        write_file(&agent.join("auth.json"), "{\"changed\": true}");

        let report = plan_snapshot(&agent, &sync_root, &FakeRunner::ok()).expect("应生成计划");
        assert!(report.plan.added.is_empty(), "{:?}", report.plan);
        assert!(report.plan.modified.is_empty(), "{:?}", report.plan);
        assert!(report.plan.deleted.is_empty(), "{:?}", report.plan);
        assert_eq!(report.totals.unchanged, 3);
        assert_eq!(report.totals.snapshot_files, 3);
        assert_eq!(report.totals.live_files, 3, "范围外文件不参与比较");
        assert_eq!(report.verification.checked, 3);
        assert_eq!(report.manifest_info.files, 3);
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

        let report = plan_snapshot(&agent, &sync_root, &FakeRunner::ok()).expect("应生成计划");

        assert_eq!(paths(&report.plan.added), vec!["skills/antd/SKILL.md"]);
        assert_eq!(paths(&report.plan.modified), vec!["settings.json"]);
        assert_eq!(paths(&report.plan.deleted), vec!["web-search.json"]);
        assert_eq!(report.totals.unchanged, 1, "AGENTS.md 未改动");
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
            plan_snapshot(&agent, &sync_root, &FakeRunner::ok()).expect_err("应报哈希不符");
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
            plan_snapshot(&agent, &sync_root, &FakeRunner::ok()).expect_err("应报哈希不符");
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

        let failure =
            plan_snapshot(&agent, &sync_root, &FakeRunner::ok()).expect_err("25 条不符应中止");
        assert_eq!(failure.code, "hash_mismatch");
        assert_eq!(failure.mismatches.len(), MAX_LISTED_MISMATCHES);
        assert!(
            failure.message.contains("25 个文件不匹配"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn manifest_missing_unsupported_and_corrupt_are_distinguished() {
        let temp = TempDir::new("manifest");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        write_file(&agent.join("settings.json"), "{}");
        fs::create_dir_all(&sync_root).expect("创建同步目录");

        let failure =
            plan_snapshot(&agent, &sync_root, &FakeRunner::ok()).expect_err("缺 manifest 应失败");
        assert_eq!(failure.code, "manifest_missing");

        fs::write(
            sync_root.join(MANIFEST_FILE_NAME),
            "{\"formatVersion\": 99}",
        )
        .expect("写 manifest");
        let failure =
            plan_snapshot(&agent, &sync_root, &FakeRunner::ok()).expect_err("版本不支持应失败");
        assert_eq!(failure.code, "manifest_unsupported");
        assert!(failure.message.contains("99"), "{}", failure.message);

        fs::write(sync_root.join(MANIFEST_FILE_NAME), "not json").expect("写坏 manifest");
        let failure = plan_snapshot(&agent, &sync_root, &FakeRunner::ok()).expect_err("损坏应失败");
        assert_eq!(failure.code, "manifest_corrupt");

        // 合法 JSON 但缺 formatVersion：同样是损坏。
        fs::write(
            sync_root.join(MANIFEST_FILE_NAME),
            "{\"profileId\": \"default\"}",
        )
        .expect("写缺字段 manifest");
        let failure =
            plan_snapshot(&agent, &sync_root, &FakeRunner::ok()).expect_err("缺字段应失败");
        assert_eq!(failure.code, "manifest_corrupt");

        // 版本对了但字段不全：结构损坏，不是"版本不支持"。
        fs::write(
            sync_root.join(MANIFEST_FILE_NAME),
            "{\"formatVersion\": 1, \"profileId\": \"default\"}",
        )
        .expect("写不完整 manifest");
        let failure =
            plan_snapshot(&agent, &sync_root, &FakeRunner::ok()).expect_err("结构不全应失败");
        assert_eq!(failure.code, "manifest_corrupt");
    }

    #[test]
    fn out_of_scope_manifest_entry_never_becomes_a_plan_item() {
        let temp = TempDir::new("scope-guard");
        let (agent, sync_root) = fixture(&temp);

        // 手改 manifest，把范围外的 auth.json 塞进去（哈希与大小都对得上）。
        let extra = sync_root.join("snapshot/auth.json");
        let bytes = b"{\"openai\": {}}";
        fs::write(&extra, bytes).expect("写范围外快照文件");
        let manifest_path = sync_root.join(MANIFEST_FILE_NAME);
        let mut manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&manifest_path).expect("读 manifest"))
                .expect("解析 manifest");
        manifest["files"]
            .as_array_mut()
            .expect("files 数组")
            .push(serde_json::json!({
                "path": "snapshot/auth.json",
                "sha256": snapshot::sha256_hex(bytes),
                "size": bytes.len() as u64,
                "kind": "json",
            }));
        fs::write(&manifest_path, manifest.to_string()).expect("写回 manifest");

        // 计划里不得出现 auth.json：它既不是新增也不参与比较。
        let report = plan_snapshot(&agent, &sync_root, &FakeRunner::ok()).expect("应生成计划");
        assert!(report.plan.added.is_empty(), "{:?}", report.plan.added);
        assert!(report.plan.modified.is_empty());
        assert!(report.plan.deleted.is_empty());
        assert_eq!(report.totals.unchanged, 3);
        assert_eq!(report.totals.snapshot_files, 4);

        // live 侧也有 auth.json，两个方向都不出现在计划里。
        assert!(!paths(&report.plan.deleted)
            .iter()
            .any(|path| path == "auth.json"));
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

        let first = plan_snapshot(&agent, &sync_root, &FakeRunner::ok()).expect("第一次");
        let second = plan_snapshot(&agent, &sync_root, &FakeRunner::ok()).expect("第二次");
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
        let report = plan_snapshot(&agent, &sync_root, &old).expect("版本不满足也要出计划");
        assert_eq!(report.requirements[0].satisfied, Some(false));
        assert_eq!(report.requirements[0].found.as_deref(), Some("22.18.0"));
        assert_eq!(report.requirements[1].satisfied, Some(false));
        assert_eq!(report.totals.unchanged, 3);

        // 探测失败：记未知，不误报为不满足。
        let report = plan_snapshot(&agent, &sync_root, &TimeoutRunner).expect("探测失败也要出计划");
        assert_eq!(report.requirements[0].satisfied, None);
        assert!(report.requirements[0].found.is_none());
        assert_eq!(report.requirements[1].satisfied, None);
    }
}

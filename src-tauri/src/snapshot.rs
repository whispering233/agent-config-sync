//! 快照导出流水线。
//!
//! 流程来源：`docs/design/10-snapshot-sync.md` 第 5 节。
//! 顺序固定为：预检 → 物化到工作区 → 只读密钥扫描 → 打成 zip → 经存储原子放入。
//! 任一步失败都删掉工作区，存储里已存在的包不受影响。
//!
//! 受管范围由 manifest 的 `managedRoots` 决定：`agent` 根占快照根，`agents-skills` 根落在
//! `snapshot/external/agents-skills/`。包内布局仍是 `manifest.json` + `snapshot/`，因此解包后的
//! 暂存目录可以直接交给计划与应用。
//!
//! `export_snapshot` 是 command 层入口；核心逻辑 [`export`] / [`pack_and_store`] 只碰文件系统、
//! 给定的版本号与一个包存储，可直接用临时目录与假存储单测。

use crate::config::{self, AGENT_DIR_ENV_VAR};
use crate::doctor::{self, ShellRunner};
use crate::package;
use crate::platform::{self, HostOs};
use crate::scope::{self, ManagedRoot, ManagedRoots, RootId};
use crate::secrets::{self, SecretFinding, SecretLevel};
use crate::store::{LocalStore, PackageStore, PruneOutcome};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::Manager;

/// 快照目录名（包内的内容前缀，也是解包后的目录名）。
pub const SNAPSHOT_DIR_NAME: &str = "snapshot";
/// manifest 文件名。
pub const MANIFEST_FILE_NAME: &str = "manifest.json";
/// manifest 格式版本。不认识的值一律拒绝读取。
pub const FORMAT_VERSION: u32 = 3;
/// 快照包保留份数（硬编码，不做配置项）：本地文件夹与 WebDAV 共用同一套保留语义。
pub const KEEP_PACKAGES: usize = 20;
/// 快照目录权限：含明文凭据，只允许本用户进入。
const SNAPSHOT_DIR_MODE: u32 = 0o700;
/// 导出工作区权限：中间产物含明文凭据。
const EXPORT_DIR_MODE: u32 = 0o700;
/// 凭据文件权限。
const AUTH_FILE_MODE: u32 = 0o600;
/// 导出工作区名：位于应用数据目录，与 `backups/` 同级。
const EXPORT_WORKSPACE_DIR_NAME: &str = "export-workspace";
/// 工作区里打包用的临时文件名。
const WORKSPACE_ARCHIVE_NAME: &str = "package.zip";
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
    /// 缺字段按空处理：旧 manifest 可能没有这个字段。先让它能反序列化，再由格式版本与
    /// `agent` 根校验给出具体错误，而不是把「字段没有」一律报成 JSON 损坏。
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

/// 一次导出的结果，供 UI 展示。`unchanged` 为真表示存储里最新包的指纹与本机这次要产出的
/// 相同，因此没有写新包：包名、体积与创建时间描述的是那份已存在的包。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportReport {
    pub unchanged: bool,
    pub package_name: String,
    pub size: u64,
    pub files: usize,
    pub fingerprint: String,
    /// RFC 3339 UTC。
    pub created_at: String,
    /// 本次快照声明的受管根。
    pub managed_roots: Vec<RootId>,
    pub required_packages: Vec<RequiredPackage>,
    /// 断链等被跳过的条目数。
    pub skipped: usize,
    /// 只读扫描结果：不阻断，也没有自动改写。
    pub warnings: Vec<SecretFinding>,
    pub plaintext_secrets: bool,
    /// 根级别的告警：例如根是符号链接、外部根是断链。与密钥扫描分开。
    pub root_warnings: Vec<String>,
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

/// 一次打包的产物：写进存储的包名、体积，生成它的 manifest，以及本次的告警。
pub struct StoredPackage {
    pub name: String,
    pub size: u64,
    pub manifest: Manifest,
    pub skipped: usize,
    pub warnings: Vec<SecretFinding>,
    pub root_warnings: Vec<String>,
}

impl StoredPackage {
    /// 本次真的写了一个新包。
    fn into_report(self) -> ExportReport {
        ExportReport {
            unchanged: false,
            package_name: self.name,
            size: self.size,
            files: self.manifest.files.len(),
            fingerprint: self.manifest.fingerprint,
            created_at: self.manifest.created_at,
            managed_roots: self.manifest.managed_roots,
            required_packages: self.manifest.required_packages,
            skipped: self.skipped,
            warnings: self.warnings,
            plaintext_secrets: self.manifest.plaintext_secrets,
            root_warnings: self.root_warnings,
        }
    }
}

/// 测试用夹具：只声明 `agent` 根，把 live 物化成**目录布局**（`manifest.json` + `snapshot/`）。
#[cfg(test)]
pub fn export_fixture(agent_dir: &Path, sync_root: &Path) -> Result<ExportReport, ExportFailure> {
    export_fixture_with(agent_dir, None, sync_root, SourceVersions::default())
}

/// 与 [`export_fixture`] 相同，另外指定 home 与来源版本：多根夹具用它覆盖外部根。
///
/// 计划、应用与回滚的单测校验的是"解包后的暂存目录"，而生产路径只产出包；他们需要一份可直接
/// 读的快照。`home_dir` 为 `None` 时只声明 `agent` 根。
#[cfg(test)]
pub fn export_fixture_with(
    agent_dir: &Path,
    home_dir: Option<&Path>,
    sync_root: &Path,
    versions: SourceVersions,
) -> Result<ExportReport, ExportFailure> {
    let snapshot_dir = sync_root.join(SNAPSHOT_DIR_NAME);
    // 夹具会被反复调用，而物化只写空目录：旧快照整份换掉。
    if snapshot_dir.exists() {
        fs::remove_dir_all(&snapshot_dir).map_err(|error| {
            ExportFailure::new("no_write_permission", format!("无法替换旧快照：{error}"))
        })?;
    }
    fs::create_dir_all(sync_root).map_err(|error| {
        ExportFailure::new(
            "no_write_permission",
            format!("无法创建同步文件夹：{error}"),
        )
    })?;

    let mut root_warnings: Vec<String> = Vec::new();
    let roots = match home_dir {
        Some(home) => detect_roots(agent_dir, Some(home), &mut root_warnings)?,
        None => ManagedRoots::new(vec![ManagedRoot::agent(agent_dir)]),
    };
    let collected = collect_into(&snapshot_dir, &roots, &versions, true)?;
    fs::write(
        sync_root.join(MANIFEST_FILE_NAME),
        manifest_bytes(&collected.manifest)?,
    )
    .map_err(|error| {
        ExportFailure::new(
            "no_write_permission",
            format!("写入 manifest 失败：{error}"),
        )
    })?;

    Ok(ExportReport {
        unchanged: false,
        package_name: package::build_name(now_seconds(), &collected.manifest.fingerprint),
        size: 0,
        files: collected.manifest.files.len(),
        fingerprint: collected.manifest.fingerprint,
        created_at: collected.manifest.created_at,
        managed_roots: collected.manifest.managed_roots,
        required_packages: collected.manifest.required_packages,
        skipped: collected.skipped,
        warnings: collected.warnings,
        plaintext_secrets: collected.manifest.plaintext_secrets,
        root_warnings,
    })
}

/// 导出成包并放进存储根：先物化到本机工作区，打成 zip，再经存储的原子放入定案，
/// 最后按保留份数清理旧包。
///
/// 内容未变（存储里最新包的完整指纹与本机这次要产出的相同）时不写新包，返回那份已存在的包：
/// 空闲期反复导出不该堆历史。返回的清理结果由调用方展示：清理失败不阻塞导出成功。
pub fn export(
    store: &LocalStore,
    agent_dir: &Path,
    home_dir: Option<&Path>,
    app_data: &Path,
    versions: SourceVersions,
) -> Result<(ExportReport, PruneOutcome), ExportFailure> {
    // 本地模式的比对基准要读最新包的 manifest：包就在本机，读它不额外花销。
    let existing = newest_package(store);
    let baseline = existing.as_ref().map(|report| report.fingerprint.as_str());

    let Some(stored) = pack_and_store(
        store,
        baseline,
        agent_dir,
        home_dir,
        app_data,
        Some(store.root()),
        versions,
        true,
    )?
    else {
        // 只有与 `existing` 的指纹相同时才会是 `None`，因此它必然是一份完整报告。
        // 没写新包也就不清理：保留份数只在真正新添一份后才重算。
        let mut report = existing
            .ok_or_else(|| ExportFailure::new("export_failed", "内容比对基准已失效，请重新导出"))?;
        // 内容没变时也要重新探测根告警：根链接/断链的状态可能变了，不能因为跳过写包就把提示吞掉。
        detect_roots(agent_dir, home_dir, &mut report.root_warnings)?;

        return Ok((report, PruneOutcome::default()));
    };

    let report = stored.into_report();
    let pruned = prune_kept(store, &report.package_name);

    Ok((report, pruned))
}

/// 保留清理：单份删不掉只进 `failed`，整个清理动作失败也只是一个失败项，都不改判成导出失败。
fn prune_kept(store: &dyn PackageStore, current: &str) -> PruneOutcome {
    store
        .prune(KEEP_PACKAGES, current)
        .unwrap_or_else(|error| PruneOutcome {
            pruned: Vec::new(),
            failed: vec![(String::new(), error.message)],
        })
}

/// 打包并原子放入存储。`baseline` 是调用方给的「上次已交付内容」的完整指纹：与本次相同就
/// 不写新包，返回 `None`。
///
/// 基准由调用方注入，是因为 WebDAV 模式下最新包在远端：为了比对而下载整包不合算，那里改用
/// 本机记录的 `lastUploadedFingerprint`。
pub fn pack_and_store(
    store: &dyn PackageStore,
    baseline: Option<&str>,
    agent_dir: &Path,
    home_dir: Option<&Path>,
    app_data: &Path,
    // 本地文件夹模式的存储根；WebDAV 模式的存储在远端，没有本地路径要校验。
    store_root: Option<&Path>,
    versions: SourceVersions,
    include_auth: bool,
) -> Result<Option<StoredPackage>, ExportFailure> {
    let workspace = prepare_workspace(app_data)?;
    let result = pack_into_store(
        store,
        baseline,
        &workspace,
        agent_dir,
        home_dir,
        store_root,
        versions,
        include_auth,
    );
    // 工作区只是本次导出的中间产物：内容已经在包里，成功失败都不留。
    let _ = fs::remove_dir_all(&workspace);
    result
}

fn pack_into_store(
    store: &dyn PackageStore,
    baseline: Option<&str>,
    workspace: &Path,
    agent_dir: &Path,
    home_dir: Option<&Path>,
    store_root: Option<&Path>,
    versions: SourceVersions,
    include_auth: bool,
) -> Result<Option<StoredPackage>, ExportFailure> {
    let mut root_warnings: Vec<String> = Vec::new();
    let roots = detect_roots(agent_dir, home_dir, &mut root_warnings)?;
    precheck(&roots, store_root)?;

    let snapshot_dir = workspace.join(SNAPSHOT_DIR_NAME);
    let collected = collect_into(&snapshot_dir, &roots, &versions, include_auth)?;
    let manifest = collected.manifest;

    if baseline.is_some_and(|fingerprint| fingerprint == manifest.fingerprint) {
        return Ok(None);
    }

    // 打包前自检一次：manifest 与快照内容是分头产出的，错位时宁可不落包。
    verify_files(workspace, &manifest.files)?;
    let archive = workspace.join(WORKSPACE_ARCHIVE_NAME);
    package::pack(&archive, &manifest_bytes(&manifest)?, &snapshot_dir)
        .map_err(|error| ExportFailure::new(error.code, error.message))?;
    let bytes = fs::read(&archive).map_err(|error| {
        ExportFailure::new("export_failed", format!("读取刚打好的快照包失败：{error}"))
    })?;

    let name = package::build_name(now_seconds(), &manifest.fingerprint);
    store
        .put(&name, &bytes)
        .map_err(|error| ExportFailure::new(error.code, error.message))?;

    Ok(Some(StoredPackage {
        name,
        size: bytes.len() as u64,
        manifest,
        skipped: collected.skipped,
        warnings: collected.warnings,
        root_warnings,
    }))
}

/// 存储里名字最大的可解析包，按「内容未变」的报告形状返回。名字大小只用来挑默认项，
/// 不当安全判定。
///
/// 读不出来（例如云盘还没传完的半包）就当没有可比对的基准：让这次导出照常写新包，
/// 坏包留给用户显式清理，而不是让一份残留把导出永久卡住。
fn newest_package(store: &LocalStore) -> Option<ExportReport> {
    let listing = store.list().ok()?;
    let newest = listing
        .packages
        .into_iter()
        .max_by(|left, right| left.name.cmp(&right.name))?;
    let bytes = package::read_manifest(&store.root().join(&newest.name)).ok()?;
    let manifest = parse_manifest(&bytes).ok()?;

    Some(ExportReport {
        unchanged: true,
        package_name: newest.name,
        size: newest.size,
        files: manifest.files.len(),
        fingerprint: manifest.fingerprint,
        created_at: manifest.created_at,
        managed_roots: manifest.managed_roots,
        required_packages: manifest.required_packages,
        skipped: 0,
        warnings: Vec::new(),
        plaintext_secrets: manifest.plaintext_secrets,
        root_warnings: Vec::new(),
    })
}

/// 解析包里的 manifest。版本单独先看：结构不认识时要报「版本不支持」而不是「文件损坏」。
pub(crate) fn parse_manifest(bytes: &[u8]) -> Result<Manifest, ExportFailure> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
        ExportFailure::new(
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
            return Err(ExportFailure::new(
                "manifest_unsupported",
                format!("manifest 的 formatVersion 是 {version}，本应用只支持 {FORMAT_VERSION}"),
            ))
        }
        None => {
            return Err(ExportFailure::new(
                "manifest_corrupt",
                "manifest 缺少 formatVersion 字段",
            ))
        }
    }

    serde_json::from_value(value).map_err(|error| {
        ExportFailure::new("manifest_corrupt", format!("manifest 字段不完整：{error}"))
    })
}

fn manifest_bytes(manifest: &Manifest) -> Result<Vec<u8>, ExportFailure> {
    let mut text = serde_json::to_string_pretty(manifest).map_err(|error| {
        ExportFailure::new("export_failed", format!("manifest 序列化失败：{error}"))
    })?;
    text.push('\n');

    Ok(text.into_bytes())
}

/// 导出工作区：只活在一次导出期间。不能放在存储根里——那里只应出现包，
/// 混进临时目录会被用户看成「无法识别的文件」。
fn prepare_workspace(app_data: &Path) -> Result<PathBuf, ExportFailure> {
    let root = app_data.join(EXPORT_WORKSPACE_DIR_NAME);
    // 只有上次进程被杀才会留下它；同一时刻只允许一次导出。
    if root.exists() {
        fs::remove_dir_all(&root).map_err(|error| {
            ExportFailure::new(
                "no_write_permission",
                format!("无法清理残留的导出工作区：{error}"),
            )
        })?;
    }
    platform::create_dir_all_mode(&root, EXPORT_DIR_MODE).map_err(|error| {
        ExportFailure::new(
            "no_write_permission",
            format!("无法创建导出工作区：{error}"),
        )
    })?;

    Ok(root)
}

/// 一次采集的产物：manifest 与需要展示给用户的告警。
struct CollectedSnapshot {
    manifest: Manifest,
    skipped: usize,
    warnings: Vec<SecretFinding>,
}

/// 按受管根物化到 `snapshot_dir`，扫描明文密钥，再按目录内容生成 manifest。
fn collect_into(
    snapshot_dir: &Path,
    roots: &ManagedRoots,
    versions: &SourceVersions,
    include_auth: bool,
) -> Result<CollectedSnapshot, ExportFailure> {
    // 每个根物化到它在快照里的位置：agent 根占快照根，外部根落在 `external/<id>/`。
    let mut skipped = 0;
    let mut symlinks: Vec<(RootId, PathBuf, PathBuf)> = Vec::new();
    for root in roots.iter() {
        let dest = snapshot_root_dir(snapshot_dir, root.id);
        let materialized = scope::materialize_root(root, &dest, include_auth).map_err(|error| {
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

    // 快照目录含明文凭据：目录只给本用户，agent 根级的 auth.json 再收紧到 0600。
    platform::set_mode(snapshot_dir, SNAPSHOT_DIR_MODE).map_err(|error| {
        ExportFailure::new(
            "no_write_permission",
            format!("设置快照目录权限失败：{error}"),
        )
    })?;
    let auth_path = snapshot_dir.join("auth.json");
    if auth_path.is_file() {
        platform::set_mode(&auth_path, AUTH_FILE_MODE).map_err(|error| {
            ExportFailure::new(
                "no_write_permission",
                format!("设置 auth.json 权限失败：{error}"),
            )
        })?;
    }

    // 扫描只读：命中只用于告警，不改写快照内容。
    let warnings = secrets::scan_snapshot(snapshot_dir)
        .map_err(|error| ExportFailure::new("export_failed", format!("密钥扫描失败：{error}")))?;
    let has_high = warnings
        .iter()
        .any(|finding| finding.level == SecretLevel::High);
    let plaintext_secrets = auth_path.is_file() || has_high;

    let collected = collect_snapshot_files(roots, snapshot_dir).map_err(|error| {
        ExportFailure::new("export_failed", format!("读取临时快照失败：{error}"))
    })?;
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
            pi: versions.pi.clone(),
            node: versions.node.clone(),
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
        required_packages: declared_packages(snapshot_dir),
        plaintext_secrets,
    };

    Ok(CollectedSnapshot {
        manifest,
        skipped,
        warnings,
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

/// 预检：受管根可用；本地文件夹模式下存储根路径还要规范，且不与任何受管根互相嵌套。
///
/// 存储根不要求预先存在：本地实现在第一次写入时按 0700 建出来。
fn precheck(roots: &ManagedRoots, store_root: Option<&Path>) -> Result<(), ExportFailure> {
    if let Some(store_root) = store_root {
        if !is_plain_absolute(store_root) {
            return Err(ExportFailure::new(
                "sync_root_invalid",
                "同步文件夹必须是绝对路径且不含 ..，请在设置页重新选择",
            ));
        }
    }
    let agent_dir = roots.path(RootId::Agent).ok_or_else(|| {
        ExportFailure::new("agent_dir_missing", "无法确定宿主目录，请先在设置页填写")
    })?;
    if !agent_dir.is_dir() {
        return Err(ExportFailure::new(
            "agent_dir_missing",
            "宿主目录不存在或不是文件夹，请在设置页确认",
        ));
    }
    let Some(store_root) = store_root else {
        return Ok(());
    };

    // 规范化后比前缀：配置校验用的同一套判定，这里不重复实现规则。存储根本身可能还不存在，
    // 那时只能按字面路径比。
    let canonical_sync = fs::canonicalize(store_root).ok();
    for root in roots.iter() {
        let canonical_root = fs::canonicalize(&root.path).map_err(|error| {
            let code = match root.id {
                RootId::Agent => "agent_dir_missing",
                RootId::AgentsSkills => "external_root_unreadable",
            };
            ExportFailure::new(code, format!("无法访问 {}：{error}", root.path.display()))
        })?;
        let overlap = match &canonical_sync {
            Some(canonical_sync) => {
                canonical_sync.starts_with(&canonical_root)
                    || canonical_root.starts_with(canonical_sync)
            }
            None => config::overlap_message(store_root, &root.path).is_some(),
        };
        if overlap {
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
        hasher.update(b"\n");
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

pub(crate) fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// RFC 3339 UTC（秒级）。只用 std：chrono 不在允许新增的依赖里。
/// 备份与暂存目录的命名也用它，不另写第二套日历换算。
pub(crate) fn format_timestamp(unix_seconds: u64) -> String {
    let (year, month, day, hour, minute, second) = utc_parts(unix_seconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// `yyyymmdd-HHmmss` UTC：包名用的紧凑形态，与 [`format_timestamp`] 共用同一套换算。
pub(crate) fn format_compact_stamp(unix_seconds: u64) -> String {
    let (year, month, day, hour, minute, second) = utc_parts(unix_seconds);
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

fn utc_parts(unix_seconds: u64) -> (i64, u64, u64, u64, u64, u64) {
    let days = (unix_seconds / 86_400) as i64;
    let seconds_of_day = unix_seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60,
    )
}

/// [`days_from_civil`] 的逆运算：包名要回读日期，同样只此一套日历换算。
pub(crate) fn days_from_civil(year: i64, month: u64, day: u64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = (year - era * 400) as u64;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era as i64 - 719_468
}

/// Howard Hinnant 的 civil_from_days：从 epoch 天数换回年月日。
pub(crate) fn civil_from_days(days: i64) -> (i64, u64, u64) {
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

/// command 层入口：从应用配置读取存储根与宿主目录，再跑导出。
///
/// 导出一次与「上传」是同一次动作：清理已经真的执行了，这里只丢掉清理明细，列表刷新会显示结果。
#[tauri::command]
pub async fn export_snapshot(app: tauri::AppHandle) -> Result<ExportReport, ExportFailure> {
    // 版本探测要起子进程，最多各等 5 秒；放阻塞线程池，别占住 UI 线程。
    tauri::async_runtime::spawn_blocking(move || {
        export_to_local_store(&app).map(|(report, _)| report)
    })
    .await
    .map_err(|error| ExportFailure::new("export_failed", format!("导出失败：{error}")))?
}

/// 一次导出需要的本机上下文：应用数据目录（工作区与状态文件）、宿主目录、home 与来源版本。
pub struct ExportContext {
    pub app_data: PathBuf,
    pub agent_dir: PathBuf,
    /// 解析 `~/.agents/skills` 用；解析不到时为空，导出会按 `home_dir_missing` 失败。
    pub home_dir: Option<PathBuf>,
    pub versions: SourceVersions,
}

/// 解析导出上下文。宿主目录与 home 决定采集范围，版本只作来源记录。
pub fn export_context(app: &tauri::AppHandle) -> Result<ExportContext, ExportFailure> {
    let config_dir = app.path().app_config_dir().map_err(|error| {
        ExportFailure::new(
            "app_config_unavailable",
            format!("无法定位应用配置目录：{error}"),
        )
    })?;
    let config = config::load_config(&config_dir)
        .map_err(|error| ExportFailure::new("app_config_unavailable", error.to_string()))?;
    let app_data = app.path().app_data_dir().map_err(|error| {
        ExportFailure::new(
            "app_data_unavailable",
            format!("无法定位应用数据目录：{error}"),
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

    Ok(ExportContext {
        app_data,
        agent_dir,
        home_dir,
        versions: SourceVersions { pi, node },
    })
}

/// 本地文件夹模式的上传与导出共用这一条链路：两者都是「把 live 导出成包放进存储根」。
/// 返回导出结果与保留清理结果：两个入口对清理的展示方式不同。
pub fn export_to_local_store(
    app: &tauri::AppHandle,
) -> Result<(ExportReport, PruneOutcome), ExportFailure> {
    // 与自动上行、应用/回滚共用一把锁：导出会重建工作区，不能两份同时跑。
    let _guard = crate::runtime::lock_operation();
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
    let context = export_context(app)?;

    export(
        &LocalStore::new(sync_root),
        &context.agent_dir,
        context.home_dir.as_deref(),
        &context.app_data,
        context.versions,
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

    /// `~/.agents/skills` 夹具：与 agent 目录平铺在同一个临时 home 下。
    fn fixture_home(temp: &TempDir, fill: impl FnOnce(&Path)) -> PathBuf {
        let home = temp.path().join("home");
        let skills = home.join(".agents/skills");
        fs::create_dir_all(&skills).expect("创建外部根");
        fill(&skills);
        home
    }

    /// `export_fixture` 产出的是目录布局，计划与应用的单测直接读它。
    fn read_fixture_manifest(sync_root: &Path) -> Manifest {
        let text = fs::read_to_string(sync_root.join(MANIFEST_FILE_NAME)).expect("读取 manifest");
        serde_json::from_str(&text).expect("解析 manifest")
    }

    fn package_manifest(store_root: &Path, name: &str) -> Manifest {
        let bytes = package::read_manifest(&store_root.join(name)).expect("读包内 manifest");
        parse_manifest(&bytes).expect("解析 manifest")
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
    fn export_writes_one_package_and_reports_it() {
        let temp = TempDir::new("export");
        let agent = temp.path().join("agent");
        let store_root = temp.path().join("cloud");
        let app_data = temp.path().join("data");
        fixture_agent(&agent);
        fs::create_dir_all(&app_data).expect("创建应用数据目录");
        let home = home_without_skills(&temp);

        let (report, pruned) = export(
            &LocalStore::new(store_root.clone()),
            &agent,
            Some(&home),
            &app_data,
            SourceVersions {
                pi: Some("1.6.0".to_string()),
                node: Some("24.14.0".to_string()),
            },
        )
        .expect("导出应成功");

        assert!(pruned.pruned.is_empty(), "只有一份包：没有可清理的");

        assert!(!report.unchanged, "{:?}", report);
        assert_eq!(report.files, 6);
        assert_eq!(report.managed_roots, vec![RootId::Agent]);
        assert_eq!(report.fingerprint.len(), 12);
        assert!(report.plaintext_secrets, "auth.json 与字面密钥都应在快照里");
        let parsed = package::parse_name(&report.package_name).expect("包名必须可解析");
        assert_eq!(parsed.fingerprint8, report.fingerprint[..8]);
        assert_eq!(report.created_at.len(), 20);
        assert!(report.created_at.ends_with('Z'));

        // 存储根里只有这一个包：没有临时名，也没有旧的目录布局残留。
        assert_eq!(dir_entries(&store_root), vec![report.package_name.clone()]);
        assert_eq!(
            report.size,
            fs::metadata(store_root.join(&report.package_name))
                .expect("包元数据")
                .len()
        );

        let manifest = package_manifest(&store_root, &report.package_name);
        assert_eq!(manifest.format_version, FORMAT_VERSION);
        assert_eq!(manifest.profile_id, PROFILE_ID);
        assert_eq!(manifest.fingerprint, report.fingerprint);
        assert_eq!(manifest.created_at, report.created_at);
        assert_eq!(manifest.source.app, env!("CARGO_PKG_VERSION"));
        assert_eq!(manifest.source.os, std::env::consts::OS);
        assert_eq!(manifest.source.pi.as_deref(), Some("1.6.0"));
        assert_eq!(manifest.source.node.as_deref(), Some("24.14.0"));
        assert_eq!(manifest.requirements.node, REQUIREMENT_NODE);
        assert!(manifest.symlinks.is_empty());
        assert_eq!(manifest.managed_roots, vec![RootId::Agent]);

        // manifest 的文件表与包内容一致：路径、大小、哈希都能复算。
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
        assert_eq!(
            fingerprint(FORMAT_VERSION, &manifest.managed_roots, &manifest.files),
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

        // 解包后与 live 逐字节一致：字面密钥原样保留、auth.json 在、sessions 不在。
        let dest = temp.path().join("unpacked");
        let unpacked =
            package::unpack(&store_root.join(&report.package_name), &dest).expect("解包");
        assert_eq!(unpacked.files, manifest.files.len() + 1, "含 manifest.json");
        let models = fs::read_to_string(dest.join("snapshot/models.json")).expect("读快照文件");
        assert!(models.contains(SECRET), "{models}");
        assert!(!models.contains("$PI_OPENROUTER_API_KEY"), "{models}");
        assert!(dest.join("snapshot/auth.json").is_file());
        assert!(!dest.join("snapshot/sessions").exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let auth_mode = fs::metadata(dest.join("snapshot/auth.json"))
                .expect("auth 元数据")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(auth_mode, 0o600, "包里的 auth.json 必须是 0600");
        }

        // 导出工作区不留在应用数据目录里。
        assert!(!app_data.join(EXPORT_WORKSPACE_DIR_NAME).exists());
    }

    #[test]
    fn unchanged_content_does_not_add_a_second_package() {
        let temp = TempDir::new("unchanged");
        let agent = temp.path().join("agent");
        let store_root = temp.path().join("cloud");
        let app_data = temp.path().join("data");
        fixture_agent(&agent);
        let home = home_without_skills(&temp);

        let store = LocalStore::new(store_root.clone());
        let (first, _) = export(
            &store,
            &agent,
            Some(&home),
            &app_data,
            SourceVersions::default(),
        )
        .expect("首次导出");
        let (second, _) = export(
            &store,
            &agent,
            Some(&home),
            &app_data,
            SourceVersions::default(),
        )
        .expect("再次导出");

        assert!(!first.unchanged);
        assert!(second.unchanged, "{:?}", second);
        assert_eq!(second.package_name, first.package_name);
        assert_eq!(second.fingerprint, first.fingerprint);
        assert_eq!(second.size, first.size);
        assert_eq!(second.files, first.files);
        assert_eq!(second.created_at, first.created_at);
        assert_eq!(second.managed_roots, first.managed_roots);
        assert_eq!(dir_entries(&store_root), vec![first.package_name]);
    }

    #[test]
    fn changed_content_adds_another_package() {
        let temp = TempDir::new("changed");
        let agent = temp.path().join("agent");
        let store_root = temp.path().join("cloud");
        let app_data = temp.path().join("data");
        fixture_agent(&agent);
        let home = home_without_skills(&temp);
        let store = LocalStore::new(store_root.clone());

        let first = export(
            &store,
            &agent,
            Some(&home),
            &app_data,
            SourceVersions::default(),
        )
        .expect("首次导出")
        .0;
        write_file(
            &agent.join("extensions/leak/index.ts"),
            &format!("const apiKey = \"sk-{SECRET}\";\n"),
        );
        let second = export(
            &store,
            &agent,
            Some(&home),
            &app_data,
            SourceVersions::default(),
        )
        .expect("再次导出")
        .0;

        assert!(!second.unchanged);
        assert_ne!(second.package_name, first.package_name);
        assert!(second.plaintext_secrets);
        let names = dir_entries(&store_root);
        assert_eq!(names.len(), 2, "{names:?}");
        for name in names {
            assert!(package::parse_name(&name).is_some(), "{name}");
        }
        let manifest = package_manifest(&store_root, &second.package_name);
        assert!(manifest
            .files
            .iter()
            .any(|file| file.path == "snapshot/extensions/leak/index.ts"));
    }

    /// 旧包的夹具名：只有名字参与保留判定，内容不是真包也行。
    fn old_package_name(index: u64) -> String {
        format!(
            "20250101-{:02}{:02}{:02}-deadbeef.zip",
            index / 3600,
            (index % 3600) / 60,
            index % 60
        )
    }

    #[test]
    fn export_prunes_the_store_to_the_keep_limit() {
        let temp = TempDir::new("export-prune");
        let agent = temp.path().join("agent");
        let store_root = temp.path().join("cloud");
        let app_data = temp.path().join("data");
        fixture_agent(&agent);
        let home = home_without_skills(&temp);
        for index in 0..24 {
            write_file(&store_root.join(old_package_name(index)), "old");
        }

        let (report, pruned) = export(
            &LocalStore::new(store_root.clone()),
            &agent,
            Some(&home),
            &app_data,
            SourceVersions::default(),
        )
        .expect("导出应成功");

        assert_eq!(
            pruned.pruned.len(),
            5,
            "24 份旧包 + 本次 1 份，留 20 删最旧 5 份"
        );
        assert!(pruned.failed.is_empty(), "{:?}", pruned);
        let names = dir_entries(&store_root);
        assert_eq!(names.len(), KEEP_PACKAGES, "{names:?}");
        assert!(names.contains(&report.package_name));
        assert!(!names.contains(&old_package_name(0)), "删的是最旧的 5 份");
    }

    #[test]
    fn prune_failure_does_not_fail_the_export() {
        let temp = TempDir::new("export-prune-failure");
        let agent = temp.path().join("agent");
        let store_root = temp.path().join("cloud");
        let app_data = temp.path().join("data");
        fixture_agent(&agent);
        let home = home_without_skills(&temp);
        // 名字像包、实际是目录：删不掉，于是清理失败但不阻塞导出。
        let stuck = old_package_name(0);
        fs::create_dir_all(store_root.join(&stuck)).expect("创建删不掉的包");
        for index in 1..24 {
            write_file(&store_root.join(old_package_name(index)), "old");
        }

        let (report, pruned) = export(
            &LocalStore::new(store_root.clone()),
            &agent,
            Some(&home),
            &app_data,
            SourceVersions::default(),
        )
        .expect("清理失败不阻塞导出");

        assert_eq!(pruned.pruned.len(), 4);
        assert_eq!(pruned.failed.len(), 1);
        assert_eq!(pruned.failed[0].0, stuck);
        assert!(!pruned.failed[0].1.is_empty());
        assert!(store_root.join(&stuck).is_dir(), "删不掉的那份不动");
        assert!(store_root.join(&report.package_name).is_file());
    }

    #[test]
    fn precheck_rejects_overlaps_and_bad_paths_but_not_a_missing_store_root() {
        let temp = TempDir::new("precheck");
        let agent = temp.path().join("agent");
        let app_data = temp.path().join("data");
        fixture_agent(&agent);
        let home = home_without_skills(&temp);

        let inside = agent.join("cloud");
        let failure = export(
            &LocalStore::new(inside),
            &agent,
            Some(&home),
            &app_data,
            SourceVersions::default(),
        )
        .expect_err("嵌套应被拒绝");
        assert_eq!(failure.code, "sync_root_overlaps_agent_dir");

        let failure = export(
            &LocalStore::new(PathBuf::from("relative/path")),
            &agent,
            Some(&home),
            &app_data,
            SourceVersions::default(),
        )
        .expect_err("相对路径应被拒绝");
        assert_eq!(failure.code, "sync_root_invalid");

        let missing_agent = temp.path().join("missing-agent");
        let failure = export(
            &LocalStore::new(temp.path().join("cloud")),
            &missing_agent,
            Some(&home),
            &app_data,
            SourceVersions::default(),
        )
        .expect_err("宿主目录缺失应被拒绝");
        assert_eq!(failure.code, "agent_dir_missing");

        // 存储根不存在不算错：本地实现按 0700 建出来。
        let fresh = temp.path().join("fresh-store");
        assert!(!fresh.exists());
        export(
            &LocalStore::new(fresh.clone()),
            &agent,
            Some(&home),
            &app_data,
            SourceVersions::default(),
        )
        .expect("存储根缺失应照常导出");
        assert!(fresh.is_dir());
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
        export_fixture_with(&agent, None, &sync_root, SourceVersions::default())
            .expect("导出夹具快照");
        let manifest = read_fixture_manifest(&sync_root);

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
    fn export_records_probed_versions_and_leaves_them_null_when_unknown() {
        let temp = TempDir::new("versions");
        let agent = temp.path().join("agent");
        let store_root = temp.path().join("cloud");
        let app_data = temp.path().join("data");
        fixture_agent(&agent);
        let home = home_without_skills(&temp);
        let store = LocalStore::new(store_root.clone());

        let versions = SourceVersions {
            pi: Some("1.6.0".to_string()),
            node: Some("24.14.0".to_string()),
        };
        let report = export(&store, &agent, Some(&home), &app_data, versions)
            .expect("导出应成功")
            .0;
        let manifest = package_manifest(&store_root, &report.package_name);
        assert_eq!(manifest.source.pi.as_deref(), Some("1.6.0"));
        assert_eq!(manifest.source.node.as_deref(), Some("24.14.0"));

        // 探测不到时留空：导出照常进行，manifest 里是 null。
        export_fixture_with(&agent, Some(&home), &store_root, SourceVersions::default())
            .expect("导出应成功");
        let manifest = read_fixture_manifest(&store_root);
        assert_eq!(manifest.source.pi, None);
        assert_eq!(manifest.source.node, None);
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

        let report =
            export_fixture_with(&agent, Some(&home), &sync_root, SourceVersions::default())
                .expect("导出应成功");
        assert_eq!(
            report.managed_roots,
            vec![RootId::Agent, RootId::AgentsSkills]
        );

        let manifest = read_fixture_manifest(&sync_root);
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
        let plain = export_fixture_with(&agent, None, &agent_only_root, SourceVersions::default())
            .expect("导出 agent-only 快照");
        assert_eq!(plain.managed_roots, vec![RootId::Agent]);
        assert_ne!(plain.fingerprint, report.fingerprint);
    }

    #[test]
    fn package_carries_the_second_root_under_external() {
        let temp = TempDir::new("external-package");
        let agent = temp.path().join("agent");
        let store_root = temp.path().join("cloud");
        let app_data = temp.path().join("data");
        fixture_agent(&agent);
        let home = fixture_home(&temp, |skills| {
            write_file(&skills.join("antd/SKILL.md"), "# 共享技能\n");
        });

        let (report, _) = export(
            &LocalStore::new(store_root.clone()),
            &agent,
            Some(&home),
            &app_data,
            SourceVersions::default(),
        )
        .expect("导出应成功");
        assert_eq!(
            report.managed_roots,
            vec![RootId::Agent, RootId::AgentsSkills]
        );

        let dest = temp.path().join("unpacked");
        package::unpack(&store_root.join(&report.package_name), &dest).expect("解包");
        assert_eq!(
            fs::read_to_string(dest.join("snapshot/external/agents-skills/antd/SKILL.md"))
                .expect("读取包内外部根文件"),
            "# 共享技能\n"
        );
        let manifest = package_manifest(&store_root, &report.package_name);
        assert_eq!(
            manifest.managed_roots,
            vec![RootId::Agent, RootId::AgentsSkills]
        );
    }

    #[cfg(unix)]
    #[test]
    fn package_skips_windows_zone_identifier_metadata() {
        let temp = TempDir::new("zone-identifier");
        let agent = temp.path().join("agent");
        let store_root = temp.path().join("cloud");
        let app_data = temp.path().join("data");
        fixture_agent(&agent);
        let home = fixture_home(&temp, |skills| {
            write_file(&skills.join("x-components/SKILL.md"), "# x-components\n");
            write_file(
                &skills.join("x-components/SKILL.md:Zone.Identifier"),
                "[ZoneTransfer]\nZoneId=3\n",
            );
        });

        let (report, _) = export(
            &LocalStore::new(store_root.clone()),
            &agent,
            Some(&home),
            &app_data,
            SourceVersions::default(),
        )
        .expect("导出应成功");
        assert!(report.skipped >= 1, "Zone.Identifier 应计入跳过条目");

        let dest = temp.path().join("unpacked");
        package::unpack(&store_root.join(&report.package_name), &dest)
            .expect("含 WSL 元数据时导出的包必须能解包");
        assert!(dest
            .join("snapshot/external/agents-skills/x-components/SKILL.md")
            .is_file());
        assert!(!dest
            .join("snapshot/external/agents-skills/x-components/SKILL.md:Zone.Identifier")
            .exists());

        let manifest = package_manifest(&store_root, &report.package_name);
        assert!(!manifest
            .files
            .iter()
            .any(|file| file.path.ends_with(":Zone.Identifier")));
    }

    #[test]
    fn empty_agents_skills_dir_is_still_declared() {
        let temp = TempDir::new("external-empty");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fixture_agent(&agent);
        let home = fixture_home(&temp, |_| {});

        let report =
            export_fixture_with(&agent, Some(&home), &sync_root, SourceVersions::default())
                .expect("导出应成功");
        assert_eq!(
            report.managed_roots,
            vec![RootId::Agent, RootId::AgentsSkills]
        );
        // 空目录也要在快照里出现，目标机才能把多出的技能列为删除候选。
        assert!(sync_root.join("snapshot/external/agents-skills").is_dir());
        assert!(read_fixture_manifest(&sync_root)
            .managed_roots
            .contains(&RootId::AgentsSkills));
    }

    #[test]
    fn missing_agents_skills_dir_produces_no_external_paths() {
        let temp = TempDir::new("external-missing");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fixture_agent(&agent);
        let home = home_without_skills(&temp);

        let report =
            export_fixture_with(&agent, Some(&home), &sync_root, SourceVersions::default())
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

        let failure =
            export_fixture_with(&agent, Some(&home), &sync_root, SourceVersions::default())
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

        let report =
            export_fixture_with(&agent, Some(&home), &sync_root, SourceVersions::default())
                .expect("导出应成功");
        // 根的类型不进快照：只跟内容，不记录根链接。
        assert_eq!(
            read_fixture_manifest(&sync_root).managed_roots,
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

        let report =
            export_fixture_with(&agent, Some(&home), &sync_root, SourceVersions::default())
                .expect("断链不应导致导出失败");
        assert_eq!(report.managed_roots, vec![RootId::Agent]);
        assert_eq!(report.root_warnings.len(), 1, "{:?}", report.root_warnings);
        assert!(report.root_warnings[0].contains("断链"));
        assert!(!sync_root.join("snapshot/external").exists());
    }

    #[test]
    fn store_root_nested_with_agents_skills_is_rejected() {
        let temp = TempDir::new("external-nested");
        let agent = temp.path().join("agent");
        let app_data = temp.path().join("data");
        fixture_agent(&agent);
        let home = fixture_home(&temp, |_| {});
        let inside = home.join(".agents/skills/cloud");

        let failure = export(
            &LocalStore::new(inside),
            &agent,
            Some(&home),
            &app_data,
            SourceVersions::default(),
        )
        .expect_err("嵌套应被拒绝");
        assert_eq!(failure.code, "sync_root_overlaps_external_root");
    }

    #[test]
    fn export_without_a_home_fails_instead_of_falling_back_to_agent_only() {
        let temp = TempDir::new("no-home");
        let agent = temp.path().join("agent");
        let store_root = temp.path().join("cloud");
        let app_data = temp.path().join("data");
        fixture_agent(&agent);

        let failure = export(
            &LocalStore::new(store_root.clone()),
            &agent,
            None,
            &app_data,
            SourceVersions::default(),
        )
        .expect_err("解析不到 home 应导出失败");
        assert_eq!(failure.code, "home_dir_missing");
        let landed = store_root.is_dir() && !dir_entries(&store_root).is_empty();
        assert!(!landed, "失败不落包");
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

        let report =
            export_fixture_with(&agent, Some(&home), &sync_root, SourceVersions::default())
                .expect("导出应成功");
        // 根的类型不进快照：内容跟随链接读出来，但也要告警让人知道写入会落到链接目标。
        assert_eq!(report.root_warnings.len(), 1, "{:?}", report.root_warnings);
        assert!(report.root_warnings[0].contains("符号链接"));
        assert!(sync_root.join("snapshot/settings.json").is_file());
        assert!(
            read_fixture_manifest(&sync_root).symlinks.is_empty(),
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
        // v2 快照没有 `managedRoots`。缺省是为了让旧 manifest 能单独读取元数据；
        // 是否可用仍由 `formatVersion` 与 `agent` 根校验决定。
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

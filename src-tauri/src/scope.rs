//! 受管范围与遍历。
//!
//! 规则来源：`docs/design/10-snapshot-sync.md` 的「受管范围」「符号链接」两节。
//! 范围判定集中在下面的常量与 [`is_excluded_name`] / [`resolve_within`]，
//! 快照、计划与落地都复用它们，不另写一套。

use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::platform;

/// 根级固定纳入的普通文件。
pub const INCLUDED_ROOT_FILES: &[&str] = &[
    "AGENTS.md",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
    "keybindings.json",
    "auth.json",
];

/// 根级纳入的资源目录，进入后整棵子树纳入。
pub const INCLUDED_ROOT_DIRS: &[&str] = &["skills", "extensions", "prompts", "themes", "agents"];

/// 任何层级都排除的名称：状态、缓存、安装与包本体。
/// 根级 `auth.json` 是例外，由 [`is_excluded_relative`] 只放行根级。
pub const EXCLUDED_NAMES: &[&str] = &[
    "models-store.json",
    "trust.json",
    "web-push.json",
    "pi-web-session-state.json",
    "pi-web-session-index.json",
    "sessions",
    "install",
    "npm",
    "fff",
    "web-search-cache",
    "missions",
    "pi-subagents",
    "pi-web-access",
    "pi-web-run",
    "run-history.jsonl",
];

/// 任何层级都排除的名称前缀。
pub const EXCLUDED_NAME_PREFIXES: &[&str] = &["pi-web-session-"];

/// 排除判定：按名称匹配，与体量无关。
pub fn is_excluded_name(name: &str) -> bool {
    EXCLUDED_NAMES.contains(&name)
        || EXCLUDED_NAME_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// 按相对路径排除：普通排除名按任意层级匹配；`auth.json` 只允许根级。
pub fn is_excluded_relative(relative: &Path) -> bool {
    let nested_auth = relative.components().count() > 1
        && relative.file_name().is_some_and(|name| name == "auth.json");
    nested_auth
        || relative.components().any(|component| match component {
            Component::Normal(name) => is_excluded_name(&name.to_string_lossy()),
            _ => true,
        })
}

/// 根级纳入判定：白名单文件、白名单目录、其余根级 `*.json`。
fn is_included_root_name(name: &str) -> bool {
    INCLUDED_ROOT_FILES.contains(&name)
        || INCLUDED_ROOT_DIRS.contains(&name)
        || Path::new(name).extension().is_some_and(|ext| ext == "json")
}

/// 条目类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
}

/// 受管范围内的一个条目。目录条目用于在目标目录里重建空目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 相对 agent 目录的路径，永不含 `..` 或绝对前缀。
    pub relative_path: PathBuf,
    pub kind: EntryKind,
    /// 文件字节数，目录为 0。
    pub size: u64,
    /// 符号链接的原始目标；普通条目为 `None`。内容按物化处理。
    pub link_target: Option<PathBuf>,
}

/// 跳过原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 断链：目标不存在。
    BrokenSymlink,
    /// 元数据不可读或目录不可读。
    Unreadable,
    /// socket / fifo / 设备等特殊文件。
    SpecialFile,
    /// 链接成环，重复进入同一物理目录。
    LinkLoop,
}

/// 被跳过的条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub relative_path: PathBuf,
    pub reason: SkipReason,
}

/// 扫描结果。`entries` 与 `skipped` 均按 `relative_path` 升序，与 readdir 顺序无关。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanOutcome {
    pub entries: Vec<Entry>,
    pub skipped: Vec<Skipped>,
}

impl ScanOutcome {
    /// manifest 的 `symlinks`：被物化的链接及其目标。
    pub fn symlinks(&self) -> Vec<(PathBuf, PathBuf)> {
        self.entries
            .iter()
            .filter_map(|entry| {
                entry
                    .link_target
                    .as_ref()
                    .map(|target| (entry.relative_path.clone(), target.clone()))
            })
            .collect()
    }
}

/// 物化结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializeOutcome {
    /// 已写入的相对路径，按扫描顺序。
    pub written: Vec<PathBuf>,
    /// 被物化为真实内容的链接。
    pub symlinks: Vec<(PathBuf, PathBuf)>,
    pub skipped: Vec<Skipped>,
}

/// 核心层错误。
#[derive(Debug)]
pub enum ScopeError {
    Io(io::Error),
    /// 目标目录非空：本函数只写空目录或新目录。
    DestNotEmpty(PathBuf),
    /// 相对路径越出根目录，拒绝拼接。
    OutOfBounds(PathBuf),
}

impl fmt::Display for ScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "读写失败：{error}"),
            Self::DestNotEmpty(path) => {
                write!(f, "目标目录不是空目录：{}", path.display())
            }
            Self::OutOfBounds(path) => {
                write!(f, "路径越出目标根：{}", path.display())
            }
        }
    }
}

impl std::error::Error for ScopeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ScopeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// 把相对路径落到 `root` 之下。绝对路径、`..`、根前缀一律拒绝；
/// 组件检查后再比一次前缀，双重保证不写到根之外。
pub fn resolve_within(root: &Path, relative: &Path) -> Result<PathBuf, ScopeError> {
    if relative.as_os_str().is_empty() {
        return Err(ScopeError::OutOfBounds(relative.to_path_buf()));
    }
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ScopeError::OutOfBounds(relative.to_path_buf()));
    }
    let joined = root.join(relative);
    if !joined.starts_with(root) {
        return Err(ScopeError::OutOfBounds(relative.to_path_buf()));
    }
    Ok(joined)
}

/// 按受管范围遍历 agent 目录。`agent_dir` 本身不可读是硬错误；范围内条目不可读只记录。
pub fn scan(agent_dir: &Path) -> Result<ScanOutcome, ScopeError> {
    let mut walker = Walker {
        outcome: ScanOutcome::default(),
        active: HashSet::new(),
    };
    walker.walk(agent_dir, Path::new(""), true)?;
    let mut outcome = walker.outcome;
    outcome
        .entries
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    outcome
        .skipped
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(outcome)
}

/// 把受管范围物化复制到 `dest_dir`：符号链接写成真实内容，断链等跳过项不中断整体。
/// `dest_dir` 必须不存在或为空；任何写入都落在 `dest_dir` 之内。
pub fn materialize(agent_dir: &Path, dest_dir: &Path) -> Result<MaterializeOutcome, ScopeError> {
    materialize_inner(agent_dir, dest_dir, true)
}

/// 与 [`materialize`] 相同，但不复制根级 `auth.json`；自动上行默认用它。
pub fn materialize_without_auth(
    agent_dir: &Path,
    dest_dir: &Path,
) -> Result<MaterializeOutcome, ScopeError> {
    materialize_inner(agent_dir, dest_dir, false)
}

fn materialize_inner(
    agent_dir: &Path,
    dest_dir: &Path,
    include_auth: bool,
) -> Result<MaterializeOutcome, ScopeError> {
    let scanned = scan(agent_dir)?;

    if dest_dir.exists() {
        if !dest_dir.is_dir() || fs::read_dir(dest_dir)?.next().is_some() {
            return Err(ScopeError::DestNotEmpty(dest_dir.to_path_buf()));
        }
    } else {
        fs::create_dir_all(dest_dir)?;
    }

    let mut written = Vec::with_capacity(scanned.entries.len());
    for entry in &scanned.entries {
        if !include_auth && entry.relative_path == Path::new("auth.json") {
            continue;
        }
        let destination = resolve_within(dest_dir, &entry.relative_path)?;
        let source = resolve_within(agent_dir, &entry.relative_path)?;
        match entry.kind {
            EntryKind::Dir => fs::create_dir_all(&destination)?,
            EntryKind::File => {
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                // fs::copy 跟随符号链接，正好就是物化。
                fs::copy(&source, &destination)?;
            }
        }
        written.push(entry.relative_path.clone());
    }

    Ok(MaterializeOutcome {
        written,
        symlinks: scanned
            .symlinks()
            .into_iter()
            .filter(|(path, _)| include_auth || path != Path::new("auth.json"))
            .collect(),
        skipped: scanned.skipped,
    })
}

struct Walker {
    outcome: ScanOutcome,
    /// 当前路径上已进入的物理目录，用于识别链接成环。
    active: HashSet<PathBuf>,
}

impl Walker {
    fn walk(&mut self, physical: &Path, relative: &Path, is_root: bool) -> Result<(), ScopeError> {
        let canonical = match fs::canonicalize(physical) {
            Ok(path) => path,
            Err(error) if is_root => return Err(ScopeError::Io(error)),
            Err(_) => {
                self.skip(relative, SkipReason::Unreadable);
                return Ok(());
            }
        };
        if !self.active.insert(canonical.clone()) {
            self.skip(relative, SkipReason::LinkLoop);
            return Ok(());
        }

        let names = match fs::read_dir(physical) {
            Ok(read_dir) => {
                let mut names: Vec<OsString> = Vec::new();
                for item in read_dir {
                    match item {
                        Ok(dir_entry) => names.push(dir_entry.file_name()),
                        // 单个目录项读不出名字，算父目录不可读。
                        Err(_) => self.skip(relative, SkipReason::Unreadable),
                    }
                }
                Some(names)
            }
            Err(error) if is_root => return Err(ScopeError::Io(error)),
            Err(_) => {
                self.skip(relative, SkipReason::Unreadable);
                None
            }
        };

        if let Some(names) = names {
            for name in names {
                self.visit(physical, relative, &name, is_root)?;
            }
        }

        self.active.remove(&canonical);
        Ok(())
    }

    /// 判定并收录单个目录项。排除项在读内容之前就返回，不做"先读后过滤"。
    fn visit(
        &mut self,
        physical: &Path,
        relative: &Path,
        name: &OsString,
        is_root: bool,
    ) -> Result<(), ScopeError> {
        let label = name.to_string_lossy();
        let relative_path = relative.join(name);
        if is_excluded_relative(&relative_path) || (is_root && !is_included_root_name(&label)) {
            return Ok(());
        }

        let physical_path = physical.join(name);

        let link_metadata = match fs::symlink_metadata(&physical_path) {
            Ok(metadata) => metadata,
            Err(_) => {
                self.skip(&relative_path, SkipReason::Unreadable);
                return Ok(());
            }
        };

        // junction 与符号链接都算链接：`read_link` 读得到目标就算。
        let link_target = if platform::is_link_like(&physical_path) {
            match fs::read_link(&physical_path) {
                Ok(target) => Some(target),
                Err(_) => {
                    self.skip(&relative_path, SkipReason::Unreadable);
                    return Ok(());
                }
            }
        } else {
            None
        };

        // 链接条目按目标类型归类；跟随失败即断链。
        let metadata = if link_target.is_some() {
            match fs::metadata(&physical_path) {
                Ok(metadata) => metadata,
                Err(_) => {
                    self.skip(&relative_path, SkipReason::BrokenSymlink);
                    return Ok(());
                }
            }
        } else {
            link_metadata
        };

        if metadata.is_dir() {
            self.outcome.entries.push(Entry {
                relative_path: relative_path.clone(),
                kind: EntryKind::Dir,
                size: 0,
                link_target,
            });
            self.walk(&physical_path, &relative_path, false)
        } else if metadata.is_file() {
            self.outcome.entries.push(Entry {
                relative_path,
                kind: EntryKind::File,
                size: metadata.len(),
                link_target,
            });
            Ok(())
        } else {
            self.skip(&relative_path, SkipReason::SpecialFile);
            Ok(())
        }
    }

    fn skip(&mut self, relative_path: &Path, reason: SkipReason) {
        self.outcome.skipped.push(Skipped {
            relative_path: relative_path.to_path_buf(),
            reason,
        });
    }
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
                "agent-config-sync-{}-{}-{}",
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

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("创建父目录");
        }
        fs::write(path, content).expect("写入夹具文件");
    }

    fn scan_paths(outcome: &ScanOutcome) -> Vec<String> {
        outcome
            .entries
            .iter()
            .map(|entry| slash(&entry.relative_path))
            .collect()
    }

    #[cfg(unix)]
    fn skipped_paths(outcome: &ScanOutcome) -> Vec<(String, SkipReason)> {
        outcome
            .skipped
            .iter()
            .map(|skipped| (slash(&skipped.relative_path), skipped.reason))
            .collect()
    }

    fn slash(path: &Path) -> String {
        path.components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")
    }

    #[test]
    fn root_json_included_and_excluded_names_skipped() {
        let temp = TempDir::new("root-json");
        let agent = temp.path().join("agent");
        fs::create_dir_all(&agent).expect("创建 agent 目录");

        for name in [
            "settings.json",
            "models.json",
            "mcp.json",
            "web-search.json",
            "keybindings.json",
        ] {
            write_file(&agent.join(name), "{}");
        }
        for name in [
            "auth.json",
            "models-store.json",
            "trust.json",
            "web-push.json",
            "pi-web-session-state.json",
            "pi-web-session-index.json",
            "pi-web-session-abc.json",
            "run-history.jsonl",
            "notes.txt",
        ] {
            write_file(&agent.join(name), "{}");
        }

        let outcome = scan(&agent).expect("扫描");
        assert_eq!(
            scan_paths(&outcome),
            vec![
                "auth.json",
                "keybindings.json",
                "mcp.json",
                "models.json",
                "settings.json",
                "web-search.json",
            ]
        );
        // 范围外条目不是"跳过"，不进 skipped。
        assert!(outcome.skipped.is_empty());
    }

    #[test]
    fn nested_auth_json_is_excluded_but_root_is_included() {
        let temp = TempDir::new("auth-root-only");
        let agent = temp.path().join("agent");
        write_file(&agent.join("auth.json"), "{}");
        write_file(&agent.join("skills/auth.json"), "{}");
        write_file(&agent.join("skills/demo/auth.json"), "{}");

        let outcome = scan(&agent).expect("扫描");
        assert_eq!(
            scan_paths(&outcome),
            vec!["auth.json", "skills", "skills/demo"]
        );
    }

    #[test]
    fn managed_directories_included_recursively_others_ignored() {
        let temp = TempDir::new("dirs");
        let agent = temp.path().join("agent");
        write_file(&agent.join("skills/antd/SKILL.md"), "skill");
        write_file(&agent.join("extensions/demo/index.js"), "ext");
        write_file(&agent.join("prompts/review.md"), "prompt");
        write_file(&agent.join("themes/dark.json"), "{}");
        write_file(&agent.join("agents/helper.md"), "agent");
        for name in [
            "sessions",
            "install",
            "npm",
            "fff",
            "web-search-cache",
            "missions",
            "pi-subagents",
            "pi-web-access",
            "pi-web-run",
            "cache",
        ] {
            write_file(&agent.join(name).join("inner.json"), "{}");
        }
        // 排除名称在任何层级都生效。
        write_file(&agent.join("skills/npm/package.json"), "{}");
        // 排除目录内的断链：若遍历进去了就会多出一条 skipped。
        #[cfg(unix)]
        std::os::unix::fs::symlink("../nowhere", agent.join("sessions/dangling"))
            .expect("创建排除目录内的断链");

        let outcome = scan(&agent).expect("扫描");
        assert_eq!(
            scan_paths(&outcome),
            vec![
                "agents",
                "agents/helper.md",
                "extensions",
                "extensions/demo",
                "extensions/demo/index.js",
                "prompts",
                "prompts/review.md",
                "skills",
                "skills/antd",
                "skills/antd/SKILL.md",
                "themes",
                "themes/dark.json",
            ]
        );
        // 排除目录从未进入，所以没有断链/不可读之类的记录。
        assert!(outcome.skipped.is_empty(), "{:?}", outcome.skipped);
    }

    #[test]
    fn materialize_requires_empty_destination() {
        let temp = TempDir::new("dest");
        let agent = temp.path().join("agent");
        write_file(&agent.join("settings.json"), "{}");

        let occupied = temp.path().join("occupied");
        write_file(&occupied.join("stale.json"), "{}");
        assert!(matches!(
            materialize(&agent, &occupied),
            Err(ScopeError::DestNotEmpty(_))
        ));

        let empty = temp.path().join("empty");
        fs::create_dir_all(&empty).expect("创建空目录");
        assert!(materialize(&agent, &empty).is_ok());

        let missing = temp.path().join("missing");
        let outcome = materialize(&agent, &missing).expect("目标不存在时自动创建");
        assert_eq!(outcome.written, vec![PathBuf::from("settings.json")]);
        assert_eq!(
            fs::read_to_string(missing.join("settings.json")).expect("读取目标文件"),
            "{}"
        );
    }

    #[test]
    fn relative_paths_stay_inside_and_escape_rejected() {
        let temp = TempDir::new("bounds");
        let agent = temp.path().join("agent");
        write_file(&agent.join("skills/nested/SKILL.md"), "skill");

        let outcome = scan(&agent).expect("扫描");
        assert!(!outcome.entries.is_empty());
        for entry in &outcome.entries {
            assert!(
                entry
                    .relative_path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
                "相对路径含非普通组件：{}",
                entry.relative_path.display()
            );
        }

        let dest = temp.path().join("dest");
        assert!(matches!(
            resolve_within(&dest, Path::new("../escape.json")),
            Err(ScopeError::OutOfBounds(_))
        ));
        assert!(matches!(
            resolve_within(&dest, Path::new("skills/../../escape.json")),
            Err(ScopeError::OutOfBounds(_))
        ));
        assert!(matches!(
            resolve_within(&dest, Path::new("/etc/passwd")),
            Err(ScopeError::OutOfBounds(_))
        ));
        assert!(matches!(
            resolve_within(&dest, Path::new("")),
            Err(ScopeError::OutOfBounds(_))
        ));
        assert_eq!(
            resolve_within(&dest, Path::new("skills/nested/SKILL.md")).expect("合法相对路径"),
            dest.join("skills/nested/SKILL.md")
        );
    }

    #[test]
    fn scan_is_deterministic() {
        let temp = TempDir::new("deterministic");
        let agent = temp.path().join("agent");
        for name in ["b.json", "a.json", "c.md"] {
            write_file(&agent.join(name), "{}");
        }
        write_file(&agent.join("skills/z/SKILL.md"), "z");
        write_file(&agent.join("skills/a/SKILL.md"), "a");

        let first = scan(&agent).expect("第一次扫描");
        let second = scan(&agent).expect("第二次扫描");
        assert_eq!(first, second);

        let paths = scan_paths(&first);
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "扫描结果必须有序");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_materialized_as_real_content() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("symlink");
        let agent = temp.path().join("agent");
        let external = temp.path().join("external");
        fs::create_dir_all(agent.join("skills")).expect("创建 skills");
        fs::create_dir_all(agent.join("agents")).expect("创建 agents");
        write_file(&external.join("shared.md"), "外部文件");
        write_file(&external.join("tree/inside.md"), "外部目录");

        // 相对链接跨出 agent 目录；目录链接同理。
        symlink("../../external/shared.md", agent.join("skills/linked.md")).expect("创建文件链接");
        symlink("../../external/tree", agent.join("agents/pi-shared")).expect("创建目录链接");

        let outcome = scan(&agent).expect("扫描");
        assert_eq!(
            scan_paths(&outcome),
            vec![
                "agents",
                "agents/pi-shared",
                "agents/pi-shared/inside.md",
                "skills",
                "skills/linked.md",
            ]
        );
        assert_eq!(
            outcome.symlinks(),
            vec![
                (
                    PathBuf::from("agents/pi-shared"),
                    PathBuf::from("../../external/tree")
                ),
                (
                    PathBuf::from("skills/linked.md"),
                    PathBuf::from("../../external/shared.md")
                ),
            ]
        );

        let dest = temp.path().join("dest");
        let materialized = materialize(&agent, &dest).expect("物化");
        let entry_paths = outcome
            .entries
            .iter()
            .map(|entry| entry.relative_path.clone())
            .collect::<Vec<_>>();
        assert_eq!(materialized.written, entry_paths);
        assert_eq!(materialized.symlinks, outcome.symlinks());

        let linked = dest.join("skills/linked.md");
        let linked_metadata = fs::symlink_metadata(&linked).expect("目标文件存在");
        assert!(
            linked_metadata.file_type().is_file(),
            "链接必须物化为真实文件"
        );
        assert_eq!(
            fs::read_to_string(&linked).expect("读取物化文件"),
            "外部文件"
        );
        assert_eq!(
            fs::read_to_string(dest.join("agents/pi-shared/inside.md")).expect("读取物化目录内容"),
            "外部目录"
        );

        // 源目录里的链接保持链接，agent 目录之外不产生写入。
        assert!(fs::symlink_metadata(agent.join("skills/linked.md"))
            .expect("源链接仍在")
            .file_type()
            .is_symlink());
        assert!(!external.join("dest").exists());
        assert_eq!(
            fs::read_dir(&external).expect("读取外部目录").count(),
            2,
            "外部目录不应被写入"
        );
    }

    #[cfg(unix)]
    #[test]
    fn broken_symlink_skipped_without_failing() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("broken");
        let agent = temp.path().join("agent");
        write_file(&agent.join("settings.json"), "{}");
        fs::create_dir_all(agent.join("skills")).expect("创建 skills");
        symlink("../missing-agents.md", agent.join("AGENTS.md")).expect("创建断链");
        symlink("gone.md", agent.join("skills/gone.md")).expect("创建断链");

        let outcome = scan(&agent).expect("断链不应导致扫描失败");
        assert_eq!(scan_paths(&outcome), vec!["settings.json", "skills"]);
        let mut skipped = skipped_paths(&outcome);
        skipped.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            skipped,
            vec![
                ("AGENTS.md".to_string(), SkipReason::BrokenSymlink),
                ("skills/gone.md".to_string(), SkipReason::BrokenSymlink),
            ]
        );

        let dest = temp.path().join("dest");
        let materialized = materialize(&agent, &dest).expect("断链不应导致物化失败");
        assert_eq!(
            materialized.written,
            vec![PathBuf::from("settings.json"), PathBuf::from("skills")]
        );
        assert_eq!(materialized.skipped.len(), 2);
        assert!(!dest.join("AGENTS.md").exists());
        assert!(!dest.join("skills/gone.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loop_skipped() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("loop");
        let agent = temp.path().join("agent");
        write_file(&agent.join("agents/helper.md"), "agent");
        symlink("..", agent.join("agents/loop")).expect("创建回环链接");

        let outcome = scan(&agent).expect("扫描");
        assert_eq!(
            scan_paths(&outcome),
            vec!["agents", "agents/helper.md", "agents/loop"]
        );
        assert_eq!(
            skipped_paths(&outcome),
            vec![("agents/loop".to_string(), SkipReason::LinkLoop)]
        );
    }

    #[cfg(unix)]
    #[test]
    fn special_files_skipped() {
        let temp = TempDir::new("special");
        let agent = temp.path().join("agent");
        fs::create_dir_all(agent.join("skills")).expect("创建 skills");
        let fifo = agent.join("skills/pipe");
        let created = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !created {
            return; // 环境无 mkfifo 时无法造特殊文件
        }

        let outcome = scan(&agent).expect("扫描");
        assert_eq!(
            skipped_paths(&outcome),
            vec![("skills/pipe".to_string(), SkipReason::SpecialFile)]
        );
    }
}

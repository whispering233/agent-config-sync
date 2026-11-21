//! 冲突文件扫描（只读）。
//!
//! 规则来源：`docs/design/10-snapshot-sync.md` 的「冲突文件」与「受管范围」两节。
//! 只识别、只算摘要：不合并、不改名、不删除，也不写任何文件。
//! 处理动作（保留 / 替换）需要备份能力，不在这里实现。

use crate::config;
use crate::scope;
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

/// 逐行比较的体量上限：任一侧超过即只报大小。
pub const MAX_COMPARE_BYTES: u64 = 1024 * 1024;

/// Windows 云盘客户端的冲突标记。
const WINDOWS_MARKER: &str = "-冲突";
/// macOS / Linux 云盘客户端的冲突标记。
const MACOS_MARKER: &str = "-NSConflict";

/// 冲突文件与原名文件的配对状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PairStatus {
    /// 原名文件存在，可比较差异。
    Paired,
    /// 原名文件不存在，只能单独列出。
    Orphan,
}

/// 摘要的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryKind {
    /// 内容完全相同，只有文件名不同。
    Identical,
    /// 文本逐行比较结果。
    Text,
    /// 任一侧不是 UTF-8 文本。
    Binary,
    /// 任一侧超过 [`MAX_COMPARE_BYTES`]。
    TooLarge,
    /// 文件读不出来。
    Unreadable,
    /// 原名文件不存在，无从比较。
    Unpaired,
}

/// 一份差异摘要。`binary` / `too_large` 只有 [`ConflictItem::size`] 可用，这里留空。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictSummary {
    pub kind: SummaryKind,
    /// 冲突文件的行数（仅 `text`）。
    pub lines: Option<u64>,
    /// 原名文件的行数（仅 `text`）。
    pub original_lines: Option<u64>,
    /// 差异行数：短边逐行比较的差异 + 行数差（仅 `text`）。
    pub differing_lines: Option<u64>,
    /// 首处差异行号，1-based；无差异为 `None`（仅 `text`）。
    pub first_diff_line: Option<u64>,
}

impl ConflictSummary {
    fn plain(kind: SummaryKind) -> Self {
        Self {
            kind,
            lines: None,
            original_lines: None,
            differing_lines: None,
            first_diff_line: None,
        }
    }
}

/// 一对冲突文件。路径相对宿主目录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictItem {
    pub original: PathBuf,
    pub conflict: PathBuf,
    pub pair_status: PairStatus,
    /// 冲突文件的字节数。
    pub size: u64,
    /// 原名文件的字节数；`orphan` 时为 `None`。
    pub original_size: Option<u64>,
    pub summary: ConflictSummary,
}

/// 扫描总计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictTotals {
    pub total: usize,
    pub paired: usize,
    pub orphan: usize,
}

/// 一次扫描的结果。`items` 按冲突文件相对路径升序。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictReport {
    pub items: Vec<ConflictItem>,
    pub totals: ConflictTotals,
}

/// 按受管范围扫描冲突文件。`agent_dir` 不可读是硬错误；单个条目读不出只影响该条。
pub fn scan(agent_dir: &Path) -> io::Result<ConflictReport> {
    let mut found = Vec::new();
    walk(
        agent_dir,
        Path::new(""),
        true,
        &mut HashSet::new(),
        &mut found,
    )?;
    found.sort_by(|left, right| left.relative.cmp(&right.relative));

    let items: Vec<ConflictItem> = found
        .into_iter()
        .map(|found| item(agent_dir, found))
        .collect();
    let paired = items
        .iter()
        .filter(|item| item.pair_status == PairStatus::Paired)
        .count();

    Ok(ConflictReport {
        totals: ConflictTotals {
            total: items.len(),
            paired,
            orphan: items.len() - paired,
        },
        items,
    })
}

/// `scan_conflicts` 的输入：生效宿主目录来自应用配置（覆盖 → 环境变量 → `~/.pi/agent`）。
#[tauri::command]
pub fn scan_conflicts(app: AppHandle) -> Result<ConflictReport, String> {
    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("无法定位应用配置目录：{error}"))?;
    let config = config::load_config(&config_dir).map_err(|error| error.to_string())?;

    // 同样只读这一个文档化的变量，错误信息里不回显其取值。
    let env_agent_dir = std::env::var_os(config::AGENT_DIR_ENV_VAR).map(PathBuf::from);
    let home_dir = app.path().home_dir().ok();
    let resolved =
        config::resolve_agent_dir(&config, env_agent_dir.as_deref(), home_dir.as_deref());

    let agent_dir = resolved.path.ok_or_else(|| {
        format!(
            "无法确定宿主目录：请填写宿主目录，或设置环境变量 {}",
            config::AGENT_DIR_ENV_VAR
        )
    })?;

    scan(&agent_dir).map_err(|error| format!("无法扫描宿主目录 {}：{error}", agent_dir.display()))
}

struct Found {
    relative: PathBuf,
    original: PathBuf,
    size: u64,
}

fn item(agent_dir: &Path, found: Found) -> ConflictItem {
    let conflict_path = agent_dir.join(&found.relative);
    let original_path = agent_dir.join(&found.original);

    let (pair_status, original_size, summary) = match fs::metadata(&original_path) {
        Ok(metadata) if metadata.is_file() => {
            let original_size = metadata.len();
            (
                PairStatus::Paired,
                Some(original_size),
                summarize(&conflict_path, found.size, &original_path, original_size),
            )
        }
        _ => (
            PairStatus::Orphan,
            None,
            ConflictSummary::plain(SummaryKind::Unpaired),
        ),
    };

    ConflictItem {
        original: found.original,
        conflict: found.relative,
        pair_status,
        size: found.size,
        original_size,
        summary,
    }
}

fn summarize(
    conflict: &Path,
    conflict_size: u64,
    original: &Path,
    original_size: u64,
) -> ConflictSummary {
    if conflict_size > MAX_COMPARE_BYTES || original_size > MAX_COMPARE_BYTES {
        return ConflictSummary::plain(SummaryKind::TooLarge);
    }

    let (Ok(conflict_bytes), Ok(original_bytes)) = (fs::read(conflict), fs::read(original)) else {
        return ConflictSummary::plain(SummaryKind::Unreadable);
    };

    if conflict_bytes == original_bytes {
        return ConflictSummary::plain(SummaryKind::Identical);
    }

    let (Ok(conflict_text), Ok(original_text)) = (
        String::from_utf8(conflict_bytes),
        String::from_utf8(original_bytes),
    ) else {
        return ConflictSummary::plain(SummaryKind::Binary);
    };

    text_summary(&conflict_text, &original_text)
}

fn text_summary(conflict: &str, original: &str) -> ConflictSummary {
    let conflict_lines: Vec<&str> = conflict.lines().collect();
    let original_lines: Vec<&str> = original.lines().collect();
    let shared = conflict_lines.len().min(original_lines.len());

    let mut differing_lines = (conflict_lines.len() as u64).abs_diff(original_lines.len() as u64);
    let mut first_diff_line = None;
    for index in 0..shared {
        if conflict_lines[index] != original_lines[index] {
            differing_lines += 1;
            if first_diff_line.is_none() {
                first_diff_line = Some(index as u64 + 1);
            }
        }
    }
    // 短边完全一致时，差异从较短文件之后的第一行开始。
    if first_diff_line.is_none() && differing_lines > 0 {
        first_diff_line = Some(shared as u64 + 1);
    }

    ConflictSummary {
        kind: SummaryKind::Text,
        lines: Some(conflict_lines.len() as u64),
        original_lines: Some(original_lines.len() as u64),
        differing_lines: Some(differing_lines),
        first_diff_line,
    }
}

/// 遍历受管范围。不进入排除目录，根级只进纳入目录；符号链接跟随，成环即跳过。
fn walk(
    dir: &Path,
    relative: &Path,
    is_root: bool,
    active: &mut HashSet<PathBuf>,
    found: &mut Vec<Found>,
) -> io::Result<()> {
    let canonical = match fs::canonicalize(dir) {
        Ok(path) => path,
        Err(error) => {
            return if is_root { Err(error) } else { Ok(()) };
        }
    };
    if !active.insert(canonical.clone()) {
        return Ok(());
    }

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            active.remove(&canonical);
            return if is_root { Err(error) } else { Ok(()) };
        }
    };

    for entry in entries {
        let Ok(entry) = entry else { continue };
        // 冲突标记是 UTF-8 文本，非 UTF-8 文件名不可能带标记。
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if scope::is_excluded_name(&name) {
            continue;
        }

        let path = entry.path();
        // 跟随链接；断链直接跳过。
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        let relative_path = relative.join(&name);

        if metadata.is_dir() {
            if is_root && !scope::INCLUDED_ROOT_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(&path, &relative_path, false, active, found)?;
        } else if metadata.is_file() {
            if is_root && !root_file_included(&name) {
                continue;
            }
            if let Some(original) = conflict_original_name(&name) {
                // 冲突副本跟随原名的排除规则：`auth.json` 的副本同样是密钥。
                if scope::is_excluded_name(&original) {
                    continue;
                }
                found.push(Found {
                    original: relative.join(original),
                    relative: relative_path,
                    size: metadata.len(),
                });
            }
        }
    }

    active.remove(&canonical);
    Ok(())
}

/// 根级文件是否在受管范围内。冲突副本按"原名是否在范围内"判定，
/// 这样根级的 `AGENTS-冲突.md` 也能被识别。
fn root_file_included(name: &str) -> bool {
    is_included_root_name(name)
        || conflict_original_name(name).is_some_and(|original| is_included_root_name(&original))
}

/// 与 `scope` 的根级纳入规则保持一致；该判定未在 `scope` 导出，故在此复述。
fn is_included_root_name(name: &str) -> bool {
    scope::INCLUDED_ROOT_FILES.contains(&name)
        || scope::INCLUDED_ROOT_DIRS.contains(&name)
        || Path::new(name)
            .extension()
            .is_some_and(|extension| extension == "json")
}

/// 从冲突文件名还原原名：去掉冲突标记与成员昵称后缀，保留扩展名。
fn conflict_original_name(name: &str) -> Option<String> {
    let (marker, index) = match (name.find(WINDOWS_MARKER), name.find(MACOS_MARKER)) {
        (Some(windows), Some(macos)) => {
            if windows <= macos {
                (WINDOWS_MARKER, windows)
            } else {
                (MACOS_MARKER, macos)
            }
        }
        (Some(windows), None) => (WINDOWS_MARKER, windows),
        (None, Some(macos)) => (MACOS_MARKER, macos),
        (None, None) => return None,
    };

    let prefix = &name[..index];
    if prefix.is_empty() {
        return None;
    }

    // 共享文件夹会在标记后追加 `-昵称`，扩展名从昵称之后的第一个 `.` 开始。
    let rest = &name[index + marker.len()..];
    let extension = match rest.find('.') {
        Some(dot) => &rest[dot..],
        None => "",
    };

    Some(format!("{prefix}{extension}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时目录夹具：Drop 时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-conflicts-{}-{}-{}",
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

    fn write_file(path: &Path, content: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("创建父目录");
        }
        fs::write(path, content).expect("写入夹具文件");
    }

    fn conflicts(report: &ConflictReport) -> Vec<String> {
        report
            .items
            .iter()
            .map(|item| item.conflict.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    fn find<'a>(report: &'a ConflictReport, conflict: &str) -> &'a ConflictItem {
        report
            .items
            .iter()
            .find(|item| item.conflict == Path::new(conflict))
            .unwrap_or_else(|| panic!("未找到冲突文件 {conflict}"))
    }

    #[test]
    fn both_naming_schemes_and_nickname_suffix_parsed() {
        assert_eq!(
            conflict_original_name("settings-冲突.json").as_deref(),
            Some("settings.json")
        );
        assert_eq!(
            conflict_original_name("settings-NSConflict.json").as_deref(),
            Some("settings.json")
        );
        assert_eq!(
            conflict_original_name("settings-冲突-张三.json").as_deref(),
            Some("settings.json")
        );
        assert_eq!(
            conflict_original_name("settings-NSConflict-张三.json").as_deref(),
            Some("settings.json")
        );
        // 目录内文件同样可识别。
        assert_eq!(
            conflict_original_name("SKILL-冲突.md").as_deref(),
            Some("SKILL.md")
        );
        // 原名自带点号时只剥掉最后一段扩展名之后的部分。
        assert_eq!(
            conflict_original_name("a.b-NSConflict.json").as_deref(),
            Some("a.b.json")
        );
        // 无扩展名。
        assert_eq!(
            conflict_original_name("LICENSE-冲突").as_deref(),
            Some("LICENSE")
        );

        for name in [
            "settings.json",
            "冲突.json",
            "-冲突.json",
            "settings-other.json",
            "NSConflict.json",
        ] {
            assert_eq!(conflict_original_name(name), None, "{name} 不应识别为冲突");
        }
    }

    #[test]
    fn scan_pairs_with_original_and_lists_orphans() {
        let temp = TempDir::new("pairs");
        let agent = temp.path();
        write_file(&agent.join("settings.json"), b"{\"a\":1}\n");
        write_file(&agent.join("settings-冲突.json"), b"{\"a\":2}\n");
        write_file(&agent.join("web-search-NSConflict.json"), b"{\"b\":3}\n");

        let report = scan(agent).expect("扫描");
        assert_eq!(
            conflicts(&report),
            vec!["settings-冲突.json", "web-search-NSConflict.json"],
            "按冲突文件相对路径升序"
        );
        assert_eq!(
            report.totals,
            ConflictTotals {
                total: 2,
                paired: 1,
                orphan: 1
            }
        );

        let paired = find(&report, "settings-冲突.json");
        assert_eq!(paired.pair_status, PairStatus::Paired);
        assert_eq!(paired.original, PathBuf::from("settings.json"));
        assert_eq!(paired.original_size, Some(8));
        assert_eq!(paired.summary.kind, SummaryKind::Text);
        assert_eq!(paired.summary.first_diff_line, Some(1));

        let orphan = find(&report, "web-search-NSConflict.json");
        assert_eq!(orphan.pair_status, PairStatus::Orphan);
        assert_eq!(orphan.original, PathBuf::from("web-search.json"));
        assert_eq!(orphan.original_size, None);
        assert_eq!(
            orphan.summary,
            ConflictSummary::plain(SummaryKind::Unpaired)
        );
        assert_eq!(orphan.size, 8);
    }

    #[test]
    fn text_summary_counts_lines_differences_and_first_diff() {
        let temp = TempDir::new("text-summary");
        let agent = temp.path();
        write_file(&agent.join("AGENTS.md"), b"a\nb\nc\n");
        write_file(&agent.join("AGENTS-冲突.md"), b"a\nB\nc\nd\n");
        // 短边一致时差异落在较短文件之后的第一行。
        write_file(&agent.join("SYSTEM.md"), b"x\ny\n");
        write_file(&agent.join("SYSTEM-NSConflict.md"), b"x\ny\nz\n");

        let report = scan(agent).expect("扫描");

        let first = find(&report, "AGENTS-冲突.md").summary.clone();
        assert_eq!(
            first,
            ConflictSummary {
                kind: SummaryKind::Text,
                lines: Some(4),
                original_lines: Some(3),
                differing_lines: Some(2),
                first_diff_line: Some(2),
            }
        );

        let appended = find(&report, "SYSTEM-NSConflict.md").summary.clone();
        assert_eq!(appended.differing_lines, Some(1));
        assert_eq!(appended.first_diff_line, Some(3));
    }

    #[test]
    fn identical_content_marked_as_only_renamed() {
        let temp = TempDir::new("identical");
        let agent = temp.path();
        write_file(&agent.join("keybindings.json"), b"{\"same\":true}\n");
        write_file(
            &agent.join("keybindings-NSConflict.json"),
            b"{\"same\":true}\n",
        );

        let report = scan(agent).expect("扫描");
        let item = find(&report, "keybindings-NSConflict.json");
        assert_eq!(item.pair_status, PairStatus::Paired);
        assert_eq!(item.summary, ConflictSummary::plain(SummaryKind::Identical));
    }

    #[test]
    fn binary_and_oversized_conflicts_only_report_size() {
        let temp = TempDir::new("binary-size");
        let agent = temp.path();

        write_file(&agent.join("themes/dark.json"), b"{\"theme\":\"dark\"}\n");
        write_file(
            &agent.join("themes/dark-冲突.json"),
            &[0xff, 0xfe, 0x00, 0x01],
        );

        let oversized = vec![b'a'; MAX_COMPARE_BYTES as usize + 1];
        write_file(&agent.join("prompts/review.md"), b"short\n");
        write_file(&agent.join("prompts/review-NSConflict.md"), &oversized);

        let report = scan(agent).expect("扫描");

        let binary = find(&report, "themes/dark-冲突.json");
        assert_eq!(binary.summary, ConflictSummary::plain(SummaryKind::Binary));
        assert_eq!(binary.size, 4);

        let large = find(&report, "prompts/review-NSConflict.md");
        assert_eq!(large.summary, ConflictSummary::plain(SummaryKind::TooLarge));
        assert_eq!(large.size, MAX_COMPARE_BYTES + 1);
        assert_eq!(large.original_size, Some(6));
    }

    #[test]
    fn scan_stays_inside_managed_scope() {
        let temp = TempDir::new("scope");
        let agent = temp.path();
        write_file(&agent.join("skills/reviewer/SKILL.md"), b"skill\n");
        write_file(&agent.join("skills/reviewer/SKILL-冲突.md"), b"skill v2\n");
        // 根级指令文件的冲突副本：原名在范围内，副本也要被识别。
        write_file(&agent.join("AGENTS.md"), b"agents\n");
        write_file(&agent.join("AGENTS-冲突.md"), b"agents v2\n");

        // 排除目录、范围外根级文件与非纳入根级目录都不进入。
        write_file(&agent.join("sessions/s-冲突.json"), b"{}");
        write_file(&agent.join("npm/p-NSConflict.json"), b"{}");
        write_file(&agent.join("notes-冲突.md"), b"notes\n");
        write_file(&agent.join("cache/c-冲突.json"), b"{}");
        // 原名被排除的副本也要留在范围外。
        write_file(&agent.join("auth.json"), b"{\"key\":\"literal\"}\n");
        write_file(&agent.join("auth-冲突.json"), b"{}");
        write_file(&agent.join("skills/auth-NSConflict.json"), b"{}");

        let report = scan(agent).expect("扫描");
        assert_eq!(
            conflicts(&report),
            vec!["AGENTS-冲突.md", "skills/reviewer/SKILL-冲突.md"]
        );
        assert_eq!(report.totals.paired, 2);
        assert_eq!(report.totals.orphan, 0);
    }

    #[test]
    fn scan_is_deterministic() {
        let temp = TempDir::new("deterministic");
        let agent = temp.path();
        for name in ["b-冲突.json", "a-NSConflict.json", "c-冲突.json"] {
            write_file(&agent.join(name), b"{}");
        }
        write_file(&agent.join("skills/z/SKILL-冲突.md"), b"z\n");

        let first = scan(agent).expect("第一次扫描");
        let second = scan(agent).expect("第二次扫描");
        assert_eq!(first, second);

        let paths = conflicts(&first);
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "扫描结果必须有序");
        assert_eq!(first.totals.orphan, 4);
    }

    #[test]
    fn report_serializes_with_frontend_field_names() {
        let temp = TempDir::new("serialize");
        let agent = temp.path();
        write_file(&agent.join("settings.json"), b"{}\n");
        write_file(&agent.join("settings-冲突.json"), b"{}\n");

        let report = scan(agent).expect("扫描");
        let value = serde_json::to_value(&report).expect("结果应可序列化");

        let item = &value["items"][0];
        let keys: HashSet<&str> = item
            .as_object()
            .expect("条目应为 JSON 对象")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            HashSet::from([
                "conflict",
                "original",
                "originalSize",
                "pairStatus",
                "size",
                "summary",
            ])
        );
        assert_eq!(item["pairStatus"], serde_json::json!("paired"));
        assert_eq!(item["original"], serde_json::json!("settings.json"));
        assert_eq!(item["summary"]["kind"], serde_json::json!("identical"));
        assert_eq!(item["summary"]["firstDiffLine"], serde_json::Value::Null);
        assert_eq!(value["totals"]["orphan"], serde_json::json!(0));
    }
}

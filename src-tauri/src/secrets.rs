//! 密钥扫描。
//!
//! 规则来源：`docs/design/30-secrets.md`。扫描对象是快照内的文本文件；
//! 只读，不改写、不阻断。findings 只带路径、字段名与行号，绝不带密钥值或片段。

use serde::Serialize;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 已知前缀，命中即 `high`。
const KNOWN_PREFIXES: &[&str] = &[
    "sk-ant-",
    "sk-",
    "ghp_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxr-",
    "xoxs-",
    "AKIA",
    "AIza",
    "hf_",
    "bce-v3/",
    "oc_sk_",
];

/// 熵规则的阈值：长度与 Shannon 熵下限。
const MIN_ENTROPY_LENGTH: usize = 20;
const MIN_ENTROPY: f64 = 4.5;

/// 语义是摘要/校验值的字段，命中不扫描。
const EXCLUDED_FIELD_WORDS: &[&str] = &[
    "sha256",
    "sha1",
    "sha512",
    "integrity",
    "etag",
    "checksum",
    "digest",
    "hash",
];

/// 语义是密钥的字段名片段。
const SECRET_FIELD_WORDS: &[&str] = &[
    "apikey",
    "token",
    "secret",
    "password",
    "passwd",
    "authorization",
    "bearer",
    "credential",
    "privatekey",
];

/// 判定级别。只用于展示，不阻断导出。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretLevel {
    High,
    Low,
}

/// 一条命中。字段名与位置用于人工判断，值不出现。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretFinding {
    /// 快照内路径，形如 `snapshot/extensions/demo.ts`。
    pub path: String,
    pub field: Option<String>,
    pub line: usize,
    pub level: SecretLevel,
    /// 命中规则：`known_prefix` 或 `entropy`。
    pub rule: &'static str,
}

/// 扫描整个快照目录。只读；非 UTF-8 文件视为二进制跳过。
/// 返回的 findings 只含位置，调用方不得把值写进日志或错误信息。
pub fn scan_snapshot(snapshot_dir: &Path) -> io::Result<Vec<SecretFinding>> {
    let mut findings = Vec::new();
    for relative in collect_files(snapshot_dir)? {
        let bytes = fs::read(snapshot_dir.join(&relative))?;
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let label = format!("snapshot/{}", slash(&relative));
        findings.extend(scan_text(&label, &text));
    }
    Ok(findings)
}

fn collect_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    fn visit(root: &Path, relative: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
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
                files.push(child);
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    visit(root, Path::new(""), &mut files)?;
    Ok(files)
}

fn slash(relative: &Path) -> String {
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn is_reference(value: &str) -> bool {
    let value = value.trim();
    value.starts_with('!')
        || (value.starts_with("${") && value.ends_with('}'))
        || value.strip_prefix('$').is_some_and(is_env_name)
}

fn is_env_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn normalized_field(field: &str) -> String {
    field
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

fn is_excluded_field(field: &str) -> bool {
    let field = normalized_field(field);
    EXCLUDED_FIELD_WORDS.iter().any(|word| field.contains(word))
}

fn is_secret_field(field: &str) -> bool {
    let field = normalized_field(field);
    SECRET_FIELD_WORDS.iter().any(|word| field.contains(word))
}

/// 判定一条候选值。字段名只提升级别，不单独构成命中。
fn classify(field: Option<&str>, value: &str) -> Option<(SecretLevel, &'static str)> {
    if field.is_some_and(is_excluded_field) {
        return None;
    }
    let (_, _, rule) = detect(value)?;
    let level = if rule == "known_prefix" || field.is_some_and(is_secret_field) {
        SecretLevel::High
    } else {
        SecretLevel::Low
    };
    Some((level, rule))
}

/// 在值里找出最像密钥的一段：前缀优先，其次高熵 token。
fn detect(value: &str) -> Option<(usize, usize, &'static str)> {
    if is_reference(value) {
        return None;
    }
    let spans = token_spans(value);
    for &(start, end) in &spans {
        if KNOWN_PREFIXES
            .iter()
            .any(|prefix| value[start..end].starts_with(prefix))
        {
            return Some((start, end, "known_prefix"));
        }
    }
    for &(start, end) in &spans {
        if looks_random(&value[start..end]) {
            return Some((start, end, "entropy"));
        }
    }
    None
}

fn token_spans(value: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut begin: Option<usize> = None;
    for (index, ch) in value.char_indices() {
        let is_separator = ch.is_whitespace()
            || matches!(
                ch,
                '"' | '\'' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '=' | ':'
            );
        match (is_separator, begin) {
            (true, Some(start)) => {
                spans.push((start, index));
                begin = None;
            }
            (false, None) => begin = Some(index),
            _ => {}
        }
    }
    if let Some(start) = begin {
        spans.push((start, value.len()));
    }
    spans
}

fn looks_random(value: &str) -> bool {
    value.chars().count() >= MIN_ENTROPY_LENGTH
        && shannon_entropy(value) >= MIN_ENTROPY
        && !is_hex_digest(value)
        && !is_public_url(value)
        && !is_version_like(value)
}

fn shannon_entropy(value: &str) -> f64 {
    let mut counts: HashMap<char, usize> = HashMap::new();
    for ch in value.chars() {
        *counts.entry(ch).or_default() += 1;
    }
    let total = value.chars().count() as f64;
    counts
        .values()
        .map(|count| {
            let share = *count as f64 / total;
            -share * share.log2()
        })
        .sum()
}

fn is_hex_digest(value: &str) -> bool {
    value.len() >= 32 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn is_public_url(value: &str) -> bool {
    ["http://", "https://", "ftp://", "data:", "file:"]
        .iter()
        .any(|scheme| value.starts_with(scheme))
}

fn is_version_like(value: &str) -> bool {
    let trimmed = value.strip_prefix(['v', 'V']).unwrap_or(value);
    trimmed.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        && trimmed
            .chars()
            .all(|ch| ch.is_ascii_digit() || matches!(ch, '.' | '-' | '+' | '_'))
}

fn scan_text(label: &str, text: &str) -> Vec<SecretFinding> {
    let mut findings = Vec::new();
    for (index, line) in text.lines().enumerate() {
        for (field, value) in candidates(line) {
            let Some((level, rule)) = classify(field.as_deref(), &value) else {
                continue;
            };
            findings.push(SecretFinding {
                path: label.to_string(),
                field,
                line: index + 1,
                level,
                rule,
            });
        }
    }
    findings
}

/// 提取一行的 (字段名, 候选值)：先认 `"键": "值"`，其次认 `name = "值"` / `name: "值"`。
fn candidates(line: &str) -> Vec<(Option<String>, String)> {
    let spans = quoted_spans(line);
    let mut out = Vec::new();
    let mut index = 0;
    while index < spans.len() {
        let (start, end) = spans[index];
        if let Some(&(value_start, value_end)) = spans.get(index + 1) {
            // `value_start` 是值的开引号之后，比较时去掉开引号本身。
            if line[end + 1..value_start - 1].trim() == ":" {
                out.push((
                    Some(line[start..end].to_string()),
                    line[value_start..value_end].to_string(),
                ));
                index += 2;
                continue;
            }
        }
        out.push((field_before(line, start), line[start..end].to_string()));
        index += 1;
    }
    out
}

/// 引号串的内容区间（不含引号本身），`\"` 视为转义。
fn quoted_spans(line: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut begin: Option<usize> = None;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if begin.is_some() => escaped = true,
            '"' => match begin.take() {
                Some(start) => spans.push((start, index)),
                None => begin = Some(index + 1),
            },
            _ => {}
        }
    }
    spans
}

fn field_before(line: &str, start: usize) -> Option<String> {
    let prefix = line[..start]
        .trim_end()
        .trim_end_matches([':', '=', ' ', '\t']);
    let name: String = prefix
        .chars()
        .rev()
        .take_while(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
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
                "agent-config-sync-secrets-{}-{}-{}",
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

    fn levels(findings: &[SecretFinding]) -> Vec<(String, SecretLevel, &'static str)> {
        findings
            .iter()
            .map(|finding| (finding.path.clone(), finding.level, finding.rule))
            .collect()
    }

    // 高熵样本：非十六进制、非版本号，长度 > 20。
    const RANDOM: &str = "Zx7Qv2Lm9Rt4Bc8Wn3Yp6Kd1";

    #[test]
    fn known_prefixes_are_high() {
        for sample in [
            "sk-live-abcdefghijklmnop",
            "ghp_0123456789abcdefghij",
            "AKIAIOSFODNN7EXAMPLE",
            "hf_abcdefghijklmnopqrst",
        ] {
            let line = format!("apiKey = \"{sample}\"");
            let findings = scan_text("snapshot/extensions/demo.ts", &line);
            assert_eq!(findings.len(), 1, "{sample} 应命中");
            assert_eq!(findings[0].level, SecretLevel::High);
            assert_eq!(findings[0].rule, "known_prefix");
            assert_eq!(findings[0].line, 1);
        }
    }

    #[test]
    fn secret_field_with_entropy_is_high() {
        let text = format!("{{\"apiKey\": \"{RANDOM}\"}}");
        let findings = scan_text("snapshot/extensions/demo.json", &text);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].level, SecretLevel::High);
        assert_eq!(findings[0].rule, "entropy");
        assert_eq!(findings[0].field.as_deref(), Some("apiKey"));
    }

    #[test]
    fn entropy_without_secret_field_is_low() {
        let text = format!("note = \"{RANDOM}\"");
        let findings = scan_text("snapshot/AGENTS.md", &text);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].level, SecretLevel::Low);
    }

    #[test]
    fn hashes_versions_and_model_ids_are_not_findings() {
        let text = "\
{\
\"integrity\": \"sha512-0123456789abcdef0123456789abcdef0123456789abcdef\",\
\"sha256\": \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\",\
\"version\": \"1.2.3\",\
\"node\": \"22.23.0\",\
\"model\": \"gpt-4.1\",\
\"homepage\": \"https://example.com/docs/some/long/path/here\"\
}";
        let findings = scan_text("snapshot/settings.json", text);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn references_are_not_findings() {
        for text in [
            "{\"apiKey\": \"$OPENROUTER_API_KEY\"}",
            "{\"Authorization\": \"Bearer ${DOCS_TOKEN}\"}",
            "{\"exaApiKey\": \"!op read op://private/exa/api-key\"}",
        ] {
            assert!(scan_text("snapshot/models.json", text).is_empty(), "{text}");
        }
    }

    #[test]
    fn scan_snapshot_is_read_only_and_finds_auth() {
        let temp = TempDir::new("snapshot");
        let root = temp.path();
        let auth = format!("{{\"openai\": {{\"type\": \"api_key\", \"key\": \"sk-{RANDOM}\"}}}}");
        write_file(&root.join("auth.json"), &auth);
        write_file(
            &root.join("extensions/demo/index.ts"),
            &format!("const apiKey = \"sk-{RANDOM}\";\n"),
        );
        write_file(&root.join("skills/demo/SKILL.md"), "# 无密钥\n");
        write_file(&root.join("binary.bin"), "not utf8 placeholder");

        let findings = scan_snapshot(root).expect("扫描");
        assert_eq!(
            levels(&findings),
            vec![
                (
                    "snapshot/auth.json".to_string(),
                    SecretLevel::High,
                    "known_prefix"
                ),
                (
                    "snapshot/extensions/demo/index.ts".to_string(),
                    SecretLevel::High,
                    "known_prefix"
                ),
            ]
        );
        assert!(!format!("{findings:?}").contains(RANDOM));
        assert_eq!(fs::read_to_string(root.join("auth.json")).unwrap(), auth);
    }

    #[test]
    fn non_utf8_files_are_skipped() {
        let temp = TempDir::new("binary");
        let path = temp.path().join("binary.bin");
        fs::write(&path, [0xff, 0xfe, 0x00]).expect("写二进制");
        assert!(scan_snapshot(temp.path()).unwrap().is_empty());
    }

    #[test]
    fn field_helpers_normalize_case_and_separators() {
        assert!(is_secret_field("api_key"));
        assert!(is_secret_field("apiKey"));
        assert!(is_secret_field("Authorization"));
        assert!(!is_secret_field("model"));
        assert!(is_excluded_field("sha256"));
        assert!(is_excluded_field("integrity"));
        assert!(!is_excluded_field("apiKey"));
    }

    #[test]
    fn entropy_rule_ignores_short_and_low_entropy_values() {
        assert!(!looks_random("short"));
        assert!(!looks_random("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!looks_random("0123456789abcdef0123456789abcdef"));
        assert!(looks_random(RANDOM));
    }
}

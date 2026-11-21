//! 密钥扫描与引用化。
//!
//! 规则来源：`docs/design/30-secrets.md`。扫描对象是快照内的文本文件；
//! 自动改写只发生在 `models.json` / `mcp.json` / `web-search.json` 三个已知文件，
//! 其他文件命中高危时只返回 findings，由调用方决定是否继续。
//!
//! 铁律：findings 只带路径、字段名与行号，绝不带密钥值或片段。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
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

/// 语义是摘要/校验值的字段，命中不扫描也永不改写。
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

/// 判定级别。`high` 未确认时阻断导出。
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

/// manifest 的 `requiredSecrets` 条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequiredSecret {
    pub name: String,
    pub used_by: Vec<String>,
    /// 只放官方申请地址；本版不内置任何第三方站点。
    pub hint: Option<String>,
}

/// 一次脱敏的完整结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SanitizeOutcome {
    pub findings: Vec<SecretFinding>,
    pub required_secrets: Vec<RequiredSecret>,
    /// 仍有 `high` 级明文：未获显式确认时不能继续导出。
    pub blocked: bool,
}

/// 三个已知文件的引用语法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefStyle {
    /// `$NAME`：`models.json`、`web-search.json`。
    Dollar,
    /// `${NAME}`：`mcp.json`。
    Braces,
}

impl RefStyle {
    fn reference(self, name: &str) -> String {
        match self {
            Self::Dollar => format!("${name}"),
            Self::Braces => format!("${{{name}}}"),
        }
    }
}

fn style_for(relative: &Path) -> Option<RefStyle> {
    match relative.to_str()? {
        "models.json" | "web-search.json" => Some(RefStyle::Dollar),
        "mcp.json" => Some(RefStyle::Braces),
        _ => None,
    }
}

/// 扫描并引用化整个快照目录。只读范围内的条目，写入仅限三个已知文件。
pub fn sanitize_snapshot(snapshot_dir: &Path) -> io::Result<SanitizeOutcome> {
    let mut findings = Vec::new();
    let mut required: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut namer = Namer::default();

    for relative in collect_files(snapshot_dir)? {
        let path = snapshot_dir.join(&relative);
        let label = format!("snapshot/{}", slash(&relative));
        let bytes = fs::read(&path)?;
        // 非 UTF-8 视为二进制：不是文本配置，不扫描也不改写。
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };

        match style_for(&relative) {
            Some(style) => {
                match rewrite_document(&text, style, &label, &mut namer, &mut required) {
                    // 解析失败时不猜结构：保留原文，交给扫描结果决定是否阻断。
                    None => findings.extend(scan_text(&label, &text)),
                    Some(updated) => {
                        if updated != text {
                            fs::write(&path, updated)?;
                        }
                    }
                }
            }
            None => findings.extend(scan_text(&label, &text)),
        }
    }

    let blocked = findings
        .iter()
        .any(|finding| finding.level == SecretLevel::High);

    Ok(SanitizeOutcome {
        findings,
        required_secrets: required
            .into_iter()
            .map(|(name, used_by)| RequiredSecret {
                name,
                used_by,
                hint: None,
            })
            .collect(),
        blocked,
    })
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

/// 引用名生成：同一个值复用同一个名字，名字冲突时追加序号。
#[derive(Default)]
struct Namer {
    by_value: HashMap<String, String>,
    used: HashSet<String>,
}

impl Namer {
    fn name_for(&mut self, secret: &str, keys: &[String]) -> String {
        if let Some(existing) = self.by_value.get(secret) {
            return existing.clone();
        }

        let base = derive_name(keys);
        let mut candidate = base.clone();
        let mut suffix = 2;
        while self.used.contains(&candidate) {
            candidate = format!("{base}_{suffix}");
            suffix += 1;
        }

        self.used.insert(candidate.clone());
        self.by_value.insert(secret.to_string(), candidate.clone());
        candidate
    }
}

/// `PI_` + 最近的归属键（provider / server 等）+ 字段名，转大写 SNAKE。
fn derive_name(keys: &[String]) -> String {
    const CONTAINERS: &[&str] = &["providers", "mcpservers", "headers", "servers", "env"];

    let (field, ancestors) = match keys.split_last() {
        Some((field, ancestors)) => (field.as_str(), ancestors),
        None => ("secret", &[][..]),
    };
    let owner = ancestors
        .iter()
        .rev()
        .find(|key| !CONTAINERS.contains(&key.to_lowercase().as_str()));

    let mut parts = Vec::new();
    if let Some(owner) = owner {
        parts.push(snake_upper(owner));
    }
    parts.push(snake_upper(field));
    format!("PI_{}", parts.join("_"))
}

fn snake_upper(value: &str) -> String {
    let mut out = String::new();
    let mut previous_lower = false;
    for ch in value.chars() {
        if ch.is_alphanumeric() {
            if ch.is_uppercase() && previous_lower {
                out.push('_');
            }
            out.push(ch.to_ascii_uppercase());
            previous_lower = ch.is_lowercase() || ch.is_ascii_digit();
        } else {
            if !out.is_empty() && !out.ends_with('_') {
                out.push('_');
            }
            previous_lower = false;
        }
    }
    out.trim_matches('_').to_string()
}

fn rewrite_document(
    text: &str,
    style: RefStyle,
    label: &str,
    namer: &mut Namer,
    required: &mut BTreeMap<String, Vec<String>>,
) -> Option<String> {
    let mut document: Value = serde_json::from_str(text).ok()?;
    let changed = rewrite_value(
        &mut document,
        &mut Vec::new(),
        style,
        label,
        namer,
        required,
    );
    // 没有替换就不动原文：格式化本身不是变更，否则刚导出的快照会立刻显示“修改”。
    if !changed {
        return Some(text.to_string());
    }
    let mut output = serde_json::to_string_pretty(&document).ok()?;
    output.push('\n');
    Some(output)
}

fn rewrite_value(
    value: &mut Value,
    keys: &mut Vec<String>,
    style: RefStyle,
    label: &str,
    namer: &mut Namer,
    required: &mut BTreeMap<String, Vec<String>>,
) -> bool {
    match value {
        Value::Object(map) => {
            let mut changed = false;
            for (key, child) in map.iter_mut() {
                keys.push(key.clone());
                match child {
                    Value::String(text) => {
                        // 用户已写的引用原样保留，只登记环境变量名。
                        for name in extract_references(text) {
                            record_secret(required, &name, label);
                        }
                        let high = classify(Some(key), text)
                            .is_some_and(|(level, _)| level == SecretLevel::High);
                        if let (true, Some((start, end, _))) = (high, detect(text)) {
                            let name = namer.name_for(&text[start..end], keys);
                            let reference = style.reference(&name);
                            text.replace_range(start..end, &reference);
                            record_secret(required, &name, label);
                            changed = true;
                        }
                    }
                    other => changed |= rewrite_value(other, keys, style, label, namer, required),
                }
                keys.pop();
            }
            changed
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items.iter_mut() {
                changed |= rewrite_value(item, keys, style, label, namer, required);
            }
            changed
        }
        _ => false,
    }
}

fn record_secret(required: &mut BTreeMap<String, Vec<String>>, name: &str, label: &str) {
    let used_by = required.entry(name.to_string()).or_default();
    if !used_by.iter().any(|item| item == label) {
        used_by.push(label.to_string());
        used_by.sort();
    }
}

/// 提取已存在的引用里的环境变量名：`$NAME`、`${NAME}`、内嵌 `${NAME}`。
fn extract_references(value: &str) -> Vec<String> {
    let value = value.trim();
    let mut names = Vec::new();
    // `!command` 是命令引用，不是环境变量，导出端无从登记。
    if value.starts_with('!') {
        return names;
    }

    if let Some(name) = value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
    {
        if is_env_name(name) {
            names.push(name.to_string());
            return names;
        }
    }
    if let Some(name) = value.strip_prefix('$') {
        if is_env_name(name) {
            names.push(name.to_string());
            return names;
        }
    }

    let mut rest = value;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else { break };
        let name = &after[..end];
        if is_env_name(name) {
            names.push(name.to_string());
        }
        rest = &after[end + 1..];
    }
    names
}

fn is_env_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn is_reference(value: &str) -> bool {
    let value = value.trim();
    value.starts_with('!')
        || (value.starts_with("${") && value.ends_with('}'))
        || value.strip_prefix('$').is_some_and(is_env_name)
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

/// 判定一条候选值。字段名只提升级别，不单独构成命中（见 30-secrets §4 分级）。
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

    fn read_file(path: &Path) -> String {
        fs::read_to_string(path).expect("读取夹具文件")
    }

    fn levels(findings: &[SecretFinding]) -> Vec<(String, SecretLevel, &'static str)> {
        findings
            .iter()
            .map(|finding| (finding.path.clone(), finding.level, finding.rule))
            .collect()
    }

    fn names(outcome: &SanitizeOutcome) -> Vec<String> {
        outcome
            .required_secrets
            .iter()
            .map(|secret| secret.name.clone())
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
    fn three_reference_syntaxes_are_applied_per_file() {
        let temp = TempDir::new("references");
        let root = temp.path();
        write_file(
            &root.join("models.json"),
            &format!("{{\"providers\": {{\"openrouter\": {{\"apiKey\": \"sk-or-{RANDOM}\"}}}}}}"),
        );
        write_file(
            &root.join("mcp.json"),
            &format!("{{\"mcpServers\": {{\"docs\": {{\"headers\": {{\"Authorization\": \"Bearer sk-{RANDOM}\"}}}}}}}}"),
        );
        write_file(
            &root.join("web-search.json"),
            &format!("{{\"exaApiKey\": \"{RANDOM}\"}}"),
        );

        let outcome = sanitize_snapshot(root).expect("脱敏");
        assert!(!outcome.blocked, "{:?}", outcome.findings);

        let models = read_file(&root.join("models.json"));
        assert!(
            models.contains("\"apiKey\": \"$PI_OPENROUTER_API_KEY\""),
            "{models}"
        );
        assert!(!models.contains(RANDOM));

        let mcp = read_file(&root.join("mcp.json"));
        assert!(
            mcp.contains("\"Authorization\": \"Bearer ${PI_DOCS_AUTHORIZATION}\""),
            "{mcp}"
        );

        let web = read_file(&root.join("web-search.json"));
        assert!(web.contains("\"exaApiKey\": \"$PI_EXA_API_KEY\""), "{web}");

        assert_eq!(
            names(&outcome),
            vec![
                "PI_DOCS_AUTHORIZATION",
                "PI_EXA_API_KEY",
                "PI_OPENROUTER_API_KEY"
            ]
        );
        assert_eq!(
            outcome.required_secrets[2].used_by,
            vec!["snapshot/models.json".to_string()]
        );
    }

    #[test]
    fn existing_references_are_kept_and_registered() {
        let temp = TempDir::new("keep-references");
        let root = temp.path();
        write_file(
            &root.join("models.json"),
            "{\"providers\": {\"openrouter\": {\"apiKey\": \"$OPENROUTER_API_KEY\"}}}",
        );
        write_file(
            &root.join("mcp.json"),
            "{\"mcpServers\": {\"docs\": {\"headers\": {\"Authorization\": \"Bearer ${DOCS_TOKEN}\"}}}}",
        );
        write_file(
            &root.join("web-search.json"),
            "{\"exaApiKey\": \"!op read op://private/exa/api-key\"}",
        );

        let outcome = sanitize_snapshot(root).expect("脱敏");
        assert!(!outcome.blocked, "{:?}", outcome.findings);
        assert!(outcome.findings.is_empty());

        assert!(read_file(&root.join("models.json")).contains("\"$OPENROUTER_API_KEY\""));
        assert!(read_file(&root.join("mcp.json")).contains("\"Bearer ${DOCS_TOKEN}\""));
        assert!(read_file(&root.join("web-search.json"))
            .contains("\"!op read op://private/exa/api-key\""));
        assert_eq!(names(&outcome), vec!["DOCS_TOKEN", "OPENROUTER_API_KEY"]);
    }

    #[test]
    fn untouched_documents_keep_their_original_bytes() {
        let temp = TempDir::new("keep-bytes");
        let root = temp.path();
        let original = "{\"providers\":{\"openrouter\":{\"apiKey\":\"$OPENROUTER_API_KEY\"}}}";
        write_file(&root.join("models.json"), original);

        let outcome = sanitize_snapshot(root).expect("脱敏");
        assert_eq!(read_file(&root.join("models.json")), original);
        assert_eq!(names(&outcome), vec!["OPENROUTER_API_KEY"]);
    }

    #[test]
    fn same_value_reuses_one_name_and_used_by_is_deduped() {
        let temp = TempDir::new("dedupe");
        let root = temp.path();
        write_file(
            &root.join("models.json"),
            &format!("{{\"providers\": {{\"openrouter\": {{\"apiKey\": \"{RANDOM}\"}}}}}}"),
        );
        write_file(
            &root.join("web-search.json"),
            &format!("{{\"exaApiKey\": \"{RANDOM}\", \"openrouterApiKey\": \"{RANDOM}\"}}"),
        );

        let outcome = sanitize_snapshot(root).expect("脱敏");
        // 同一个值只对应一个引用名；同一个文件里出现两次不重复登记 `usedBy`。
        let referenced = outcome
            .required_secrets
            .iter()
            .filter(|secret| secret.name.starts_with("PI_"))
            .count();
        assert_eq!(referenced, 1, "{:?}", outcome.required_secrets);
        let secret = &outcome.required_secrets[0];
        assert_eq!(secret.used_by.len(), 2);
        assert_eq!(
            secret.used_by,
            vec![
                "snapshot/models.json".to_string(),
                "snapshot/web-search.json".to_string()
            ]
        );
    }

    #[test]
    fn high_findings_outside_known_files_block_and_are_not_rewritten() {
        let temp = TempDir::new("block");
        let root = temp.path();
        let source = format!("const apiKey = \"sk-{RANDOM}\";\n");
        write_file(&root.join("extensions/demo/index.ts"), &source);

        let outcome = sanitize_snapshot(root).expect("脱敏");
        assert!(outcome.blocked);
        assert_eq!(
            levels(&outcome.findings),
            vec![(
                "snapshot/extensions/demo/index.ts".to_string(),
                SecretLevel::High,
                "known_prefix"
            )]
        );
        // 非已知文件不自动改写，值也不会出现在 findings 里。
        assert_eq!(read_file(&root.join("extensions/demo/index.ts")), source);
        assert!(!format!("{:?}", outcome.findings).contains(RANDOM));
        assert!(outcome.required_secrets.is_empty());
    }

    #[test]
    fn unparsable_known_file_is_scanned_not_rewritten() {
        let temp = TempDir::new("json5");
        let root = temp.path();
        // JSON5 风格的注释：本版不解析，也就不改写，只按文本扫描拦截。
        write_file(
            &root.join("models.json"),
            &format!("// 注释\n{{\"apiKey\": \"sk-{RANDOM}\"}}\n"),
        );

        let outcome = sanitize_snapshot(root).expect("脱敏");
        assert!(outcome.blocked);
        assert_eq!(outcome.findings.len(), 1);
        assert!(read_file(&root.join("models.json")).contains(RANDOM));
    }

    #[test]
    fn names_are_stable_and_conflicts_get_suffixes() {
        let temp = TempDir::new("conflict");
        let root = temp.path();
        write_file(
            &root.join("models.json"),
            &format!(
                "{{\"providers\": {{\"a\": {{\"apiKey\": \"sk-one-{RANDOM}\"}}, \"b\": {{\"apiKey\": \"sk-two-{RANDOM}\"}}}}}}"
            ),
        );

        let outcome = sanitize_snapshot(root).expect("脱敏");
        assert_eq!(
            names(&outcome),
            vec!["PI_A_API_KEY", "PI_B_API_KEY"],
            "不同归属键应产生不同名字"
        );

        // 归属键相同但值不同：追加序号；同一个值复用同一个名字。
        let mut namer = Namer::default();
        let keys = vec![
            "providers".to_string(),
            "openrouter".to_string(),
            "apiKey".to_string(),
        ];
        assert_eq!(namer.name_for("one", &keys), "PI_OPENROUTER_API_KEY");
        assert_eq!(namer.name_for("two", &keys), "PI_OPENROUTER_API_KEY_2");
        assert_eq!(namer.name_for("one", &keys), "PI_OPENROUTER_API_KEY");
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

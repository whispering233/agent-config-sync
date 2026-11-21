//! 最新版本查询：把 npm registry 的 latest 结果缓存到应用配置目录。
//!
//! 只有用户点「检查更新」才发网络请求；查询最多两个并发，10 秒超时，
//! 不重试。缓存只含版本号与提示，不含命令输出或凭据。

use crate::doctor::{CommandOutcome, CommandRunner};
use crate::platform::{self, HostOs};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 应用配置目录下的缓存文件名。
pub const CACHE_FILE_NAME: &str = "versions-cache.json";
/// 缓存超过这个时间会在报告里标成过期，但不会自动联网刷新。
pub const CACHE_TTL_SECONDS: u64 = 6 * 60 * 60;
/// 单次 `npm view` 的超时。
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
/// Pi CLI 的 npm 包名。
pub const PI_PACKAGE: &str = "@earendil-works/pi-coding-agent";

/// 一个包的最新版本查询结果。`version` 为空时看 `note`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestVersion {
    pub version: Option<String>,
    pub note: Option<String>,
}

impl LatestVersion {
    fn available(version: String) -> Self {
        Self {
            version: Some(version),
            note: None,
        }
    }

    fn note(text: &str) -> Self {
        Self {
            version: None,
            note: Some(text.to_string()),
        }
    }
}

/// 报告里的最新版本区块。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestVersions {
    /// 上次成功查询的时间；从未查询过为 `None`。
    pub checked_at: Option<String>,
    /// 缓存超过 [`CACHE_TTL_SECONDS`]；仍展示，但提示用户刷新。
    pub stale: bool,
    pub pi: Option<LatestVersion>,
    /// key 是 `settings.json` / `pi list` 里的来源（如 `npm:pi-lens`）。
    pub packages: BTreeMap<String, LatestVersion>,
    /// 本次刷新失败、退回旧缓存时的说明。
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CacheFile {
    checked_at: String,
    checked_at_unix: u64,
    pi: Option<LatestVersion>,
    packages: BTreeMap<String, LatestVersion>,
}

/// 读缓存；文件不存在或损坏都按「还没查过」处理，不影响环境检测。
pub fn load_cache(path: &Path) -> Option<LatestVersions> {
    let text = fs::read_to_string(path).ok()?;
    let cache: CacheFile = serde_json::from_str(&text).ok()?;
    let age = now_seconds().saturating_sub(cache.checked_at_unix);

    Some(LatestVersions {
        checked_at: Some(cache.checked_at),
        stale: age > CACHE_TTL_SECONDS,
        pi: cache.pi,
        packages: cache.packages,
        error: None,
    })
}

fn save_cache(path: &Path, latest: &LatestVersions) {
    let Some(checked_at) = latest.checked_at.clone() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let cache = CacheFile {
        checked_at,
        checked_at_unix: now_seconds(),
        pi: latest.pi.clone(),
        packages: latest.packages.clone(),
    };
    if let Ok(bytes) = serde_json::to_vec_pretty(&cache) {
        let _ = fs::write(path, bytes);
    }
}

/// 按来源清单查询最新版本。非 npm 源直接标「不适用」，不启动子进程。
pub fn load_or_query(
    cache_path: &Path,
    sources: &[String],
    runner: &(dyn CommandRunner + Sync),
    host: HostOs,
    refresh: bool,
) -> LatestVersions {
    let cached = load_cache(cache_path);
    if !refresh {
        return cached.unwrap_or_default();
    }

    let pi = Some(query_one(runner, host, PI_PACKAGE));
    let packages = query_packages(sources, runner, host);
    let has_version = pi.as_ref().is_some_and(|item| item.version.is_some())
        || packages.values().any(|item| item.version.is_some());

    // 全部查询都失败时保留上一次成功结果；失败原因写在 error 里，不覆盖好数据。
    if !has_version {
        if let Some(mut previous) = cached {
            if previous.pi.is_some() || !previous.packages.is_empty() {
                previous.stale = true;
                previous.error = Some("查询最新版本失败，显示上次缓存".to_string());
                return previous;
            }
        }
    }

    let latest = LatestVersions {
        checked_at: Some(crate::snapshot::format_timestamp(now_seconds())),
        stale: false,
        pi,
        packages,
        error: None,
    };
    save_cache(cache_path, &latest);
    latest
}

fn query_packages(
    sources: &[String],
    runner: &(dyn CommandRunner + Sync),
    host: HostOs,
) -> BTreeMap<String, LatestVersion> {
    let mut result = BTreeMap::new();
    let mut pending = Vec::new();

    for source in sources {
        // 来源来自用户 settings.json：不要把带 token、参数或空白的字符串写进缓存。
        if !is_safe_source(source) {
            continue;
        }
        let Some((package, spec)) = npm_source(source) else {
            result.insert(source.clone(), LatestVersion::note("不适用（非 npm 源）"));
            continue;
        };
        if let Some(spec) = spec {
            if !is_exact_release(spec) {
                result.insert(
                    source.clone(),
                    LatestVersion::note("未固定版本，不查询最新"),
                );
                continue;
            }
            pending.push((source.clone(), package.to_string(), Some(spec.to_string())));
        } else {
            pending.push((source.clone(), package.to_string(), None));
        }
    }

    // 最多同时跑两个 npm 进程；结果按来源逐项取回，部分失败不影响其余。
    for chunk in pending.chunks(2) {
        let results = Arc::new(Mutex::new(Vec::new()));
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for (source, package, spec) in chunk {
                let results = Arc::clone(&results);
                handles.push(scope.spawn(move || {
                    let mut item = query_one(runner, host, package);
                    if item.note.is_none() {
                        if let Some(spec) = spec {
                            item.note = Some(format!("已固定 {spec}"));
                        }
                    }
                    results
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push((source.clone(), item));
                }));
            }
            for handle in handles {
                let _ = handle.join();
            }
        });

        for (source, item) in results
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
        {
            // 声明里可能写成 `npm:pi-lens@1.2.3`，而 `pi list` 只有 `npm:pi-lens`：
            // 精确版本额外挂一份无版本别名，实装列表也能看到最新版。
            if let Some((package, _)) = npm_source(&source) {
                let normalized = format!("npm:{package}");
                if normalized != source {
                    result.insert(normalized, item.clone());
                }
            }
            result.insert(source, item);
        }
    }

    result
}

fn query_one(runner: &dyn CommandRunner, host: HostOs, package: &str) -> LatestVersion {
    let outcome = match host {
        HostOs::Posix => {
            let script = format!("npm view {} version", platform::sh_quote(package));
            runner.run(&script, QUERY_TIMEOUT, &[])
        }
        HostOs::Windows => {
            match platform::program_argv(HostOs::Windows, "npm", &["view", package, "version"]) {
                Ok(argv) => runner.run_program(&argv, QUERY_TIMEOUT, &[]),
                Err(detail) => CommandOutcome::Unavailable(detail),
            }
        }
    };

    match outcome {
        CommandOutcome::Ok { stdout, .. } => from_stdout(&stdout),
        CommandOutcome::Failed {
            code,
            stdout,
            stderr,
        } => {
            if let Some(version) = parse_exact_release(&stdout) {
                return LatestVersion::available(version);
            }
            let text = stderr.to_lowercase();
            if text.contains("e404") || code == Some(404) {
                LatestVersion::note("未发布或包名不存在")
            } else if text.contains("e401")
                || text.contains("e403")
                || code == Some(401)
                || code == Some(403)
            {
                LatestVersion::note("npm 认证失败，未知")
            } else {
                LatestVersion::note("查询失败，未知")
            }
        }
        CommandOutcome::Timeout => LatestVersion::note("查询超时（10 秒），未知"),
        CommandOutcome::Unavailable(detail) => {
            LatestVersion::note(&format!("无法启动 npm：{detail}"))
        }
    }
}

fn from_stdout(stdout: &str) -> LatestVersion {
    match parse_exact_release(stdout) {
        Some(version) => LatestVersion::available(version),
        None => LatestVersion::note("版本号无法解析"),
    }
}

/// 严格取 `x.y.z`，避免把 npm 的提示文字当成版本。
pub fn parse_exact_release(text: &str) -> Option<String> {
    let first = text
        .lines()
        .next()?
        .trim()
        .strip_prefix('v')
        .unwrap_or(text.lines().next()?.trim());
    let parts: Vec<&str> = first.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    Some(first.to_string())
}

/// `npm:` 来源拆成包名与可选版本/范围。非 npm 来源返回 `None`。
fn npm_source(source: &str) -> Option<(&str, Option<&str>)> {
    let body = source.strip_prefix("npm:")?;
    if body.is_empty() {
        return None;
    }
    let scope_end = if body.starts_with('@') {
        body.find('/')? + 1
    } else {
        0
    };
    let (package, spec) = match body[scope_end..].find('@') {
        Some(index) => {
            let separator = scope_end + index;
            (&body[..separator], Some(&body[separator + 1..]))
        }
        None => (body, None),
    };
    if !valid_package_name(package) || spec.is_some_and(|spec| spec.is_empty()) {
        return None;
    }
    Some((package, spec))
}

/// npm 包名只允许字母数字与 `-_.`，作用域包恰好一个 `/`；其余一律不查、不缓存。
fn valid_package_name(name: &str) -> bool {
    let (scope, package) = match name.strip_prefix('@') {
        Some(rest) => match rest.split_once('/') {
            Some((scope, package)) => (Some(scope), package),
            None => return false,
        },
        None => (None, name),
    };
    let valid = |value: &str| {
        !value.is_empty()
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    package.contains('/') == false && valid(package) && scope.is_none_or(valid)
}

/// 缓存 key 来自用户文件：含空白、`=` 或已知密钥标记的来源直接跳过。
fn is_safe_source(source: &str) -> bool {
    source.len() <= 200
        && !source.chars().any(|c| c.is_whitespace() || c == '=')
        && crate::doctor::redact(source) == source
}

fn is_exact_release(text: &str) -> bool {
    parse_exact_release(text).is_some()
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeRunner {
        outcomes: Mutex<Vec<CommandOutcome>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeRunner {
        fn new(outcomes: Vec<CommandOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, script: &str, _timeout: Duration, _env: &[(&str, String)]) -> CommandOutcome {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(script.to_string());
            self.outcomes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(0)
        }

        fn run_program(
            &self,
            argv: &[String],
            _timeout: Duration,
            _env: &[(&str, String)],
        ) -> CommandOutcome {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(argv.join(" "));
            self.outcomes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(0)
        }
    }

    fn ok(stdout: &str) -> CommandOutcome {
        CommandOutcome::Ok {
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    #[test]
    fn parses_exact_versions_only() {
        assert_eq!(parse_exact_release("1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(parse_exact_release("v1.2.3\n").as_deref(), Some("1.2.3"));
        assert_eq!(parse_exact_release("^1.2.3"), None);
        assert_eq!(parse_exact_release("1.2"), None);
        assert_eq!(parse_exact_release("1.2.3-beta"), None);
    }

    #[test]
    fn npm_sources_split_package_and_spec() {
        assert_eq!(npm_source("npm:pi-lens"), Some(("pi-lens", None)));
        assert_eq!(
            npm_source("npm:@scope/pkg@1.2.3"),
            Some(("@scope/pkg", Some("1.2.3")))
        );
        assert_eq!(
            npm_source("npm:pi-lens@^1.2.3"),
            Some(("pi-lens", Some("^1.2.3")))
        );
        assert_eq!(npm_source("git:github.com/u/r"), None);
        assert_eq!(npm_source("npm:foo/bar"), None);
    }

    #[test]
    fn unsafe_sources_are_not_cached_or_queried() {
        let runner = FakeRunner::new(Vec::new());
        let result = query_packages(
            &[
                "npm:token=sk-live-9999".to_string(),
                "git:token=sk-live-9999".to_string(),
            ],
            &runner,
            HostOs::Posix,
        );
        assert!(result.is_empty());
    }

    #[test]
    fn fixed_and_range_sources_do_not_hide_latest_queries() {
        let runner = FakeRunner::new(vec![ok("2.0.0\n"), ok("3.0.0\n")]);
        let sources = vec![
            "npm:pi-lens".to_string(),
            "npm:pi-old@1.2.3".to_string(),
            "npm:pi-range@^1.0.0".to_string(),
            "git:github.com/u/r".to_string(),
        ];
        let result = query_packages(&sources, &runner, HostOs::Posix);

        assert_eq!(result["npm:pi-lens"].version.as_deref(), Some("2.0.0"));
        assert_eq!(result["npm:pi-old@1.2.3"].version.as_deref(), Some("3.0.0"));
        assert_eq!(
            result["npm:pi-old@1.2.3"].note.as_deref(),
            Some("已固定 1.2.3")
        );
        assert_eq!(
            result["npm:pi-range@^1.0.0"].note.as_deref(),
            Some("未固定版本，不查询最新")
        );
        assert_eq!(
            result["git:github.com/u/r"].note.as_deref(),
            Some("不适用（非 npm 源）")
        );
    }

    #[test]
    fn failed_refresh_keeps_the_previous_cache() {
        let path = std::env::temp_dir().join(format!(
            "agent-config-sync-versions-{}-{}.json",
            std::process::id(),
            now_seconds()
        ));
        let _ = fs::remove_file(&path);
        save_cache(
            &path,
            &LatestVersions {
                checked_at: Some("2026-11-20T00:00:00Z".to_string()),
                stale: false,
                pi: Some(LatestVersion::available("1.0.0".to_string())),
                packages: BTreeMap::new(),
                error: None,
            },
        );
        let runner = FakeRunner::new(vec![CommandOutcome::Timeout]);

        let result = load_or_query(&path, &[], &runner, HostOs::Posix, true);

        assert_eq!(
            result.pi.as_ref().and_then(|item| item.version.as_deref()),
            Some("1.0.0")
        );
        assert!(result.error.is_some());
        let _ = fs::remove_file(path);
    }
}

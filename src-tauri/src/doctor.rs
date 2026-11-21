//! 只读环境医生：探测 node / npm / pi / 凭据 / 包 / 技能链接，输出分级报告。
//!
//! 规则来源：`docs/design/20-env-doctor.md`。核心逻辑不依赖 Tauri，
//! shell 命令通过 [`CommandRunner`] 注入，单测用假命令替代真实进程。
//! 全程只读：只读文件与环境，不写文件、不改环境变量、不安装任何东西。

use crate::config::{self, AGENT_DIR_ENV_VAR};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 普通探测的超时。
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// `pi auth check` 的超时：它可能要走一次网络往返。
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
/// 宿主与安装器要求的 Node 最低版本。
pub const MIN_NODE_VERSION: Version = (22, 19, 0);

/// 元组形式的语义化版本。
pub type Version = (u32, u32, u32);

/// 段内「命令不存在」标记，由 shell 片段自己打印。
const MISSING_MARKER: &str = "__PI_DOCTOR_MISSING__";
const SECTION_BEGIN: &str = "__PI_DOCTOR__BEGIN__";
const SECTION_END: &str = "__PI_DOCTOR__END__";

/// 检查状态，四态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
    Unknown,
}

/// 一条检查结果。`action` 是建议动作的 id，不含命令文本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub id: String,
    pub status: CheckStatus,
    pub value: Option<String>,
    pub detail: Option<String>,
    pub action: Option<String>,
}

/// 报告里的平台信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Platform {
    pub os: String,
    pub arch: String,
}

/// 一次检查的完整报告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    pub generated_at: String,
    pub platform: Platform,
    pub checks: Vec<Check>,
}

/// 一次 shell 命令的执行结果。`Failed` 也保留 stdout，便于取回已打印的探测段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    Ok {
        stdout: String,
        stderr: String,
    },
    Failed {
        code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    Timeout,
    /// 连 shell 都起不来（例如 `$SHELL` 指向不存在的程序）。
    Unavailable(String),
}

/// 命令执行器。生产用 [`ShellRunner`]，单测用假实现。
pub trait CommandRunner {
    fn run(&self, script: &str, timeout: Duration, env: &[(&str, String)]) -> CommandOutcome;
}

/// 通过登录 shell 执行命令：GUI 进程的 PATH 通常不含 nvm/fnm/mise 的路径。
pub struct ShellRunner {
    shell: OsString,
}

impl ShellRunner {
    /// `$SHELL -lc`；没有该变量时退回 `sh`。
    ///
    /// shortcut: Windows 需要 `cmd /C`，v1 只支持 Linux，按 POSIX 处理。
    pub fn from_env() -> Self {
        Self {
            shell: std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("sh")),
        }
    }
}

impl CommandRunner for ShellRunner {
    fn run(&self, script: &str, timeout: Duration, env: &[(&str, String)]) -> CommandOutcome {
        let mut command = Command::new(&self.shell);
        command
            .arg("-lc")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in env {
            command.env(key, value);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return CommandOutcome::Unavailable(error.to_string()),
        };

        // 并行读两个管道：边读边轮询，避免子进程写满管道后卡住把超时拖成死锁。
        let stdout_reader = child.stdout.take().map(spawn_reader);
        let stderr_reader = child.stderr.take().map(spawn_reader);
        let deadline = Instant::now() + timeout;
        let mut timed_out = false;

        let exit = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        timed_out = true;
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return CommandOutcome::Unavailable(error.to_string());
                }
            }
        };

        let stdout = join_reader(stdout_reader);
        let stderr = join_reader(stderr_reader);

        if timed_out {
            return CommandOutcome::Timeout;
        }

        match exit {
            Some(status) if status.success() => CommandOutcome::Ok { stdout, stderr },
            Some(status) => CommandOutcome::Failed {
                code: status.code(),
                stdout,
                stderr,
            },
            None => CommandOutcome::Timeout,
        }
    }
}

fn spawn_reader(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buffer = String::new();
        let _ = pipe.read_to_string(&mut buffer);
        buffer
    })
}

fn join_reader(handle: Option<std::thread::JoinHandle<String>>) -> String {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

/// 一段探测：前后各打印一个哨兵，shell 启动脚本的噪音落在哨兵之外。
fn section(id: &str, body: &str) -> String {
    format!("printf '\\n{SECTION_BEGIN}{id}__\\n'\n{body}\nprintf '\\n{SECTION_END}{id}__\\n'")
}

/// 解析哨兵分隔的多段输出，返回 `id → 段内容`。哨兵外的内容一律丢弃。
pub fn parse_sections(stdout: &str) -> BTreeMap<String, String> {
    let mut sections = BTreeMap::new();
    let mut open: Option<(String, Vec<&str>)> = None;

    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix(SECTION_BEGIN) {
            open = Some((rest.trim_end_matches('_').to_string(), Vec::new()));
        } else if let Some(rest) = line.strip_prefix(SECTION_END) {
            if let Some((id, lines)) = open.take() {
                if rest.trim_end_matches('_') == id {
                    sections.insert(id, lines.join("\n").trim().to_string());
                }
            }
        } else if let Some((_, lines)) = open.as_mut() {
            lines.push(line);
        }
    }

    sections
}

/// 把 `key:value` 行解析成字段；`list:` 之后的行原样收进 `list`。
fn parse_fields(body: Option<&str>) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let Some(body) = body else {
        return fields;
    };
    let mut list: Option<Vec<&str>> = None;

    for line in body.lines() {
        if let Some(lines) = list.as_mut() {
            lines.push(line);
        } else if line == "list:" {
            list = Some(Vec::new());
        } else if let Some((key, value)) = line.split_once(':') {
            fields.insert(key.trim().to_string(), value.trim().to_string());
        }
    }

    if let Some(lines) = list {
        fields.insert("list".to_string(), lines.join("\n").trim().to_string());
    }

    fields
}

/// 一段探测的结果视图。
enum SectionData {
    /// 段内容原文与解析出的字段；凭据检查只读原文（JSON），不走字段解析。
    Body {
        raw: String,
        fields: BTreeMap<String, String>,
    },
    /// 命令不存在（段内打印了 `MISSING_MARKER`）。
    Missing,
    Timeout,
    Unavailable(String),
    /// 命令非零退出且没有打印该段。
    Absent,
}

fn section_data(outcome: &CommandOutcome, id: &str) -> SectionData {
    match outcome {
        CommandOutcome::Timeout => SectionData::Timeout,
        CommandOutcome::Unavailable(detail) => SectionData::Unavailable(detail.clone()),
        CommandOutcome::Ok { stdout, .. } | CommandOutcome::Failed { stdout, .. } => {
            match parse_sections(stdout).get(id) {
                Some(body) if body.trim() == MISSING_MARKER => SectionData::Missing,
                Some(body) => SectionData::Body {
                    raw: body.clone(),
                    fields: parse_fields(Some(body)),
                },
                None => SectionData::Absent,
            }
        }
    }
}

fn record(
    id: &str,
    status: CheckStatus,
    value: Option<String>,
    detail: Option<String>,
    action: Option<&str>,
) -> Check {
    Check {
        id: id.to_string(),
        status,
        value: value.map(|text| redact(&text)),
        detail: detail.map(|text| redact(&text)),
        action: action.map(str::to_string),
    }
}

fn pass(id: &str, value: Option<String>, detail: Option<String>) -> Check {
    record(id, CheckStatus::Pass, value, detail, None)
}

fn fail(id: &str, detail: &str, action: Option<&str>) -> Check {
    record(
        id,
        CheckStatus::Fail,
        None,
        Some(detail.to_string()),
        action,
    )
}

fn unknown(id: &str, detail: &str) -> Check {
    record(
        id,
        CheckStatus::Unknown,
        None,
        Some(detail.to_string()),
        None,
    )
}

/// 快速组：一条登录 shell 命令里跑全部 5 秒级探测。
fn fast_script() -> String {
    let node = r#"n="$(command -v node 2>/dev/null)"
if [ -z "$n" ]; then
  for c in "$HOME"/.nvm/versions/node/*/bin/node "$HOME"/.local/share/fnm/aliases/default/bin/node "$HOME"/.volta/bin/node "$HOME"/.local/share/mise/shims/node /usr/local/bin/node /usr/bin/node; do
    if [ -x "$c" ]; then n="$c"; break; fi
  done
fi
if [ -z "$n" ]; then
  echo __PI_DOCTOR_MISSING__
else
  echo "path:$n"
  echo "version:$("$n" -p 'process.versions.node' 2>/dev/null)"
fi"#;

    let npm = r#"m="$(command -v npm 2>/dev/null)"
if [ -z "$m" ]; then
  echo __PI_DOCTOR_MISSING__
else
  echo "path:$m"
  echo "version:$("$m" -v 2>/dev/null)"
  echo "prefix:$("$m" prefix -g 2>/dev/null)"
fi"#;

    let pi = r#"p="$(command -v pi 2>/dev/null)"
if [ -z "$p" ] && [ -n "$PI_DOCTOR_AGENT_DIR" ] && [ -x "$PI_DOCTOR_AGENT_DIR/bin/pi" ]; then
  p="$PI_DOCTOR_AGENT_DIR/bin/pi"
fi
if [ -z "$p" ]; then
  np="$(npm prefix -g 2>/dev/null)"
  if [ -n "$np" ] && [ -x "$np/bin/pi" ]; then p="$np/bin/pi"; fi
fi
if [ -z "$p" ]; then
  echo __PI_DOCTOR_MISSING__
else
  echo "path:$p"
  echo "version:$("$p" --version 2>/dev/null | head -n 1)"
  echo "list:"
  "$p" list 2>/dev/null
fi"#;

    [
        section("node", node),
        section("npm", npm),
        section("pi", pi),
    ]
    .join("\n")
}

fn auth_script(pi_path: &str, provider: &str) -> String {
    let body = format!(
        r#"p={}; [ -n "$p" ] || p="$(command -v pi 2>/dev/null)"
if [ -z "$p" ]; then
  echo __PI_DOCTOR_MISSING__
else
  "$p" auth check --provider {} --json 2>&1
fi"#,
        sh_quote(pi_path),
        sh_quote(provider)
    );
    section("auth", &body)
}

/// 单引号包裹并转义，避免 provider id 里的字符逃出引号。
pub(crate) fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn platform() -> Platform {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    Platform {
        os: std::env::consts::OS.to_string(),
        arch: arch.to_string(),
    }
}

fn platform_check() -> Check {
    let platform = platform();
    pass(
        "os",
        Some(format!("{} · {}", platform.os, platform.arch)),
        None,
    )
}

fn agent_dir_check(agent_dir: Option<&Path>) -> Check {
    let Some(dir) = agent_dir else {
        return fail(
            "agent_dir",
            "无法确定宿主目录，请在设置里填写或设置环境变量 PI_CODING_AGENT_DIR",
            Some("agent_dir.edit"),
        );
    };
    let value = Some(dir.display().to_string());

    match fs::metadata(dir) {
        Ok(metadata) if !metadata.is_dir() => record(
            "agent_dir",
            CheckStatus::Fail,
            value,
            Some("路径存在但不是文件夹".to_string()),
            Some("agent_dir.edit"),
        ),
        Ok(metadata) if metadata.permissions().readonly() => record(
            "agent_dir",
            CheckStatus::Warn,
            value,
            Some("目录存在但不可写".to_string()),
            Some("agent_dir.edit"),
        ),
        Ok(_) => pass("agent_dir", value, None),
        Err(error) => record(
            "agent_dir",
            CheckStatus::Fail,
            value,
            Some(format!("目录不存在或不可读（{error}）")),
            Some("agent_dir.edit"),
        ),
    }
}

fn node_checks(outcome: &CommandOutcome) -> Vec<Check> {
    match section_data(outcome, "node") {
        SectionData::Timeout => vec![
            unknown("node.path", "探测超时（probe_timeout）"),
            unknown("node.version", "探测超时（probe_timeout）"),
        ],
        SectionData::Unavailable(detail) => vec![
            unknown("node.path", &detail),
            unknown("node.version", &detail),
        ],
        SectionData::Missing => vec![
            fail(
                "node.path",
                "登录 shell 里找不到 node（tool_not_found）",
                Some("node.install"),
            ),
            unknown("node.version", "node 未找到（tool_not_found）"),
        ],
        SectionData::Absent => vec![
            fail(
                "node.path",
                "node 探测没有输出（tool_broken）",
                Some("node.install"),
            ),
            unknown("node.version", "node 探测没有输出（tool_broken）"),
        ],
        SectionData::Body { fields, .. } => {
            let path = fields.get("path").cloned().unwrap_or_default();
            let path_check = if path.is_empty() {
                fail(
                    "node.path",
                    "node 探测没有给出路径（tool_broken）",
                    Some("node.install"),
                )
            } else {
                pass("node.path", Some(path), None)
            };

            let raw = fields
                .get("version")
                .map(String::as_str)
                .unwrap_or_default()
                .to_string();
            let version_check = if raw.is_empty() {
                fail(
                    "node.version",
                    "找到 node 但读不到版本（tool_broken）",
                    Some("node.install"),
                )
            } else {
                match parse_version(&raw) {
                    None => unknown("node.version", "版本号无法解析（version_unparsable）"),
                    Some(version) if version >= MIN_NODE_VERSION => {
                        pass("node.version", Some(raw), None)
                    }
                    Some(version) => record(
                        "node.version",
                        CheckStatus::Fail,
                        Some(raw),
                        Some(format!(
                            "版本 {} 低于要求的 {}.{}.{}",
                            format_version(version),
                            MIN_NODE_VERSION.0,
                            MIN_NODE_VERSION.1,
                            MIN_NODE_VERSION.2
                        )),
                        Some("node.install"),
                    ),
                }
            };

            vec![path_check, version_check]
        }
    }
}

fn npm_checks(outcome: &CommandOutcome) -> Vec<Check> {
    match section_data(outcome, "npm") {
        SectionData::Timeout => vec![unknown("npm", "探测超时（probe_timeout）")],
        SectionData::Unavailable(detail) => vec![unknown("npm", &detail)],
        SectionData::Missing => vec![fail(
            "npm",
            "登录 shell 里找不到 npm（tool_not_found）",
            Some("node.install"),
        )],
        SectionData::Absent => vec![fail(
            "npm",
            "npm 探测没有输出（tool_broken）",
            Some("node.install"),
        )],
        SectionData::Body { fields, .. } => {
            let path = fields.get("path").cloned().unwrap_or_default();
            let version = fields.get("version").cloned().unwrap_or_default();
            let prefix = fields.get("prefix").cloned().unwrap_or_default();

            if path.is_empty() {
                return vec![fail(
                    "npm",
                    "npm 探测没有给出路径（tool_broken）",
                    Some("node.install"),
                )];
            }
            if version.is_empty() {
                return vec![fail(
                    "npm",
                    "找到 npm 但读不到版本（tool_broken）",
                    Some("node.install"),
                )];
            }

            let prefix_text = if prefix.is_empty() {
                "全局前缀未知".to_string()
            } else {
                format!("全局前缀 {prefix}")
            };
            vec![pass(
                "npm",
                Some(version),
                Some(format!("路径 {path}；{prefix_text}")),
            )]
        }
    }
}

fn pi_checks(outcome: &CommandOutcome) -> (Vec<Check>, Option<String>, Option<String>) {
    match section_data(outcome, "pi") {
        SectionData::Timeout => (
            vec![
                unknown("pi.path", "探测超时（probe_timeout）"),
                unknown("pi.version", "探测超时（probe_timeout）"),
            ],
            None,
            None,
        ),
        SectionData::Unavailable(detail) => (
            vec![unknown("pi.path", &detail), unknown("pi.version", &detail)],
            None,
            None,
        ),
        SectionData::Missing => (
            vec![
                fail(
                    "pi.path",
                    "登录 shell 里找不到 pi（tool_not_found）",
                    Some("host.install"),
                ),
                unknown("pi.version", "pi 未找到（tool_not_found）"),
            ],
            None,
            None,
        ),
        SectionData::Absent => (
            vec![
                fail(
                    "pi.path",
                    "pi 探测没有输出（tool_broken）",
                    Some("host.install"),
                ),
                unknown("pi.version", "pi 探测没有输出（tool_broken）"),
            ],
            None,
            None,
        ),
        SectionData::Body { fields, .. } => {
            let path = fields.get("path").cloned().unwrap_or_default();
            let raw = fields.get("version").cloned().unwrap_or_default();
            let list = fields.get("list").cloned();

            let path_check = if path.is_empty() {
                fail(
                    "pi.path",
                    "pi 探测没有给出路径（tool_broken）",
                    Some("host.install"),
                )
            } else {
                pass("pi.path", Some(path.clone()), None)
            };

            let version_check = if raw.is_empty() {
                fail(
                    "pi.version",
                    "找到 pi 但读不到版本（tool_broken）",
                    Some("host.upgrade"),
                )
            } else {
                match parse_version(&raw) {
                    None => unknown("pi.version", "版本号无法解析（version_unparsable）"),
                    Some(_) => pass("pi.version", Some(raw), None),
                }
            };

            (
                vec![path_check, version_check],
                if path.is_empty() { None } else { Some(path) },
                list,
            )
        }
    }
}

fn install_kind_check(agent_dir: Option<&Path>, pi_path: Option<&str>) -> Check {
    let managed_marker = agent_dir.map(|dir| dir.join("install").join("managed-install.json"));

    if let Some(marker) = managed_marker.as_ref().filter(|path| path.exists()) {
        return pass(
            "pi.install_kind",
            Some("托管安装".to_string()),
            Some(format!("存在 {}", marker.display())),
        );
    }

    match pi_path {
        Some(path) if path.contains("node_modules") => pass(
            "pi.install_kind",
            Some("npm 全局安装".to_string()),
            Some(path.to_string()),
        ),
        Some(path) => record(
            "pi.install_kind",
            CheckStatus::Warn,
            Some("未知".to_string()),
            Some(format!(
                "{path} 既不是托管安装也不是 npm 全局安装，升级方式需要手动确认"
            )),
            Some("host.upgrade"),
        ),
        None => unknown("pi.install_kind", "找不到 pi，无法判断安装方式"),
    }
}

fn auth_checks(
    agent_dir: Option<&Path>,
    pi_path: Option<&str>,
    runner: &dyn CommandRunner,
) -> Vec<Check> {
    let providers = match provider_ids(agent_dir) {
        Ok(providers) => providers,
        Err(detail) => return vec![unknown("auth", &detail)],
    };
    if providers.is_empty() {
        return vec![pass("auth", Some("没有配置 provider".to_string()), None)];
    }

    let env = [("PI_DOCTOR_PI", pi_path.unwrap_or_default().to_string())];
    providers
        .into_iter()
        .map(|provider| {
            let outcome = runner.run(
                &auth_script(pi_path.unwrap_or_default(), &provider),
                AUTH_TIMEOUT,
                &env,
            );
            auth_check(&provider, &outcome)
        })
        .collect()
}

fn auth_check(provider: &str, outcome: &CommandOutcome) -> Check {
    let id = format!("auth:{provider}");
    match section_data(outcome, "auth") {
        SectionData::Timeout => unknown(&id, "凭据检查超时（probe_timeout）"),
        SectionData::Unavailable(detail) => unknown(&id, &detail),
        SectionData::Missing => fail(
            &id,
            "pi 未找到，无法检查凭据（tool_not_found）",
            Some("host.install"),
        ),
        SectionData::Absent => fail(
            &id,
            "凭据检查没有输出（tool_broken）",
            Some("auth.configure"),
        ),
        SectionData::Body { raw, .. } => match parse_auth_status(&raw).as_deref() {
            Some("ready") => pass(&id, Some("ready".to_string()), None),
            Some("not_ready") => fail(
                &id,
                "凭据未就绪（not_ready），需要补齐 key 或重新登录",
                Some("auth.configure"),
            ),
            Some("invalid") => fail(
                &id,
                "凭据无效（invalid），需要重新填写",
                Some("auth.configure"),
            ),
            Some(other) => unknown(&id, &format!("未知的凭据状态 {other}")),
            None => unknown(&id, "凭据检查输出无法解析（version_unparsable）"),
        },
    }
}

/// `pi auth check --json` 只看 `status` 字段，其余内容一律不展示。
fn parse_auth_status(raw: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    value
        .get("status")
        .and_then(|status| status.as_str())
        .map(str::to_string)
}

fn provider_ids(agent_dir: Option<&Path>) -> Result<Vec<String>, String> {
    let Some(dir) = agent_dir else {
        return Err("无法确定宿主目录，跳过凭据检查".to_string());
    };
    let text = fs::read_to_string(dir.join("models.json"))
        .map_err(|error| format!("无法读取 models.json（{error}），跳过凭据检查"))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("models.json 无法解析（{error}），跳过凭据检查"))?;
    let providers = value
        .get("providers")
        .and_then(|providers| providers.as_object())
        .ok_or_else(|| "models.json 里没有 providers，跳过凭据检查".to_string())?;

    let mut ids: Vec<String> = providers.keys().cloned().collect();
    ids.sort();
    Ok(ids)
}

fn packages_check(agent_dir: Option<&Path>, list: Option<&str>) -> Check {
    let Some(dir) = agent_dir else {
        return unknown("packages", "无法确定宿主目录，跳过包检查");
    };
    let declared = match declared_packages(dir) {
        Ok(declared) => declared,
        Err(detail) => return unknown("packages", &detail),
    };
    let Some(list) = list else {
        return unknown("packages", "拿不到 pi list 输出，无法比对实装");
    };
    if list.trim().is_empty() {
        return unknown("packages", "pi list 没有输出，无法比对实装");
    }

    let installed = parse_installed_packages(list);
    let missing: Vec<&String> = declared.difference(&installed).collect();
    let extra: Vec<&String> = installed.difference(&declared).collect();

    if !missing.is_empty() {
        return record(
            "packages",
            CheckStatus::Fail,
            Some(format!(
                "声明 {} 个，实装 {} 个",
                declared.len(),
                installed.len()
            )),
            Some(format!(
                "缺包：{}",
                missing
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join("、")
            )),
            Some("packages.install"),
        );
    }
    if !extra.is_empty() {
        return record(
            "packages",
            CheckStatus::Warn,
            Some(format!(
                "声明 {} 个，实装 {} 个",
                declared.len(),
                installed.len()
            )),
            Some(format!(
                "多装：{}",
                extra
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join("、")
            )),
            None,
        );
    }

    pass(
        "packages",
        Some(format!("{} 个包声明与实装一致", declared.len())),
        None,
    )
}

fn declared_packages(agent_dir: &Path) -> Result<BTreeSet<String>, String> {
    let path = agent_dir.join("settings.json");
    let text =
        fs::read_to_string(&path).map_err(|error| format!("无法读取 settings.json（{error}）"))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("settings.json 无法解析（{error}）"))?;

    let packages = value
        .get("packages")
        .and_then(|packages| packages.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    Ok(packages)
}

/// `pi list` 的条目行没有空格、含协议前缀、且不是绝对路径。
pub fn parse_installed_packages(list: &str) -> BTreeSet<String> {
    list.lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && !line.contains(char::is_whitespace)
                && line.contains(':')
                && !line.starts_with('/')
                && !line.starts_with('\\')
        })
        .map(str::to_string)
        .collect()
}

fn skills_check(agent_dir: Option<&Path>) -> Check {
    let Some(dir) = agent_dir else {
        return unknown("skills.links", "无法确定宿主目录，跳过技能链接检查");
    };
    let skills = dir.join("skills");
    if !skills.exists() {
        return pass("skills.links", Some("没有 skills 目录".to_string()), None);
    }

    let (links, broken) = match skills_links(&skills) {
        Ok(result) => result,
        Err(error) => return unknown("skills.links", &format!("无法读取 skills 目录（{error}）")),
    };

    if broken.is_empty() {
        return pass("skills.links", Some(format!("{links} 个链接")), None);
    }

    record(
        "skills.links",
        CheckStatus::Warn,
        Some(format!("{links} 个链接，{} 个断链", broken.len())),
        Some(format!("断链：{}", broken.join("、"))),
        Some("skills.relink"),
    )
}

/// 统计 `skills/` 下的符号链接与断链，返回 (链接数, 断链名)。
pub fn skills_links(skills: &Path) -> Result<(usize, Vec<String>), std::io::Error> {
    let mut links = 0;
    let mut broken = Vec::new();

    for entry in fs::read_dir(skills)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_symlink() {
            continue;
        }
        links += 1;

        let path = entry.path();
        let target = fs::read_link(&path)?;
        let resolved = if target.is_absolute() {
            target
        } else {
            skills.join(target)
        };
        if fs::metadata(&resolved).is_err() {
            broken.push(entry.file_name().to_string_lossy().to_string());
        }
    }

    Ok((links, broken))
}

/// 取第一个形如 `x.y.z` 的片段，忽略前后杂质。
pub fn parse_version(text: &str) -> Option<Version> {
    let bytes = text.as_bytes();

    for start in 0..bytes.len() {
        if !bytes[start].is_ascii_digit() {
            continue;
        }
        // 片段必须从数字串开头开始，避免从 "42.1.2" 中间截出 "2.1.2"。
        if start > 0 && (bytes[start - 1].is_ascii_digit() || bytes[start - 1] == b'.') {
            continue;
        }

        let mut parts = [0u32; 3];
        let mut cursor = start;
        let mut matched = true;

        for (index, part) in parts.iter_mut().enumerate() {
            let digits_start = cursor;
            while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                cursor += 1;
            }
            if cursor == digits_start || cursor - digits_start > 9 {
                matched = false;
                break;
            }
            *part = text[digits_start..cursor].parse().ok()?;
            if index < 2 {
                if cursor < bytes.len() && bytes[cursor] == b'.' {
                    cursor += 1;
                } else {
                    matched = false;
                    break;
                }
            }
        }

        if matched {
            return Some((parts[0], parts[1], parts[2]));
        }
    }

    None
}

fn format_version(version: Version) -> String {
    format!("{}.{}.{}", version.0, version.1, version.2)
}

const SECRET_MARKERS: &[&str] = &[
    "sk-",
    "Bearer ",
    "bearer ",
    "token=",
    "TOKEN=",
    "api_key=",
    "apiKey=",
    "apikey=",
    "password=",
    "secret=",
    "Authorization: ",
];

/// 抹掉日志与报告里的密钥：命中前缀后，把紧随其后的整段 token 换成 `<redacted>`。
pub fn redact(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;

    while let Some((index, marker)) = next_marker(rest) {
        output.push_str(&rest[..index]);
        output.push_str(marker);
        let after = &rest[index + marker.len()..];
        let token_len = if marker.ends_with(' ') || !after.starts_with(' ') {
            secret_token_len(after)
        } else {
            0
        };
        if token_len == 0 {
            rest = after;
        } else {
            output.push_str("<redacted>");
            rest = &after[token_len..];
        }
    }

    output.push_str(rest);
    output
}

fn next_marker(text: &str) -> Option<(usize, &'static str)> {
    SECRET_MARKERS
        .iter()
        .filter_map(|marker| text.find(marker).map(|index| (index, *marker)))
        .min_by_key(|(index, _)| *index)
}

/// token 结束于空白与常见分隔符；`Bearer ` 这类标记自带空格，其余标记后必须紧跟 token。
fn secret_token_len(text: &str) -> usize {
    let mut length = 0;
    for character in text.chars() {
        if character.is_whitespace()
            || matches!(character, '"' | '\'' | ',' | ';' | ')' | '}' | '&' | '?')
        {
            break;
        }
        length += character.len_utf8();
    }
    length
}

/// 报告里的时间戳：UTC ISO8601，秒级。
fn iso8601_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    iso8601_from_unix(seconds)
}

pub fn iso8601_from_unix(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let remainder = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        remainder / 3600,
        remainder % 3600 / 60,
        remainder % 60
    )
}

/// Howard Hinnant 的 days → 公历日期换算。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = (if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    }) / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = (if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    }) as u32;

    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// 执行一次完整检查。只读：只有读文件与读命令输出。
pub fn check(agent_dir: Option<&Path>, runner: &dyn CommandRunner) -> DoctorReport {
    let mut checks = vec![platform_check(), agent_dir_check(agent_dir)];

    // 子进程串行执行：同时最多 2 个的上限自然满足，也不会拖慢宿主。
    let fast_env = [(
        "PI_DOCTOR_AGENT_DIR",
        agent_dir
            .map(|dir| dir.display().to_string())
            .unwrap_or_default(),
    )];
    let fast_outcome = runner.run(&fast_script(), PROBE_TIMEOUT, &fast_env);

    checks.extend(node_checks(&fast_outcome));
    checks.extend(npm_checks(&fast_outcome));
    let (pi, pi_path, pi_list) = pi_checks(&fast_outcome);
    checks.extend(pi);
    checks.push(install_kind_check(agent_dir, pi_path.as_deref()));
    checks.extend(auth_checks(agent_dir, pi_path.as_deref(), runner));
    checks.push(packages_check(agent_dir, pi_list.as_deref()));
    checks.push(skills_check(agent_dir));

    DoctorReport {
        generated_at: iso8601_now(),
        platform: platform(),
        checks,
    }
}

/// command 层入口：解析出生效宿主目录后跑一次检查。
fn probe(app: &tauri::AppHandle) -> Result<DoctorReport, String> {
    use tauri::Manager;

    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("无法定位应用配置目录：{error}"))?;
    let config = config::load_config(&config_dir).map_err(|error| error.to_string())?;
    let env_agent_dir = std::env::var_os(AGENT_DIR_ENV_VAR).map(std::path::PathBuf::from);
    let home_dir = app.path().home_dir().ok();
    let resolved =
        config::resolve_agent_dir(&config, env_agent_dir.as_deref(), home_dir.as_deref());

    Ok(check(resolved.path.as_deref(), &ShellRunner::from_env()))
}

/// 子进程会阻塞 5 到 15 秒，放阻塞线程池，别占住 UI 线程。
#[tauri::command]
pub async fn run_check(app: tauri::AppHandle) -> Result<DoctorReport, String> {
    tauri::async_runtime::spawn_blocking(move || probe(&app))
        .await
        .map_err(|error| format!("医生检查失败：{error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT_DIR_ID: AtomicU32 = AtomicU32::new(0);

    /// 临时目录夹具：不引入 tempfile，退出时自行清理。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = NEXT_DIR_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-doctor-{}-{}-{}",
                label,
                std::process::id(),
                unique
            ));
            if path.exists() {
                fs::remove_dir_all(&path).expect("清理残留临时目录");
            }
            fs::create_dir_all(&path).expect("创建临时目录");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.path.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// 假命令执行器：按调用顺序返回预置结果，并记录收到的脚本与超时。
    struct FakeRunner {
        outcomes: RefCell<Vec<CommandOutcome>>,
        calls: RefCell<Vec<(String, Duration)>>,
    }

    impl FakeRunner {
        fn new(outcomes: Vec<CommandOutcome>) -> Self {
            Self {
                outcomes: RefCell::new(outcomes),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn single(outcome: CommandOutcome) -> Self {
            Self::new(vec![outcome.clone(), outcome])
        }

        fn calls(&self) -> Vec<(String, Duration)> {
            self.calls.borrow().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, script: &str, timeout: Duration, _env: &[(&str, String)]) -> CommandOutcome {
            self.calls.borrow_mut().push((script.to_string(), timeout));
            let mut outcomes = self.outcomes.borrow_mut();
            if outcomes.is_empty() {
                CommandOutcome::Timeout
            } else {
                outcomes.remove(0)
            }
        }
    }

    fn ok(stdout: &str) -> CommandOutcome {
        CommandOutcome::Ok {
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    /// 真实的探测输出形态：哨兵各占一行，前后是 shell 启动噪音。
    fn fake_section(id: &str, body: &str) -> String {
        format!("\n{SECTION_BEGIN}{id}__\n{body}\n{SECTION_END}{id}__\n")
    }

    fn fast_ok(node: &str, npm: &str, pi: &str) -> CommandOutcome {
        ok(&format!(
            "shell noise\n{}{}{}more noise\n",
            fake_section("node", node),
            fake_section("npm", npm),
            fake_section("pi", pi)
        ))
    }

    fn find<'a>(report: &'a DoctorReport, id: &str) -> &'a Check {
        report
            .checks
            .iter()
            .find(|check| check.id == id)
            .unwrap_or_else(|| panic!("缺少检查项 {id}"))
    }

    #[test]
    fn parse_version_takes_first_clean_fragment() {
        assert_eq!(parse_version("22.23.0"), Some((22, 23, 0)));
        assert_eq!(parse_version("v1.5.2"), Some((1, 5, 2)));
        assert_eq!(
            parse_version("pi 1.5.2 (build 2026-11-20)"),
            Some((1, 5, 2))
        );
        assert_eq!(parse_version("npm 10.9.2\n"), Some((10, 9, 2)));
        assert_eq!(parse_version("version 0.1.0-beta.2"), Some((0, 1, 0)));
        assert_eq!(parse_version("2026-11-20T05:30:00Z"), None);
        assert_eq!(parse_version("no digits here"), None);
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("v1.x.2"), None);
    }

    #[test]
    fn parse_sections_ignores_shell_noise() {
        let stdout = "welcome to bash\n\x1b[1mnvm loaded\x1b[0m\n__PI_DOCTOR__BEGIN__node__\npath:/usr/bin/node\nversion:22.23.0\n__PI_DOCTOR__END__node__\ntrailing noise\n";

        let sections = parse_sections(stdout);

        assert_eq!(sections.len(), 1);
        assert_eq!(
            sections.get("node").map(String::as_str),
            Some("path:/usr/bin/node\nversion:22.23.0")
        );
    }

    #[test]
    fn parse_sections_keeps_multiline_list() {
        let stdout = format!(
            "noise\n{}",
            fake_section(
                "pi",
                "path:/usr/bin/pi\nlist:\nUser packages:\n  npm:pi-lens"
            )
        );

        let sections = parse_sections(&stdout);
        let fields = parse_fields(sections.get("pi").map(String::as_str));

        assert_eq!(fields.get("path").map(String::as_str), Some("/usr/bin/pi"));
        assert_eq!(
            fields.get("list").map(String::as_str),
            Some("User packages:\n  npm:pi-lens")
        );
    }

    #[test]
    fn timeout_marks_probes_unknown() {
        let runner = FakeRunner::single(CommandOutcome::Timeout);
        let report = check(None, &runner);

        for id in ["node.path", "node.version", "npm", "pi.path", "pi.version"] {
            let check = find(&report, id);
            assert_eq!(check.status, CheckStatus::Unknown, "{id}");
            assert!(check
                .detail
                .clone()
                .unwrap_or_default()
                .contains("probe_timeout"));
        }

        let calls = runner.calls();
        assert_eq!(
            calls.first().map(|(_, timeout)| *timeout),
            Some(PROBE_TIMEOUT)
        );
        assert!(calls
            .first()
            .unwrap()
            .0
            .contains("__PI_DOCTOR__BEGIN__node__"));
    }

    #[test]
    fn missing_command_differs_from_broken_command() {
        let missing = check(
            None,
            &FakeRunner::new(vec![fast_ok(
                MISSING_MARKER,
                MISSING_MARKER,
                MISSING_MARKER,
            )]),
        );
        assert_eq!(find(&missing, "node.path").status, CheckStatus::Fail);
        assert!(find(&missing, "node.path")
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("tool_not_found"));
        assert_eq!(find(&missing, "pi.path").status, CheckStatus::Fail);

        let broken = check(
            None,
            &FakeRunner::new(vec![fast_ok(
                "path:/usr/bin/node\nversion:",
                "path:/usr/bin/npm\nversion:\nprefix:/usr",
                "path:/usr/bin/pi\nversion:",
            )]),
        );
        assert_eq!(find(&broken, "node.path").status, CheckStatus::Pass);
        assert_eq!(find(&broken, "node.version").status, CheckStatus::Fail);
        assert!(find(&broken, "node.version")
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("tool_broken"));
        assert_eq!(find(&broken, "pi.version").status, CheckStatus::Fail);
        assert_eq!(find(&broken, "npm").status, CheckStatus::Fail);
    }

    #[test]
    fn node_below_minimum_fails_with_upgrade_hint() {
        let report = check(
            None,
            &FakeRunner::new(vec![fast_ok(
                "path:/usr/bin/node\nversion:20.11.1",
                "path:/usr/bin/npm\nversion:10.2.4\nprefix:/usr",
                "path:/usr/bin/pi\nversion:1.5.2\nlist:",
            )]),
        );

        let version = find(&report, "node.version");
        assert_eq!(version.status, CheckStatus::Fail);
        assert_eq!(version.value.as_deref(), Some("20.11.1"));
        assert_eq!(version.action.as_deref(), Some("node.install"));
        assert_eq!(find(&report, "node.path").status, CheckStatus::Pass);
    }

    #[test]
    fn unparsable_version_is_unknown() {
        let report = check(
            None,
            &FakeRunner::new(vec![fast_ok(
                "path:/usr/bin/node\nversion:twenty-two",
                "path:/usr/bin/npm\nversion:10.2.4\nprefix:/usr",
                "path:/usr/bin/pi\nversion:dev-build\nlist:",
            )]),
        );

        assert_eq!(find(&report, "node.version").status, CheckStatus::Unknown);
        assert_eq!(find(&report, "pi.version").status, CheckStatus::Unknown);
    }

    #[test]
    fn auth_checks_use_own_timeout_and_report_status() {
        let dir = TempDir::new("auth");
        dir.write(
            "models.json",
            r#"{"providers":{"openrouter":{},"local":{}}}"#,
        );
        let list = "User packages:\n  npm:pi-lens\n    /tmp/pi-lens";
        dir.write("settings.json", r#"{"packages":["npm:pi-lens"]}"#);

        let runner = FakeRunner::new(vec![
            fast_ok(
                "path:/usr/bin/node\nversion:22.23.0",
                "path:/usr/bin/npm\nversion:10.9.2\nprefix:/usr",
                &format!("path:/usr/bin/pi\nversion:1.5.2\nlist:\n{list}"),
            ),
            ok(&fake_section(
                "auth",
                r#"{"status":"not_ready","provider":"local"}"#,
            )),
            ok(&fake_section(
                "auth",
                r#"{"status":"ready","provider":"openrouter"}"#,
            )),
        ]);

        let report = check(Some(dir.path()), &runner);

        // provider id 按字典序检查：local 先于 openrouter。
        assert_eq!(find(&report, "auth:local").status, CheckStatus::Fail);
        assert_eq!(find(&report, "auth:openrouter").status, CheckStatus::Pass);
        assert_eq!(
            find(&report, "auth:local").action.as_deref(),
            Some("auth.configure")
        );

        let calls = runner.calls();
        assert_eq!(calls[0].1, PROBE_TIMEOUT);
        assert_eq!(calls[1].1, AUTH_TIMEOUT);
        assert_eq!(calls[2].1, AUTH_TIMEOUT);
        assert!(calls[1].0.contains("--provider 'local'"));
        assert!(calls[2].0.contains("--provider 'openrouter'"));
        assert_eq!(find(&report, "packages").status, CheckStatus::Pass);
    }

    #[test]
    fn auth_without_models_file_is_unknown() {
        let dir = TempDir::new("auth-missing");
        let report = check(
            Some(dir.path()),
            &FakeRunner::single(CommandOutcome::Timeout),
        );

        assert_eq!(find(&report, "auth").status, CheckStatus::Unknown);
        assert!(find(&report, "auth").detail.is_some());
    }

    #[test]
    fn packages_report_missing_and_extra() {
        let dir = TempDir::new("packages");
        dir.write(
            "settings.json",
            r#"{"packages":["npm:pi-lens","npm:missing-pkg"]}"#,
        );
        let list =
            "User packages:\n  npm:pi-lens\n    /tmp/pi-lens\n  npm:extra-pkg\n    /tmp/extra-pkg";

        let report = check(
            Some(dir.path()),
            &FakeRunner::new(vec![fast_ok(
                "path:/usr/bin/node\nversion:22.23.0",
                "path:/usr/bin/npm\nversion:10.9.2\nprefix:/usr",
                &format!("path:/usr/bin/pi\nversion:1.5.2\nlist:\n{list}"),
            )]),
        );

        let packages = find(&report, "packages");
        assert_eq!(packages.status, CheckStatus::Fail);
        assert_eq!(packages.action.as_deref(), Some("packages.install"));
        assert!(packages
            .detail
            .clone()
            .unwrap_or_default()
            .contains("npm:missing-pkg"));
    }

    #[test]
    fn package_list_parsing_ignores_paths_and_headers() {
        let list = "User packages:\n  npm:pi-lens\n    /home/u/.pi/agent/npm/node_modules/pi-lens\n  git:github.com/u/r\n    /home/u/r";

        let installed = parse_installed_packages(list);

        assert_eq!(
            installed.iter().cloned().collect::<Vec<_>>(),
            vec!["git:github.com/u/r".to_string(), "npm:pi-lens".to_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn skills_count_symlinks_and_broken_links() {
        let dir = TempDir::new("skills");
        let skills = dir.path().join("skills");
        let target = dir.path().join("real-skill");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&skills).unwrap();
        std::os::unix::fs::symlink(&target, skills.join("good")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("nowhere"), skills.join("bad")).unwrap();

        let (links, broken) = skills_links(&skills).unwrap();
        assert_eq!(links, 2);
        assert_eq!(broken, vec!["bad".to_string()]);

        let report = check(
            Some(dir.path()),
            &FakeRunner::single(CommandOutcome::Timeout),
        );
        let skills_check = find(&report, "skills.links");
        assert_eq!(skills_check.status, CheckStatus::Warn);
        assert_eq!(skills_check.action.as_deref(), Some("skills.relink"));
        assert!(skills_check
            .detail
            .clone()
            .unwrap_or_default()
            .contains("bad"));
    }

    #[test]
    fn install_kind_prefers_managed_marker() {
        let dir = TempDir::new("install-kind");
        dir.write("install/managed-install.json", "{}");

        let report = check(
            Some(dir.path()),
            &FakeRunner::new(vec![fast_ok(
                "path:/usr/bin/node\nversion:22.23.0",
                "path:/usr/bin/npm\nversion:10.9.2\nprefix:/usr",
                &format!(
                    "path:{}\nversion:1.5.2\nlist:",
                    dir.path().join("bin/pi").display()
                ),
            )]),
        );

        assert_eq!(find(&report, "pi.install_kind").status, CheckStatus::Pass);
        assert_eq!(
            find(&report, "pi.install_kind").value.as_deref(),
            Some("托管安装")
        );
    }

    #[test]
    fn install_kind_falls_back_to_npm_layout() {
        let report = check(
            None,
            &FakeRunner::new(vec![fast_ok(
                "path:/usr/bin/node\nversion:22.23.0",
                "path:/usr/bin/npm\nversion:10.9.2\nprefix:/usr",
                "path:/usr/lib/node_modules/@earendil-works/pi-coding-agent/bin/pi\nversion:1.5.2\nlist:",
            )]),
        );

        assert_eq!(
            find(&report, "pi.install_kind").value.as_deref(),
            Some("npm 全局安装")
        );
    }

    #[test]
    fn agent_dir_missing_reports_action() {
        let dir = TempDir::new("agent-dir");
        let missing = dir.path().join("not-there");

        let report = check(Some(&missing), &FakeRunner::single(CommandOutcome::Timeout));
        let check = find(&report, "agent_dir");

        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.action.as_deref(), Some("agent_dir.edit"));
    }

    #[test]
    fn redact_masks_known_markers() {
        let text = "key sk-abc123 invalid; header Bearer eyJhbGci; url ?token=deadbeef&x=1";

        let redacted = redact(text);

        assert!(!redacted.contains("sk-abc123"));
        assert!(!redacted.contains("eyJhbGci"));
        assert!(!redacted.contains("deadbeef"));
        assert_eq!(redacted.matches("<redacted>").count(), 3);
        assert!(redacted.contains("?token=<redacted>&x=1"));
    }

    #[test]
    fn redact_keeps_plain_text_and_bare_markers() {
        assert_eq!(redact("node 22.23.0 is ready"), "node 22.23.0 is ready");
        assert_eq!(redact("token= at end"), "token= at end");
    }

    #[test]
    fn redaction_is_applied_to_command_output() {
        let dir = TempDir::new("redact");
        dir.write(
            "settings.json",
            r#"{"packages":["npm:token=sk-live-9999"]}"#,
        );
        let list = "User packages:\n  npm:pi-lens\n    /tmp/pi-lens";

        let report = check(
            Some(dir.path()),
            &FakeRunner::new(vec![fast_ok(
                "path:/usr/bin/node\nversion:22.23.0",
                "path:/usr/bin/npm\nversion:10.9.2\nprefix:/usr",
                &format!("path:/usr/bin/pi\nversion:1.5.2\nlist:\n{list}"),
            )]),
        );

        let packages = find(&report, "packages");
        assert_eq!(packages.status, CheckStatus::Fail);
        assert!(!format!("{packages:?}").contains("sk-live-9999"));
        assert!(packages
            .detail
            .clone()
            .unwrap_or_default()
            .contains("<redacted>"));
    }

    #[test]
    fn report_serializes_documented_field_names() {
        let report = check(
            None,
            &FakeRunner::new(vec![fast_ok(
                "path:/usr/bin/node\nversion:22.23.0",
                "path:/usr/bin/npm\nversion:10.9.2\nprefix:/usr",
                "path:/usr/bin/pi\nversion:1.5.2\nlist:",
            )]),
        );
        let json = serde_json::to_value(&report).unwrap();

        assert!(json.get("generatedAt").is_some());
        assert!(json["generatedAt"].as_str().unwrap().ends_with('Z'));
        assert!(json["platform"]["os"].is_string());
        assert!(json["platform"]["arch"].is_string());

        let first = &json["checks"][0];
        for key in ["id", "status", "value", "detail", "action"] {
            assert!(first.get(key).is_some(), "缺字段 {key}");
        }
        assert_eq!(first["status"], serde_json::json!("pass"));
    }

    #[test]
    fn timestamp_formats_as_utc_iso8601() {
        assert_eq!(iso8601_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_from_unix(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}

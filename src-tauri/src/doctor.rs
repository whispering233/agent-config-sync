//! 环境检测：探测 node / npm / pi / 凭据 / 包 / 技能链接，输出分级报告。
//!
//! 规则来源：`docs/design/20-env-doctor.md`。核心逻辑不依赖 Tauri，
//! shell 命令通过 [`CommandRunner`] 注入，单测用假命令替代真实进程。
//! 本模块全程只读：只读文件与环境，不写文件、不改环境变量、不安装任何东西。

use crate::config::{self, AGENT_DIR_ENV_VAR};
use crate::installer::{self, InstallKind};
use crate::platform::{self, HostOs, ProgramPath};
use crate::scope::{self, RootId};
use crate::versions::{self, LatestVersions};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
    /// 最新版本只在用户点「检查更新」时刷新；其余时间来自缓存。
    pub latest: LatestVersions,
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
    /// POSIX 登录 shell 入口：一条脚本里跑多个探测，靠哨兵分段取回结果。
    fn run(&self, script: &str, timeout: Duration, env: &[(&str, String)]) -> CommandOutcome;
    /// 逐命令入口：直接起 `argv[0]`，不经脚本包装。Windows 上的探测与安装全走这条。
    ///
    /// 不跑进程的替身只需实现 [`CommandRunner::run`]；万一有人把逐命令探测接到
    /// 未实现它的替身上，这里报「无法启动」而不是默默返回空输出。
    fn run_program(
        &self,
        _argv: &[String],
        _timeout: Duration,
        _env: &[(&str, String)],
    ) -> CommandOutcome {
        CommandOutcome::Unavailable("当前执行器不支持逐命令调用".to_string())
    }
}

/// 通过登录 shell 执行命令：GUI 进程的 PATH 通常不含 nvm/fnm/mise 的路径。
/// Windows 上没有登录 shell，探测与安装统一走 [`CommandRunner::run_program`]。
pub struct ShellRunner {
    shell: OsString,
}

impl ShellRunner {
    /// `$SHELL -lc`；没有该变量时退回 `sh`。
    pub fn from_env() -> Self {
        Self {
            shell: std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("sh")),
        }
    }
}

impl CommandRunner for ShellRunner {
    fn run(&self, script: &str, timeout: Duration, env: &[(&str, String)]) -> CommandOutcome {
        let argv = vec![
            self.shell.to_string_lossy().into_owned(),
            "-lc".to_string(),
            script.to_string(),
        ];
        self.run_program(&argv, timeout, env)
    }

    fn run_program(
        &self,
        argv: &[String],
        timeout: Duration,
        env: &[(&str, String)],
    ) -> CommandOutcome {
        let mut command = match platform::build_command(argv) {
            Ok(command) => command,
            Err(detail) => return CommandOutcome::Unavailable(detail),
        };
        for (key, value) in env {
            command.env(key, value);
        }
        run_child(command, timeout, false)
    }
}

/// 起一个子进程并把两条管道都读回来。`group` 为真时 POSIX 下独立成进程组，
/// 超时整组终止（安装命令用它）。
pub(crate) fn run_child(command: Command, timeout: Duration, group: bool) -> CommandOutcome {
    run_child_with_input(command, timeout, group, None)
}

/// 与 [`run_child`] 相同，但可以先往 stdin 写一段输入再关闭：宿主 RPC 靠它送请求，
/// 收到 EOF 后自行退出，不需要额外的关闭命令。
pub(crate) fn run_child_with_input(
    mut command: Command,
    timeout: Duration,
    group: bool,
    input: Option<&str>,
) -> CommandOutcome {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    platform::isolate_group(&mut command, group);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return CommandOutcome::Unavailable(error.to_string()),
    };

    if let Some(input) = input {
        // 写入放在独立线程：宿主读得慢也不占住下面的退出轮询。
        if let Some(mut stdin) = child.stdin.take() {
            let input = input.to_string();
            std::thread::spawn(move || {
                let _ = stdin.write_all(input.as_bytes());
                let _ = stdin.flush();
            });
        }
    }

    // 并行读两个管道：边读边轮询，避免子进程写满管道后卡住把超时拖成死锁。
    let stdout_reader = child.stdout.take().map(spawn_reader);
    let stderr_reader = child.stderr.take().map(spawn_reader);
    let deadline = Instant::now() + timeout;

    let exit = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                kill_tree(&mut child, group);
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                kill_tree(&mut child, group);
                return CommandOutcome::Unavailable(error.to_string());
            }
        }
    };

    let stdout = join_reader(stdout_reader);
    let stderr = join_reader(stderr_reader);

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

/// 先按平台杀整棵树或整个进程组，再兜底杀直接子进程并回收。
/// Windows 的 `child.kill()` 只杀 `cmd.exe`，npm 派生的子进程会活下来。
fn kill_tree(child: &mut Child, group: bool) {
    platform::kill_tree(child.id());
    if group {
        platform::kill_group(child.id());
    }
    let _ = child.kill();
    let _ = child.wait();
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

/// 取第一行并去掉首尾空白：宿主 CLI 的版本号可能带前缀或换行。
pub(crate) fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

/// 按 Windows 的形态跑一次逐命令探测：`.cmd` 中间层经 `cmd /C`。
/// 参数不是合法命令行（含 cmd 会展开的字符）时直接判为无法启动。
pub(crate) fn probe_program(
    runner: &dyn CommandRunner,
    program: &Path,
    args: &[&str],
) -> CommandOutcome {
    match platform::program_argv(HostOs::Windows, &program.display().to_string(), args) {
        Ok(argv) => runner.run_program(&argv, PROBE_TIMEOUT, &[]),
        Err(detail) => CommandOutcome::Unavailable(detail),
    }
}

/// 一次逐命令探测：解析到的程序路径与执行结果。
struct ProgramProbe {
    program: String,
    outcome: CommandOutcome,
}

impl ProgramProbe {
    fn run(runner: &dyn CommandRunner, program: &Path, args: &[&str]) -> Self {
        Self {
            program: program.display().to_string(),
            outcome: probe_program(runner, program, args),
        }
    }

    fn stdout(&self) -> &str {
        match &self.outcome {
            CommandOutcome::Ok { stdout, .. } | CommandOutcome::Failed { stdout, .. } => stdout,
            CommandOutcome::Timeout | CommandOutcome::Unavailable(_) => "",
        }
    }

    /// 该探测对应的段视图：命令跑起来了就带上 `path`，其余字段由各探测补。
    /// 非零退出或没有输出只让字段为空，「找到但执行失败」因此仍能报出入口位置。
    fn section(&self, fields: &[(&str, &str)]) -> SectionData {
        match &self.outcome {
            CommandOutcome::Timeout => SectionData::Timeout,
            CommandOutcome::Unavailable(detail) => SectionData::Unavailable(detail.clone()),
            CommandOutcome::Ok { stdout, .. } | CommandOutcome::Failed { stdout, .. } => {
                let mut map = BTreeMap::new();
                map.insert("path".to_string(), self.program.clone());
                for (key, value) in fields {
                    map.insert((*key).to_string(), (*value).to_string());
                }
                SectionData::Body {
                    raw: stdout.clone(),
                    fields: map,
                }
            }
        }
    }
}

/// 导出 manifest 用：本机 pi 与 node 的版本（顺序即 `(pi, node)`），探测失败留空，
/// 导出不因此中断。与 [`check`] 共用同一组探测，避免「环境检测说有版本、manifest 里是 null」。
pub fn probe_versions(
    agent_dir: Option<&Path>,
    runner: &dyn CommandRunner,
    host: HostOs,
) -> (Option<String>, Option<String>) {
    let probes = fast_probes(runner, agent_dir, &ProgramPath::from_env(), host);
    (section_version(&probes.pi), section_version(&probes.node))
}

/// 从探测段里取 `version` 字段：能解析就规范成 `x.y.z`，解析不了就留原文。
fn section_version(section: &SectionData) -> Option<String> {
    let SectionData::Body { fields, .. } = section else {
        return None;
    };
    let raw = fields.get("version")?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(
        parse_version(raw)
            .map(format_version)
            .unwrap_or_else(|| raw.to_string()),
    )
}

/// 快速组的三个探测结果。POSIX 从一条登录 shell 脚本的哨兵段里取；Windows 每个探测点
/// 单独起进程（`.cmd` 中间层经 `cmd /d /s /c`），不再拼批处理。
struct FastProbes {
    node: SectionData,
    npm: SectionData,
    pi: SectionData,
}

fn fast_probes(
    runner: &dyn CommandRunner,
    agent_dir: Option<&Path>,
    search: &ProgramPath,
    host: HostOs,
) -> FastProbes {
    if host == HostOs::Windows {
        return windows_probes(runner, search, agent_dir);
    }

    let env = [(
        "PI_DOCTOR_AGENT_DIR",
        agent_dir
            .map(|dir| dir.display().to_string())
            .unwrap_or_default(),
    )];
    let outcome = runner.run(&fast_script(), PROBE_TIMEOUT, &env);
    FastProbes {
        node: section_data(&outcome, "node"),
        npm: section_data(&outcome, "npm"),
        pi: section_data(&outcome, "pi"),
    }
}

fn windows_probes(
    runner: &dyn CommandRunner,
    search: &ProgramPath,
    agent_dir: Option<&Path>,
) -> FastProbes {
    let node = windows_node(runner, search);
    let (npm, prefix) = windows_npm(runner, search);
    let pi = windows_pi(runner, search, agent_dir, prefix.as_deref());
    FastProbes { node, npm, pi }
}

fn windows_node(runner: &dyn CommandRunner, search: &ProgramPath) -> SectionData {
    let Some(program) = search.find("node") else {
        return SectionData::Missing;
    };
    let probe = ProgramProbe::run(runner, &program, &["-p", "process.versions.node"]);
    let version = first_line(probe.stdout());
    probe.section(&[("version", &version)])
}

/// `npm -v` 与 `npm prefix -g`：全局前缀稍后用来兜底找 `pi.cmd`。
fn windows_npm(runner: &dyn CommandRunner, search: &ProgramPath) -> (SectionData, Option<String>) {
    let Some(program) = search.find("npm") else {
        return (SectionData::Missing, None);
    };

    let version = ProgramProbe::run(runner, &program, &["-v"]);
    if matches!(
        version.outcome,
        CommandOutcome::Timeout | CommandOutcome::Unavailable(_)
    ) {
        return (version.section(&[]), None);
    }

    let prefix = npm_global_prefix(runner, &program).unwrap_or_default();
    let value = first_line(version.stdout());
    let section = version.section(&[("version", &value), ("prefix", &prefix)]);
    let fallback = (!prefix.is_empty()).then_some(prefix);
    (section, fallback)
}

/// npm 全局前缀：`npm prefix -g` 的第一行。找不到 `pi` 时靠它兜底，
/// 安装页与包对齐页也走这里，避免“检测说装了、安装页说没装”。
pub(crate) fn npm_global_prefix(runner: &dyn CommandRunner, npm: &Path) -> Option<String> {
    let prefix = first_line(ProgramProbe::run(runner, npm, &["prefix", "-g"]).stdout());
    (!prefix.is_empty()).then_some(prefix)
}

fn windows_pi(
    runner: &dyn CommandRunner,
    search: &ProgramPath,
    agent_dir: Option<&Path>,
    npm_prefix: Option<&str>,
) -> SectionData {
    let Some(program) = find_pi(search, agent_dir, npm_prefix) else {
        return SectionData::Missing;
    };

    let version = ProgramProbe::run(runner, &program, &["--version"]);
    if matches!(
        version.outcome,
        CommandOutcome::Timeout | CommandOutcome::Unavailable(_)
    ) {
        return version.section(&[]);
    }

    let list = ProgramProbe::run(runner, &program, &["list"]);
    let value = first_line(version.stdout());
    version.section(&[("version", &value), ("list", list.stdout().trim())])
}

/// `pi` 入口：先看 PATH，再兜底 `<agent_dir>/bin` 与 npm 全局前缀；都没有就算未找到。
/// 检测、安装与包对齐三处都走这里，入口判断不会不一致。
pub(crate) fn find_pi(
    search: &ProgramPath,
    agent_dir: Option<&Path>,
    npm_prefix: Option<&str>,
) -> Option<PathBuf> {
    search
        .find("pi")
        .or_else(|| agent_dir.and_then(|dir| search.find_in(&dir.join("bin"), "pi")))
        .or_else(|| npm_prefix.and_then(|prefix| search.find_in(Path::new(prefix), "pi")))
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
        platform::sh_quote(pi_path),
        platform::sh_quote(provider)
    );
    section("auth", &body)
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

fn node_checks(section: &SectionData) -> Vec<Check> {
    match section {
        SectionData::Timeout => vec![
            unknown("node.path", "探测超时（probe_timeout）"),
            unknown("node.version", "探测超时（probe_timeout）"),
        ],
        SectionData::Unavailable(detail) => vec![
            unknown("node.path", detail),
            unknown("node.version", detail),
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

fn npm_checks(section: &SectionData) -> Vec<Check> {
    match section {
        SectionData::Timeout => vec![unknown("npm", "探测超时（probe_timeout）")],
        SectionData::Unavailable(detail) => vec![unknown("npm", detail)],
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

fn pi_checks(section: &SectionData) -> (Vec<Check>, Option<String>, Option<String>) {
    match section {
        SectionData::Timeout => (
            vec![
                unknown("pi.path", "探测超时（probe_timeout）"),
                unknown("pi.version", "探测超时（probe_timeout）"),
            ],
            None,
            None,
        ),
        SectionData::Unavailable(detail) => (
            vec![unknown("pi.path", detail), unknown("pi.version", detail)],
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
    let Some(path) = pi_path else {
        return unknown("pi.install_kind", "找不到 pi，无法判断安装方式");
    };

    match installer::install_kind(agent_dir, path) {
        InstallKind::Managed => pass(
            "pi.install_kind",
            Some("托管安装".to_string()),
            Some(path.to_string()),
        ),
        InstallKind::Npm => pass(
            "pi.install_kind",
            Some("npm 全局安装".to_string()),
            Some(path.to_string()),
        ),
        InstallKind::Unknown => record(
            "pi.install_kind",
            CheckStatus::Warn,
            Some("未知".to_string()),
            Some(format!(
                "{path} 既不是托管安装也不是 npm 全局安装，升级方式需要手动确认"
            )),
            Some("host.upgrade"),
        ),
    }
}

fn auth_checks(
    agent_dir: Option<&Path>,
    pi_path: Option<&str>,
    runner: &dyn CommandRunner,
    host: HostOs,
) -> Vec<Check> {
    let providers = match provider_ids(agent_dir) {
        Ok(providers) => providers,
        Err(detail) => return vec![unknown("auth", &detail)],
    };
    if providers.is_empty() {
        return vec![pass("auth", Some("没有配置 provider".to_string()), None)];
    }

    providers
        .into_iter()
        .map(|provider| auth_check(&provider, auth_probe(pi_path, &provider, runner, host)))
        .collect()
}

/// 一次凭据探测。POSIX 走登录 shell 脚本；Windows 直接跑 `pi auth check`，provider id
/// 来自用户配置，必须经 [`platform::program_argv`] 按平台转义后再拼。
fn auth_probe(
    pi_path: Option<&str>,
    provider: &str,
    runner: &dyn CommandRunner,
    host: HostOs,
) -> SectionData {
    if host == HostOs::Windows {
        return auth_probe_windows(pi_path, provider, runner);
    }

    let outcome = runner.run(
        &auth_script(pi_path.unwrap_or_default(), provider),
        AUTH_TIMEOUT,
        &[],
    );
    section_data(&outcome, "auth")
}

fn auth_probe_windows(
    pi_path: Option<&str>,
    provider: &str,
    runner: &dyn CommandRunner,
) -> SectionData {
    let program = pi_path.map(str::to_string).or_else(|| {
        ProgramPath::from_env()
            .find("pi")
            .map(|path| path.display().to_string())
    });
    let Some(program) = program else {
        return SectionData::Missing;
    };
    let argv = match platform::program_argv(
        HostOs::Windows,
        &program,
        &["auth", "check", "--provider", provider, "--json"],
    ) {
        Ok(argv) => argv,
        Err(detail) => return SectionData::Unavailable(detail),
    };

    match runner.run_program(&argv, AUTH_TIMEOUT, &[]) {
        // 段内容就是 JSON 原文，只看 stdout：stderr 是给人看的错误信息。
        CommandOutcome::Ok { stdout, .. } | CommandOutcome::Failed { stdout, .. } => {
            SectionData::Body {
                raw: stdout,
                fields: BTreeMap::new(),
            }
        }
        CommandOutcome::Timeout => SectionData::Timeout,
        CommandOutcome::Unavailable(detail) => SectionData::Unavailable(detail),
    }
}

fn auth_check(provider: &str, section: SectionData) -> Check {
    let id = format!("auth:{provider}");
    match section {
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

/// 技能链接检查的两个根：`<agent_dir>/skills` 与 `<home>/.agents/skills`。
/// 解析不到的根直接不在列表里，检查因而不会把「不知道路径」当成「没有断链」。
fn skills_dirs(agent_dir: Option<&Path>, home_dir: Option<&Path>) -> Vec<(RootId, PathBuf)> {
    let mut dirs = Vec::with_capacity(2);
    if let Some(dir) = agent_dir {
        dirs.push((RootId::Agent, dir.join("skills")));
    }
    if let Some(home) = home_dir {
        dirs.push((RootId::AgentsSkills, scope::agents_skills_path(home)));
    }
    dirs
}

fn skills_check(agent_dir: Option<&Path>, home_dir: Option<&Path>) -> Check {
    let dirs = skills_dirs(agent_dir, home_dir);
    if dirs.is_empty() {
        return unknown(
            "skills.links",
            "无法确定宿主目录与 home 目录，跳过技能链接检查",
        );
    }

    let mut links = 0;
    let mut broken = Vec::new();
    let mut broken_roots = Vec::new();
    let mut unreadable = Vec::new();
    let mut notes = Vec::new();
    let mut scanned = 0;

    for (root, dir) in dirs {
        // 根不存在是正常情况（没装这个目录就没有技能），不是错误。
        // 但要看 `symlink_metadata`：`exists()` 跟着链接走，根本身是断链时它会说“不存在”，
        // 静静跳过就等于把断链说成没问题。
        if fs::symlink_metadata(&dir).is_err() {
            continue;
        }
        scanned += 1;
        if platform::is_link_like(&dir) {
            // 根本身是链接：目标是断的就没有内容可扫，但必须告警。
            if fs::metadata(&dir).is_err() {
                broken_roots.push(format!(
                    "{} 是断链，按没有该目录处理",
                    checked_dir_label(root)
                ));
                continue;
            }
            notes.push(format!(
                "{} 本身是链接，按链接目标的实际内容检查",
                checked_dir_label(root)
            ));
        }
        match skills_links(&dir) {
            Ok((count, names)) => {
                links += count;
                broken.extend(names.iter().map(|name| broken_label(root, name)));
            }
            Err(error) => {
                unreadable.push(format!("无法读取 {}（{error}）", checked_dir_label(root)))
            }
        }
    }

    let broken_count = broken.len() + broken_roots.len();
    let mut findings = Vec::new();
    if !broken.is_empty() {
        findings.push(format!("断链：{}", broken.join("、")));
    }
    findings.extend(broken_roots);
    findings.extend(notes);

    if !unreadable.is_empty() {
        let mut detail = unreadable;
        detail.extend(findings);
        return unknown("skills.links", &detail.join("；"));
    }
    if scanned == 0 {
        return pass("skills.links", Some("没有 skills 目录".to_string()), None);
    }

    if broken_count == 0 {
        return record(
            "skills.links",
            CheckStatus::Pass,
            Some(format!("{links} 个链接")),
            (!findings.is_empty()).then(|| findings.join("；")),
            None,
        );
    }

    record(
        "skills.links",
        CheckStatus::Warn,
        Some(format!("{links} 个链接，{broken_count} 个断链")),
        Some(findings.join("；")),
        Some("skills.relink"),
    )
}

/// 被检查的目录本身怎么称呼：agent 根下的目录在 `skills/` 里，外部根本身就是技能目录。
/// 只说「宿主目录」会让人以为检查的是宿主目录根，指错要找的位置。
fn checked_dir_label(root: RootId) -> String {
    match root {
        RootId::Agent => root.describe("skills"),
        RootId::AgentsSkills => root.label().to_string(),
    }
}

/// 断链在告警里的位置：agent 根下的链接在 `skills/` 里，外部根本身就是技能目录。
fn broken_label(root: RootId, relative: &str) -> String {
    match root {
        RootId::Agent => root.describe(&format!("skills/{relative}")),
        RootId::AgentsSkills => root.describe(relative),
    }
}

/// 递归统计技能目录下的链接类条目与断链，返回 (链接数, 断链的相对路径)。
/// Windows 的 junction 也算链接，所以判定走 [`platform::link_like`]。
pub fn skills_links(skills: &Path) -> Result<(usize, Vec<String>), std::io::Error> {
    let mut scan = SkillsLinkScan::default();
    scan.walk(skills, Path::new(""), &mut HashSet::new())?;
    // 排序后告警文本与 readdir 顺序无关。
    scan.broken.sort();
    Ok((scan.links, scan.broken))
}

#[derive(Default)]
struct SkillsLinkScan {
    links: usize,
    broken: Vec<String>,
}

impl SkillsLinkScan {
    /// 目录读不出来就上报：把「没检查到」说成「没有断链」会误导用户。
    fn walk(
        &mut self,
        dir: &Path,
        relative: &Path,
        active: &mut HashSet<PathBuf>,
    ) -> std::io::Result<()> {
        let canonical = fs::canonicalize(dir)?;
        if !active.insert(canonical.clone()) {
            return Ok(());
        }
        let outcome = self.entries(dir, relative, active);
        active.remove(&canonical);
        outcome
    }

    fn entries(
        &mut self,
        dir: &Path,
        relative: &Path,
        active: &mut HashSet<PathBuf>,
    ) -> std::io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let path = entry.path();
            let child = relative.join(entry.file_name());
            let target = fs::read_link(&path);

            if platform::link_like(file_type.is_symlink(), &target) {
                self.links += 1;
                // 链接类却读不到目标：目标无从验证，按断链处理。
                let resolved = target.ok().map(|target| {
                    if target.is_absolute() {
                        target
                    } else {
                        dir.join(target)
                    }
                });
                if resolved.is_none_or(|target| fs::metadata(&target).is_err()) {
                    self.broken.push(relative_text(&child));
                }
                // 不进入链接：目标若在根内，它自己的位置会被扫到，避免重复计数与成环。
                continue;
            }
            if file_type.is_dir() {
                self.walk(&path, &child, active)?;
            }
        }
        Ok(())
    }
}

/// 断链明细用 `/` 分隔，告警文本在两条平台上一致。
fn relative_text(relative: &Path) -> String {
    relative.to_string_lossy().replace('\\', "/")
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

/// 执行一次完整检查。只读：只有读文件与读命令输出。`host` 由调用方给定：
/// 生产传 [`HostOs::current`]，单测显式指定，这样两条命令形态在任意平台上都能断言。
#[cfg(test)]
pub fn check(
    agent_dir: Option<&Path>,
    home_dir: Option<&Path>,
    runner: &dyn CommandRunner,
    host: HostOs,
) -> DoctorReport {
    check_with_latest(agent_dir, home_dir, runner, host, LatestVersions::default())
}

/// 与 [`check`] 相同，但把缓存/新查到的版本写进报告。
pub fn check_with_latest(
    agent_dir: Option<&Path>,
    home_dir: Option<&Path>,
    runner: &dyn CommandRunner,
    host: HostOs,
    latest: LatestVersions,
) -> DoctorReport {
    let mut checks = vec![platform_check(), agent_dir_check(agent_dir)];

    // 子进程串行执行：同时最多 2 个的上限自然满足，也不会拖慢宿主。
    let probes = fast_probes(runner, agent_dir, &ProgramPath::from_env(), host);

    checks.extend(node_checks(&probes.node));
    checks.extend(npm_checks(&probes.npm));
    let (pi, pi_path, pi_list) = pi_checks(&probes.pi);
    checks.extend(pi);
    if let Some(check) = latest_pi_check(&latest) {
        checks.push(check);
    }
    checks.push(install_kind_check(agent_dir, pi_path.as_deref()));
    checks.extend(auth_checks(agent_dir, pi_path.as_deref(), runner, host));
    checks.push(packages_check(agent_dir, pi_list.as_deref()));
    checks.push(skills_check(agent_dir, home_dir));

    DoctorReport {
        generated_at: iso8601_now(),
        platform: platform(),
        checks,
        latest,
    }
}

fn latest_pi_check(latest: &LatestVersions) -> Option<Check> {
    match latest.pi.as_ref() {
        Some(item) => Some(match item.version.as_deref() {
            Some(version) => pass("pi.latest", Some(version.to_string()), item.note.clone()),
            None => unknown("pi.latest", item.note.as_deref().unwrap_or("最新版本未知")),
        }),
        // 没有缓存时不冒出一行「未知」，避免把「还没查过」算进环境异常。
        None => latest
            .error
            .as_deref()
            .map(|error| unknown("pi.latest", error)),
    }
}

/// command 层入口：解析出生效宿主目录后跑一次检查。
fn probe(app: &tauri::AppHandle, refresh_versions: bool) -> Result<DoctorReport, String> {
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
    let runner = ShellRunner::from_env();
    let host = HostOs::current();
    let cache_path = config_dir.join(versions::CACHE_FILE_NAME);

    // 默认只读缓存；只有 refresh_versions 为真时才启动最多两个 npm 查询。
    let latest = if refresh_versions {
        let mut sources = BTreeSet::new();
        if let Some(agent_dir) = resolved.path.as_deref() {
            if let Ok(status) = crate::packages::status(agent_dir, &runner, host) {
                sources.extend(
                    status
                        .declared
                        .iter()
                        .filter(|item| item.enabled)
                        .map(|item| item.source.clone()),
                );
                sources.extend(status.installed.iter().map(|item| item.source.clone()));
            }
        }
        let sources: Vec<String> = sources.into_iter().collect();
        versions::load_or_query(&cache_path, &sources, &runner, host, true)
    } else {
        versions::load_cache(&cache_path).unwrap_or_default()
    };

    Ok(check_with_latest(
        resolved.path.as_deref(),
        home_dir.as_deref(),
        &runner,
        host,
        latest,
    ))
}

/// 子进程会阻塞 5 到 15 秒，放阻塞线程池，别占住 UI 线程。
#[tauri::command]
pub async fn run_check(
    app: tauri::AppHandle,
    refresh_versions: bool,
) -> Result<DoctorReport, String> {
    tauri::async_runtime::spawn_blocking(move || probe(&app, refresh_versions))
        .await
        .map_err(|error| format!("环境检测失败：{error}"))?
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

    /// 假命令执行器：按调用顺序返回预置结果，并记录收到的脚本与逐命令调用。
    struct FakeRunner {
        outcomes: RefCell<Vec<CommandOutcome>>,
        calls: RefCell<Vec<(String, Duration)>>,
        programs: RefCell<Vec<(Vec<String>, Duration)>>,
    }

    impl FakeRunner {
        fn new(outcomes: Vec<CommandOutcome>) -> Self {
            Self {
                outcomes: RefCell::new(outcomes),
                calls: RefCell::new(Vec::new()),
                programs: RefCell::new(Vec::new()),
            }
        }

        fn single(outcome: CommandOutcome) -> Self {
            Self::new(vec![outcome.clone(), outcome])
        }

        fn calls(&self) -> Vec<(String, Duration)> {
            self.calls.borrow().clone()
        }

        fn programs(&self) -> Vec<(Vec<String>, Duration)> {
            self.programs.borrow().clone()
        }

        fn next_outcome(&self) -> CommandOutcome {
            let mut outcomes = self.outcomes.borrow_mut();
            if outcomes.is_empty() {
                CommandOutcome::Timeout
            } else {
                outcomes.remove(0)
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, script: &str, timeout: Duration, _env: &[(&str, String)]) -> CommandOutcome {
            self.calls.borrow_mut().push((script.to_string(), timeout));
            self.next_outcome()
        }

        fn run_program(
            &self,
            argv: &[String],
            timeout: Duration,
            _env: &[(&str, String)],
        ) -> CommandOutcome {
            self.programs.borrow_mut().push((argv.to_vec(), timeout));
            self.next_outcome()
        }
    }

    fn ok(stdout: &str) -> CommandOutcome {
        CommandOutcome::Ok {
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    fn failed(code: i32, stderr: &str) -> CommandOutcome {
        CommandOutcome::Failed {
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    /// Windows 的 PATH 与 PATHEXT 替身：后缀顺序取真实默认值的前三个。
    fn windows_search(dirs: &[&Path]) -> ProgramPath {
        ProgramPath::new(
            dirs.iter().map(|dir| dir.to_path_buf()).collect(),
            vec![".cmd".to_string(), ".exe".to_string(), ".bat".to_string()],
        )
    }

    /// 逐命令调用里的 `cmd /d /s /c` 整行原文；POSIX 替身上拿到的是程序与参数。
    fn command_lines(runner: &FakeRunner) -> Vec<String> {
        runner
            .programs()
            .into_iter()
            .map(|(argv, _)| platform::cmd_line_of(&argv))
            .collect()
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

    /// 单测统一按 POSIX 形态检查：命令形态用替身，不碰真实进程，
    /// 这样同一组用例在 Windows 上跑也走登录 shell 那条路。
    fn check_posix(agent_dir: Option<&Path>, runner: &dyn CommandRunner) -> DoctorReport {
        check(agent_dir, None, runner, HostOs::Posix)
    }

    /// 同上，但把 home 目录也交进去：`skills.links` 要扫外部根。
    fn check_posix_home(
        agent_dir: Option<&Path>,
        home_dir: &Path,
        runner: &dyn CommandRunner,
    ) -> DoctorReport {
        check(agent_dir, Some(home_dir), runner, HostOs::Posix)
    }

    fn find<'a>(report: &'a DoctorReport, id: &str) -> &'a Check {
        report
            .checks
            .iter()
            .find(|check| check.id == id)
            .unwrap_or_else(|| panic!("缺少检查项 {id}"))
    }

    #[test]
    fn probe_versions_normalizes_and_leaves_unknown_empty() {
        let node = "path:/usr/bin/node\nversion:v24.14.0";
        let pi = "path:/usr/bin/pi\nversion:pi 1.6.0 (build 2026-11-20)";
        let runner = FakeRunner::single(fast_ok(node, "", pi));
        assert_eq!(
            probe_versions(None, &runner, HostOs::Posix),
            (Some("1.6.0".to_string()), Some("24.14.0".to_string()))
        );

        // 两个命令都没装：留空，不报错——导出照常进行。
        let missing = FakeRunner::single(ok(&format!(
            "{}{}",
            fake_section("node", MISSING_MARKER),
            fake_section("pi", MISSING_MARKER)
        )));
        assert_eq!(probe_versions(None, &missing, HostOs::Posix), (None, None));
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
        let report = check_posix(None, &runner);

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
        let missing = check_posix(
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

        let broken = check_posix(
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
        let report = check_posix(
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
        let report = check_posix(
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

        let report = check_posix(Some(dir.path()), &runner);

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
        let report = check_posix(
            Some(dir.path()),
            &FakeRunner::single(CommandOutcome::Timeout),
        );

        assert_eq!(find(&report, "auth").status, CheckStatus::Unknown);
        assert!(find(&report, "auth").detail.is_some());
    }

    #[test]
    fn latest_pi_check_uses_available_version_and_notes() {
        let available = LatestVersions {
            pi: Some(crate::versions::LatestVersion {
                version: Some("9.9.9".to_string()),
                note: None,
            }),
            ..LatestVersions::default()
        };
        let check = latest_pi_check(&available).expect("有最新版时应输出检查项");
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.value.as_deref(), Some("9.9.9"));

        let unavailable = LatestVersions {
            pi: Some(crate::versions::LatestVersion {
                version: None,
                note: Some("查询超时（10 秒），未知".to_string()),
            }),
            ..LatestVersions::default()
        };
        let unavailable = latest_pi_check(&unavailable).expect("查询失败时应输出检查项");
        assert_eq!(unavailable.status, CheckStatus::Unknown);
        assert_eq!(
            unavailable.detail.as_deref(),
            Some("查询超时（10 秒），未知")
        );
        assert!(latest_pi_check(&LatestVersions::default()).is_none());
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

        let report = check_posix(
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

        let report = check_posix(
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

    #[cfg(unix)]
    #[test]
    fn skills_links_recurse_and_label_both_roots() {
        let agent = TempDir::new("skills-agent");
        let home = TempDir::new("skills-home");
        let skills = agent.path().join("skills");
        let external = home.path().join(".agents/skills");
        let target = agent.path().join("real-skill");
        fs::create_dir_all(&target).expect("建目标目录");
        fs::create_dir_all(skills.join("group")).expect("建嵌套目录");
        fs::create_dir_all(&external).expect("建外部根");
        std::os::unix::fs::symlink(&target, skills.join("good")).unwrap();
        std::os::unix::fs::symlink(agent.path().join("nowhere"), skills.join("bad")).unwrap();
        // 嵌套链接同样要算：只看顶层会漏掉。
        std::os::unix::fs::symlink(agent.path().join("nowhere2"), skills.join("group/deep"))
            .unwrap();
        std::os::unix::fs::symlink(home.path().join("gone"), external.join("external-bad"))
            .unwrap();

        let (links, broken) = skills_links(&skills).expect("扫描");
        assert_eq!(links, 3);
        assert_eq!(broken, vec!["bad".to_string(), "group/deep".to_string()]);

        let report = check_posix_home(
            Some(agent.path()),
            home.path(),
            &FakeRunner::single(CommandOutcome::Timeout),
        );
        let check = find(&report, "skills.links");
        assert_eq!(check.status, CheckStatus::Warn);
        assert_eq!(check.value.as_deref(), Some("4 个链接，3 个断链"));
        let detail = check.detail.clone().unwrap_or_default();
        assert!(detail.contains("宿主目录 里的 skills/bad"), "{detail}");
        assert!(
            detail.contains("宿主目录 里的 skills/group/deep"),
            "{detail}"
        );
        assert!(
            detail.contains("~/.agents/skills 里的 external-bad"),
            "{detail}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn broken_root_link_is_reported_instead_of_skipped() {
        let agent = TempDir::new("skills-broken-root-agent");
        let home = TempDir::new("skills-broken-root-home");
        let external = home.path().join(".agents/skills");
        fs::create_dir_all(home.path().join(".agents")).expect("建 .agents");
        // 外部根本身是断链：`exists()` 跟着链接走，会说它不存在。
        std::os::unix::fs::symlink(home.path().join("gone"), &external).expect("建断链根");

        let report = check_posix_home(
            Some(agent.path()),
            home.path(),
            &FakeRunner::single(CommandOutcome::Timeout),
        );

        let check = find(&report, "skills.links");
        assert_eq!(check.status, CheckStatus::Warn);
        assert_eq!(check.value.as_deref(), Some("0 个链接，1 个断链"));
        let detail = check.detail.clone().unwrap_or_default();
        assert!(
            detail.contains("~/.agents/skills 是断链，按没有该目录处理"),
            "{detail}"
        );
        assert_eq!(check.action.as_deref(), Some("skills.relink"));
    }

    #[cfg(unix)]
    #[test]
    fn linked_root_with_existing_target_passes_and_names_the_target_content() {
        let agent = TempDir::new("skills-linked-root-agent");
        let home = TempDir::new("skills-linked-root-home");
        let real = agent.path().join("real-skills");
        fs::create_dir_all(real.join("demo")).expect("建真实技能目录");
        std::os::unix::fs::symlink(&real, agent.path().join("skills")).expect("建根链接");

        let report = check_posix_home(
            Some(agent.path()),
            home.path(),
            &FakeRunner::single(CommandOutcome::Timeout),
        );

        let check = find(&report, "skills.links");
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.value.as_deref(), Some("0 个链接"));
        let detail = check.detail.clone().unwrap_or_default();
        assert!(
            detail.contains("宿主目录 里的 skills 本身是链接，按链接目标的实际内容检查"),
            "{detail}"
        );
    }

    #[test]
    fn skills_check_reads_both_roots_and_skips_missing_ones() {
        let agent = TempDir::new("skills-both-agent");
        let home = TempDir::new("skills-both-home");
        fs::create_dir_all(agent.path().join("skills")).expect("建 agent 技能目录");
        fs::create_dir_all(home.path().join(".agents/skills")).expect("建外部根");

        let report = check_posix_home(
            Some(agent.path()),
            home.path(),
            &FakeRunner::single(CommandOutcome::Timeout),
        );
        let check = find(&report, "skills.links");
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.value.as_deref(), Some("0 个链接"));

        // 两个根都不存在时是「没有目录」，不是「无法检查」。
        let empty_agent = TempDir::new("skills-empty-agent");
        let empty_home = TempDir::new("skills-empty-home");
        let report = check_posix_home(
            Some(empty_agent.path()),
            empty_home.path(),
            &FakeRunner::single(CommandOutcome::Timeout),
        );
        assert_eq!(
            find(&report, "skills.links").value.as_deref(),
            Some("没有 skills 目录")
        );
    }

    #[test]
    fn install_kind_prefers_managed_marker() {
        let dir = TempDir::new("install-kind");
        dir.write("install/managed-install.json", "{}");

        let report = check_posix(
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
        let report = check_posix(
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
    fn install_kind_accepts_windows_npm_shim() {
        let dir = TempDir::new("npm-shim");
        fs::create_dir_all(dir.path().join("node_modules")).expect("建目录");
        let pi = dir.write("pi.cmd", "");

        let check = install_kind_check(None, Some(&pi.display().to_string()));

        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.value.as_deref(), Some("npm 全局安装"));
    }

    #[test]
    fn agent_dir_missing_reports_action() {
        let dir = TempDir::new("agent-dir");
        let missing = dir.path().join("not-there");

        let report = check_posix(Some(&missing), &FakeRunner::single(CommandOutcome::Timeout));
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

        let report = check_posix(
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
        let report = check_posix(
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

    #[test]
    fn posix_probes_keep_the_single_login_shell_script() {
        let runner = FakeRunner::single(CommandOutcome::Timeout);
        check_posix(None, &runner);

        // POSIX 仍是一条脚本跑完全部探测：没有逐命令调用。
        assert!(runner.programs().is_empty());
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].0.contains("__PI_DOCTOR__BEGIN__node__"));
    }

    #[test]
    fn windows_probes_run_one_command_each() {
        let dir = TempDir::new("windows-shape");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).expect("建目录");
        for name in ["node.exe", "npm.cmd", "pi.cmd"] {
            fs::write(bin.join(name), "").expect("写文件");
        }
        let search = windows_search(&[&bin]);
        let list = "User packages:\n  npm:pi-lens\n    C:\\Users\\u\\.pi\\agent\\npm\\pi-lens";

        let runner = FakeRunner::new(vec![
            ok("22.23.0\n"),
            ok("10.9.2\n"),
            ok("C:\\Users\\u\\AppData\\Roaming\\npm\n"),
            ok("1.5.2\n"),
            ok(list),
        ]);

        let probes = windows_probes(&runner, &search, None);

        let node = bin.join("node.exe").display().to_string();
        let npm = bin.join("npm.cmd").display().to_string();
        let pi = bin.join("pi.cmd").display().to_string();
        assert_eq!(
            command_lines(&runner),
            vec![
                platform::cmd_line(&node, &["-p", "process.versions.node"]),
                platform::cmd_line(&npm, &["-v"]),
                platform::cmd_line(&npm, &["prefix", "-g"]),
                platform::cmd_line(&pi, &["--version"]),
                platform::cmd_line(&pi, &["list"]),
            ]
        );
        // 每个探测点单独 5 秒超时，不拼脚本。
        assert!(runner
            .programs()
            .iter()
            .all(|(_, timeout)| *timeout == PROBE_TIMEOUT));

        let node = node_checks(&probes.node);
        assert_eq!(node[0].status, CheckStatus::Pass);
        assert_eq!(
            node[0].value.as_deref(),
            Some(bin.join("node.exe").display().to_string().as_str())
        );
        assert_eq!(node[1].status, CheckStatus::Pass);
        assert_eq!(node[1].value.as_deref(), Some("22.23.0"));

        assert_eq!(npm_checks(&probes.npm)[0].status, CheckStatus::Pass);

        let (pi_checks, pi_path, pi_list) = pi_checks(&probes.pi);
        assert_eq!(pi_checks[0].status, CheckStatus::Pass);
        assert_eq!(pi_path, Some(bin.join("pi.cmd").display().to_string()));
        assert_eq!(pi_list.as_deref(), Some(list));
    }

    #[test]
    fn windows_probes_report_missing_programs() {
        let dir = TempDir::new("windows-missing");
        let search = windows_search(&[dir.path()]);
        let runner = FakeRunner::single(ok(""));

        let probes = windows_probes(&runner, &search, None);

        assert!(runner.programs().is_empty(), "解析不到程序就不起进程");

        let node = node_checks(&probes.node);
        assert_eq!(node[0].status, CheckStatus::Fail);
        assert!(node[0]
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("tool_not_found"));

        let npm = npm_checks(&probes.npm);
        assert_eq!(npm[0].status, CheckStatus::Fail);
        assert!(npm[0]
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("tool_not_found"));

        let (pi, pi_path, pi_list) = pi_checks(&probes.pi);
        assert_eq!(pi[0].status, CheckStatus::Fail);
        assert!(pi[0]
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("tool_not_found"));
        assert_eq!(pi_path, None);
        assert_eq!(pi_list, None);
    }

    #[test]
    fn windows_probes_report_broken_commands() {
        let dir = TempDir::new("windows-broken");
        fs::write(dir.path().join("node.exe"), "").expect("写文件");
        fs::write(dir.path().join("pi.cmd"), "").expect("写文件");
        let search = windows_search(&[dir.path()]);

        // node 在，`-p` 非零退出；npm 没装；pi 两次调用拿到不同结果。
        let runner = FakeRunner::new(vec![
            failed(1, "boom"),
            ok("1.5.2\n"),
            failed(9, "list failed"),
        ]);

        let probes = windows_probes(&runner, &search, None);

        let node = node_checks(&probes.node);
        assert_eq!(node[0].status, CheckStatus::Pass, "找到 node 就要报出路径");
        assert_eq!(
            node[0].value.as_deref(),
            Some(dir.path().join("node.exe").display().to_string().as_str())
        );
        assert_eq!(node[1].status, CheckStatus::Fail);
        assert!(node[1]
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("tool_broken"));

        assert_eq!(npm_checks(&probes.npm)[0].status, CheckStatus::Fail);

        let (pi, pi_path, _) = pi_checks(&probes.pi);
        assert_eq!(pi[0].status, CheckStatus::Pass);
        assert_eq!(pi[1].status, CheckStatus::Pass);
        assert_eq!(
            pi_path,
            Some(dir.path().join("pi.cmd").display().to_string())
        );
    }

    #[test]
    fn windows_pi_falls_back_to_agent_dir_then_npm_prefix() {
        let agent = TempDir::new("windows-agent");
        fs::create_dir_all(agent.path().join("bin")).expect("建目录");
        fs::write(agent.path().join("bin/pi.cmd"), "").expect("写文件");

        let empty = TempDir::new("windows-empty-path");
        let search = windows_search(&[empty.path()]);
        let pi = agent
            .path()
            .join("bin")
            .join("pi.cmd")
            .display()
            .to_string();

        let runner = FakeRunner::single(ok("1.5.2\n"));
        let found = windows_pi(&runner, &search, Some(agent.path()), None);
        assert!(
            matches!(found, SectionData::Body { .. }),
            "agent_dir/bin 兜底生效"
        );
        assert_eq!(
            command_lines(&runner)[0],
            platform::cmd_line(&pi, &["--version"])
        );

        // npm 全局前缀下的 `pi.cmd`：PATH 与 agent_dir 都找不到时才算数。
        let prefix = TempDir::new("windows-prefix");
        fs::write(prefix.path().join("pi.cmd"), "").expect("写文件");
        let runner = FakeRunner::single(ok("1.5.2\n"));
        let found = windows_pi(
            &runner,
            &search,
            None,
            Some(prefix.path().to_str().unwrap()),
        );
        assert!(matches!(found, SectionData::Body { .. }));
        assert!(command_lines(&runner)[0].contains("pi.cmd"));
    }

    #[test]
    fn windows_auth_runs_pi_auth_check_directly() {
        let dir = TempDir::new("windows-auth");
        let pi = dir.path().join("pi.cmd");
        fs::write(&pi, "").expect("写文件");

        let runner = FakeRunner::single(ok(r#"{"status":"ready"}"#));
        let section = auth_probe_windows(Some(&pi.display().to_string()), "openrouter", &runner);
        let check = auth_check("openrouter", section);

        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(
            command_lines(&runner),
            vec![platform::cmd_line(
                &pi.display().to_string(),
                &["auth", "check", "--provider", "openrouter", "--json"]
            )]
        );
    }

    #[test]
    fn windows_auth_rejects_provider_ids_cmd_would_expand() {
        let dir = TempDir::new("windows-auth-rejected");
        let pi = dir.path().join("pi.cmd");
        fs::write(&pi, "").expect("写文件");

        let runner = FakeRunner::single(ok("{}"));
        let section =
            auth_probe_windows(Some(&pi.display().to_string()), "p%USERPROFILE%", &runner);
        let check = auth_check("p%USERPROFILE%", section);

        assert_eq!(check.status, CheckStatus::Unknown);
        assert!(check
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("拒绝执行"));
        assert!(runner.programs().is_empty(), "未过转义的参数不能交给 cmd");
    }
}

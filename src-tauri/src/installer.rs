//! 宿主安装与升级。
//!
//! 规则来源：`docs/design/20-env-doctor.md` 第 5 节。两段式：`preview` 只探测前置条件并
//! 给出命令，`execute` 在用户确认后才执行。命令由本模块按平台与安装类型拼接，前端只能提交
//! 动作枚举，传不了任意命令。
//!
//! 绝不修改 shell 启动文件，绝不自动安装 Node，失败不自动重试也不换方案。
//! 探测复用医生的 [`CommandRunner`]/[`ShellRunner`]；安装命令的执行走 [`GroupRunner`]，
//! 因为它必须在超时后终止整个进程组（`curl | sh` 会派生孙进程，ShellRunner 只杀直接子进程）。

use crate::config::{self, AGENT_DIR_ENV_VAR};
use crate::doctor::{self, CommandOutcome, CommandRunner, ShellRunner, Version};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tauri::Manager;

/// 安装/升级命令的超时：官方 installer 要下载依赖，10 分钟足够。
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(600);
/// 输出保留上限（字节，只留尾部）。
pub const OUTPUT_LIMIT_BYTES: usize = 200 * 1024;
/// 输出保留行数（只留尾部）。
pub const OUTPUT_TAIL_LINES: usize = 100;

/// probe 输出的行前缀，用来过滤登录 shell 启动脚本的噪音。
const PROBE_PREFIX: &str = "__PI_INSTALL__";
/// 官方 installer 地址。
const INSTALL_SCRIPT_URL: &str = "https://pi.dev/install.sh";
/// npm 全局包名。
const NPM_PACKAGE: &str = "@earendil-works/pi-coding-agent";
/// 托管安装的标记文件（相对宿主目录）。
const MANAGED_MARKER: [&str; 2] = ["install", "managed-install.json"];

/// 用户要做的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallAction {
    Install,
    Upgrade,
}

/// 现有 pi 的安装类型，决定升级命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallKind {
    /// 存在 `<agent_dir>/install/managed-install.json`。
    Managed,
    /// 入口路径在 `node_modules` 下。
    Npm,
    /// 两者都不是：不猜命令，交给用户手动升级。
    Unknown,
}

/// 一条将执行的命令。`argv` 是完整命令行：POSIX 走登录 shell（`-lc`），Windows 直接调 npm。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallCommand {
    pub action: InstallAction,
    pub argv: Vec<String>,
}

/// 一次预览：前置条件、现有安装情况、将执行的命令。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallPreview {
    pub node_version: Option<String>,
    pub npm_version: Option<String>,
    pub pi_path: Option<String>,
    pub pi_version: Option<String>,
    pub install_kind: Option<InstallKind>,
    /// `None` 表示前置条件不满足（见 `blocking`）或平台不支持，不执行任何命令。
    pub command: Option<InstallCommand>,
    /// 阻断原因，按探测顺序。
    pub blocking: Vec<String>,
}

/// 执行结果的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallStatus {
    Succeeded,
    Failed,
    Timeout,
    Unavailable,
}

/// 一次执行的结果。输出已脱敏。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallResult {
    pub action: InstallAction,
    pub status: InstallStatus,
    pub exit_code: Option<i32>,
    pub output: String,
    pub truncated: bool,
    pub elapsed_ms: u64,
}

/// 安装命令执行器。生产用 [`GroupRunner`]，单测用假实现，不碰真实进程。
pub trait InstallRunner {
    fn run(&self, argv: &[String], timeout: Duration) -> CommandOutcome;
}

/// 生产执行器：子进程独立成进程组，超时后终止整组。
pub struct GroupRunner;

impl InstallRunner for GroupRunner {
    fn run(&self, argv: &[String], timeout: Duration) -> CommandOutcome {
        let Some((program, args)) = argv.split_first() else {
            return CommandOutcome::Unavailable("命令为空".to_string());
        };

        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return CommandOutcome::Unavailable(error.to_string()),
        };

        let stdout_reader = child.stdout.take().map(read_async);
        let stderr_reader = child.stderr.take().map(read_async);
        let deadline = Instant::now() + timeout;

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() >= deadline => {
                    kill_group(&mut child);
                    break None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(error) => {
                    kill_group(&mut child);
                    return CommandOutcome::Unavailable(error.to_string());
                }
            }
        };

        let stdout = join_reader(stdout_reader);
        let stderr = join_reader(stderr_reader);

        match status {
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

fn read_async(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
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

/// 先终止整个进程组，再兜底杀直接子进程并回收。
fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    {
        // 子进程是组长（`process_group(0)`），组号等于它的 pid。
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg("--")
            .arg(format!("-{}", child.id()))
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// 前置探测的输出。
#[derive(Debug, Default)]
struct Probe {
    node_path: String,
    node_version: String,
    npm_version: String,
    pi_path: String,
    pi_version: String,
}

/// 一条登录 shell 命令跑完全部探测：GUI 进程的 PATH 通常不含 nvm/fnm/mise。
fn probe_script() -> String {
    format!(
        r#"n="$(command -v node 2>/dev/null)"
if [ -z "$n" ]; then
  for c in "$HOME"/.nvm/versions/node/*/bin/node "$HOME"/.local/share/fnm/aliases/default/bin/node "$HOME"/.volta/bin/node /usr/local/bin/node /usr/bin/node; do
    if [ -x "$c" ]; then n="$c"; break; fi
  done
fi
m="$(command -v npm 2>/dev/null)"
p="$(command -v pi 2>/dev/null)"
if [ -z "$p" ] && [ -n "$PI_INSTALL_AGENT_DIR" ] && [ -x "$PI_INSTALL_AGENT_DIR/bin/pi" ]; then
  p="$PI_INSTALL_AGENT_DIR/bin/pi"
fi
echo "{PROBE_PREFIX}node_path:$n"
echo "{PROBE_PREFIX}node_version:$([ -n "$n" ] && "$n" -p 'process.versions.node' 2>/dev/null)"
echo "{PROBE_PREFIX}npm_version:$([ -n "$m" ] && "$m" -v 2>/dev/null)"
echo "{PROBE_PREFIX}pi_path:$p"
echo "{PROBE_PREFIX}pi_version:$([ -n "$p" ] && "$p" --version 2>/dev/null | head -n 1)"
"#
    )
}

/// 只认带前缀的行，其余噪音一律丢弃。返回 `None` 表示探测一行都没打印。
fn parse_probe(stdout: &str) -> Option<Probe> {
    let mut probe = Probe::default();
    let mut seen = false;

    for line in stdout.lines() {
        let Some(rest) = line.trim_end().strip_prefix(PROBE_PREFIX) else {
            continue;
        };
        let Some((key, value)) = rest.split_once(':') else {
            continue;
        };
        seen = true;
        let value = value.trim().to_string();
        match key {
            "node_path" => probe.node_path = value,
            "node_version" => probe.node_version = value,
            "npm_version" => probe.npm_version = value,
            "pi_path" => probe.pi_path = value,
            "pi_version" => probe.pi_version = value,
            _ => {}
        }
    }

    seen.then_some(probe)
}

/// 命令选择：安装走官方 installer（Linux/macOS）或 npm（Windows），升级按安装类型。
fn select_command(
    os: &str,
    installed: bool,
    kind: InstallKind,
) -> Result<InstallCommand, String> {
    let action = if installed {
        InstallAction::Upgrade
    } else {
        InstallAction::Install
    };

    if os == "windows" {
        // 官方 install.ps1 在输入被重定向时可能静默接受 Portable Git 的默认选项，v1 规避。
        let package = if installed {
            format!("{NPM_PACKAGE}@latest")
        } else {
            NPM_PACKAGE.to_string()
        };
        return Ok(InstallCommand {
            action,
            argv: vec!["npm".to_string(), "i".to_string(), "-g".to_string(), package],
        });
    }

    if os != "linux" && os != "macos" {
        return Err(format!(
            "当前平台 {os} 不支持自动安装或升级，请按官方文档手动安装"
        ));
    }

    let script = match (installed, kind) {
        // 官方 installer 会 pin 依赖；经登录 shell 执行且不分配终端，行为确定。
        (false, _) => format!("curl -fsSL {INSTALL_SCRIPT_URL} | sh"),
        (true, InstallKind::Managed) => "pi update".to_string(),
        (true, InstallKind::Npm) => format!("npm i -g {NPM_PACKAGE}@latest"),
        (true, InstallKind::Unknown) => {
            return Err(
                "无法判断 Pi 的安装方式（既不是托管安装也不是 npm 全局安装），请手动升级"
                    .to_string(),
            )
        }
    };

    Ok(InstallCommand {
        action,
        argv: shell_argv(&script),
    })
}

/// POSIX 命令统一走登录 shell：只有这样才拿得到 nvm/fnm/mise 的 PATH。
fn shell_argv(script: &str) -> Vec<String> {
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("sh"));
    vec![
        shell.to_string_lossy().into_owned(),
        "-lc".to_string(),
        script.to_string(),
    ]
}

/// 托管安装看标记文件，npm 全局安装看入口路径。
fn install_kind(agent_dir: Option<&Path>, pi_path: &str) -> InstallKind {
    let marker = agent_dir
        .map(|dir| dir.join(MANAGED_MARKER[0]).join(MANAGED_MARKER[1]))
        .filter(|path| path.exists());

    if marker.is_some() {
        InstallKind::Managed
    } else if pi_path.contains("node_modules") {
        InstallKind::Npm
    } else {
        InstallKind::Unknown
    }
}

/// 探测前置条件并给出命令，不执行。
pub fn preview(agent_dir: Option<&Path>, runner: &dyn CommandRunner) -> InstallPreview {
    let env = [(
        "PI_INSTALL_AGENT_DIR",
        agent_dir
            .map(|dir| dir.display().to_string())
            .unwrap_or_default(),
    )];
    let outcome = runner.run(&probe_script(), doctor::PROBE_TIMEOUT, &env);

    build_preview(agent_dir, std::env::consts::OS, &outcome)
}

/// 预览的纯逻辑部分，平台由调用方给定，便于单测覆盖命令选择矩阵。
fn build_preview(agent_dir: Option<&Path>, os: &str, outcome: &CommandOutcome) -> InstallPreview {
    let mut preview = InstallPreview {
        node_version: None,
        npm_version: None,
        pi_path: None,
        pi_version: None,
        install_kind: None,
        command: None,
        blocking: Vec::new(),
    };

    let stdout = match outcome {
        CommandOutcome::Ok { stdout, .. } | CommandOutcome::Failed { stdout, .. } => stdout,
        CommandOutcome::Timeout => {
            preview.blocking = vec!["探测前置条件超时（probe_timeout），请重新检测".to_string()];
            return preview;
        }
        CommandOutcome::Unavailable(detail) => {
            preview.blocking = vec![format!("无法执行探测命令（{detail}）")];
            return preview;
        }
    };

    let Some(probe) = parse_probe(stdout) else {
        preview.blocking = vec!["探测没有返回结果（tool_broken），请重新检测".to_string()];
        return preview;
    };

    let min = doctor::MIN_NODE_VERSION;
    let min_text = format!("{}.{}.{}", min.0, min.1, min.2);

    if probe.node_path.is_empty() {
        preview.blocking.push(format!(
            "未找到 Node.js（tool_not_found）：需要 {min_text} 或更高版本，本应用不会自动安装 Node"
        ));
    } else {
        match doctor::parse_version(&probe.node_version) {
            Some(version) if version >= min => {
                preview.node_version = Some(probe.node_version.clone());
            }
            Some(version) => preview.blocking.push(format!(
                "Node {} 低于要求的 {min_text}",
                version_text(version)
            )),
            None => preview.blocking.push("Node 版本无法解析（version_unparsable）".to_string()),
        }
    }

    if probe.npm_version.is_empty() {
        preview
            .blocking
            .push("未找到 npm（tool_not_found）：npm 随 Node 一起安装".to_string());
    } else {
        preview.npm_version = Some(probe.npm_version.clone());
    }

    let installed = !probe.pi_path.is_empty();
    if installed {
        preview.pi_path = Some(probe.pi_path.clone());
        if !probe.pi_version.is_empty() {
            preview.pi_version = Some(probe.pi_version.clone());
        }
        preview.install_kind = Some(install_kind(agent_dir, &probe.pi_path));
    }

    if !preview.blocking.is_empty() {
        return preview;
    }

    match select_command(
        os,
        installed,
        preview.install_kind.unwrap_or(InstallKind::Unknown),
    ) {
        Ok(command) => preview.command = Some(command),
        Err(reason) => preview.blocking.push(reason),
    }

    preview
}

fn version_text(version: Version) -> String {
    format!("{}.{}.{}", version.0, version.1, version.2)
}

/// 执行安装/升级。先重新探测确认命令，未确认（`confirm == false`）一律不执行。
pub fn execute(
    agent_dir: Option<&Path>,
    action: InstallAction,
    confirm: bool,
    probes: &dyn CommandRunner,
    runner: &dyn InstallRunner,
) -> Result<InstallResult, String> {
    if !confirm {
        return Err("install_declined：未确认，未执行任何命令".to_string());
    }

    let preview = preview(agent_dir, probes);
    if !preview.blocking.is_empty() {
        return Err(preview.blocking.join("；"));
    }
    let command = match preview.command {
        Some(command) if command.action == action => command,
        Some(command) => {
            return Err(format!(
                "当前状态只支持「{}」，请重新检测后再执行",
                action_text(command.action)
            ))
        }
        None => return Err("没有可执行的命令，请重新检测".to_string()),
    };

    let started = Instant::now();
    let outcome = runner.run(&command.argv, EXEC_TIMEOUT);

    Ok(result_of(action, outcome, started.elapsed()))
}

fn action_text(action: InstallAction) -> &'static str {
    match action {
        InstallAction::Install => "安装",
        InstallAction::Upgrade => "升级",
    }
}

/// 退出码、脱敏后的尾部输出、耗时。
fn result_of(action: InstallAction, outcome: CommandOutcome, elapsed: Duration) -> InstallResult {
    let (status, exit_code, stdout, stderr) = match outcome {
        CommandOutcome::Ok { stdout, stderr } => {
            (InstallStatus::Succeeded, Some(0), stdout, stderr)
        }
        CommandOutcome::Failed {
            code,
            stdout,
            stderr,
        } => (InstallStatus::Failed, code, stdout, stderr),
        CommandOutcome::Timeout => (
            InstallStatus::Timeout,
            None,
            String::new(),
            String::new(),
        ),
        CommandOutcome::Unavailable(detail) => {
            (InstallStatus::Unavailable, None, String::new(), detail)
        }
    };
    let (output, truncated) = tail_output(&stdout, &stderr);

    InstallResult {
        action,
        status,
        exit_code,
        output,
        truncated,
        elapsed_ms: elapsed.as_millis() as u64,
    }
}

/// 只留尾部：先按字节截到 200 KB（按字符边界），再取最后 100 行，最后脱敏。
fn tail_output(stdout: &str, stderr: &str) -> (String, bool) {
    let mut text = match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (true, _) => stderr.trim_start().to_string(),
        (false, true) => stdout.trim_end().to_string(),
        (false, false) => format!("{}\n{}", stdout.trim_end(), stderr.trim_start()),
    };
    let mut truncated = false;

    if text.len() > OUTPUT_LIMIT_BYTES {
        let mut start = text.len() - OUTPUT_LIMIT_BYTES;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text = text[start..].to_string();
        truncated = true;
    }

    let lines: Vec<&str> = text.lines().collect();
    let tail = if lines.len() > OUTPUT_TAIL_LINES {
        truncated = true;
        lines[lines.len() - OUTPUT_TAIL_LINES..].join("\n")
    } else {
        lines.join("\n")
    };

    (doctor::redact(&tail), truncated)
}

/// command 层入口：解析出生效宿主目录。
fn resolve_agent_dir(app: &tauri::AppHandle) -> Result<Option<PathBuf>, String> {
    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("无法定位应用配置目录：{error}"))?;
    let config = config::load_config(&config_dir).map_err(|error| error.to_string())?;
    let env_agent_dir = std::env::var_os(AGENT_DIR_ENV_VAR).map(PathBuf::from);
    let home_dir = app.path().home_dir().ok();

    Ok(config::resolve_agent_dir(&config, env_agent_dir.as_deref(), home_dir.as_deref()).path)
}

/// 只探测，不执行。探测要走登录 shell，放阻塞线程池。
#[tauri::command]
pub async fn preview_install(app: tauri::AppHandle) -> Result<InstallPreview, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let agent_dir = resolve_agent_dir(&app)?;
        Ok(preview(agent_dir.as_deref(), &ShellRunner::from_env()))
    })
    .await
    .map_err(|error| format!("预览安装命令失败：{error}"))?
}

/// 确认后执行安装/升级。最长 10 分钟，同样不占 UI 线程。
#[tauri::command]
pub async fn install_or_upgrade(
    app: tauri::AppHandle,
    action: InstallAction,
    confirm: bool,
) -> Result<InstallResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let agent_dir = resolve_agent_dir(&app)?;
        execute(
            agent_dir.as_deref(),
            action,
            confirm,
            &ShellRunner::from_env(),
            &GroupRunner,
        )
    })
    .await
    .map_err(|error| format!("执行安装失败：{error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;
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
                "agent-config-sync-installer-{}-{}-{}",
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

        fn write(&self, name: &str) {
            let path = self.path.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, "{}").unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// 假探测执行器：只回放预置结果，不跑进程。
    struct FakeProbe(CommandOutcome);

    impl CommandRunner for FakeProbe {
        fn run(&self, _script: &str, _timeout: Duration, _env: &[(&str, String)]) -> CommandOutcome {
            self.0.clone()
        }
    }

    /// 假安装执行器：记录请求的命令行，回放预置结果。
    struct FakeInstall {
        outcome: CommandOutcome,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl FakeInstall {
        fn new(outcome: CommandOutcome) -> Self {
            Self {
                outcome,
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }
    }

    impl InstallRunner for FakeInstall {
        fn run(&self, argv: &[String], _timeout: Duration) -> CommandOutcome {
            self.calls.borrow_mut().push(argv.to_vec());
            self.outcome.clone()
        }
    }

    fn ok(stdout: &str) -> CommandOutcome {
        CommandOutcome::Ok {
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    /// 真实的探测输出形态：带前缀的行，前后是 shell 启动噪音。
    fn probe_stdout(node_path: &str, node_version: &str, npm: &str, pi_path: &str, pi_version: &str) -> String {
        format!(
            "shell noise\n{PROBE_PREFIX}node_path:{node_path}\n{PROBE_PREFIX}node_version:{node_version}\n{PROBE_PREFIX}npm_version:{npm}\n{PROBE_PREFIX}pi_path:{pi_path}\n{PROBE_PREFIX}pi_version:{pi_version}\nmore noise\n"
        )
    }

    fn healthy(node_path: &str, pi_path: &str, pi_version: &str) -> CommandOutcome {
        ok(&probe_stdout(node_path, "22.23.0", "10.9.0", pi_path, pi_version))
    }

    fn preview_for(agent_dir: Option<&Path>, os: &str, outcome: CommandOutcome) -> InstallPreview {
        build_preview(agent_dir, os, &outcome)
    }

    fn script_of(command: &InstallCommand) -> &str {
        command.argv.last().map(String::as_str).unwrap_or_default()
    }

    #[test]
    fn install_command_matrix() {
        let install = select_command("linux", false, InstallKind::Unknown).unwrap();
        assert_eq!(install.action, InstallAction::Install);
        assert_eq!(install.argv[1], "-lc");
        assert_eq!(script_of(&install), "curl -fsSL https://pi.dev/install.sh | sh");

        let macos = select_command("macos", false, InstallKind::Unknown).unwrap();
        assert_eq!(script_of(&macos), "curl -fsSL https://pi.dev/install.sh | sh");

        let managed = select_command("linux", true, InstallKind::Managed).unwrap();
        assert_eq!(managed.action, InstallAction::Upgrade);
        assert_eq!(script_of(&managed), "pi update");

        let npm = select_command("linux", true, InstallKind::Npm).unwrap();
        assert_eq!(
            script_of(&npm),
            "npm i -g @earendil-works/pi-coding-agent@latest"
        );
    }

    #[test]
    fn windows_always_uses_npm() {
        let install = select_command("windows", false, InstallKind::Unknown).unwrap();
        assert_eq!(install.action, InstallAction::Install);
        assert_eq!(
            install.argv,
            vec!["npm", "i", "-g", "@earendil-works/pi-coding-agent"]
        );

        let upgrade = select_command("windows", true, InstallKind::Npm).unwrap();
        assert_eq!(upgrade.action, InstallAction::Upgrade);
        assert_eq!(
            upgrade.argv,
            vec![
                "npm",
                "i",
                "-g",
                "@earendil-works/pi-coding-agent@latest"
            ]
        );
    }

    #[test]
    fn unknown_platform_and_kind_are_refused() {
        assert!(select_command("freebsd", false, InstallKind::Unknown).is_err());
        assert!(select_command("linux", true, InstallKind::Unknown).is_err());
    }

    #[test]
    fn missing_node_blocks_without_command() {
        let preview = preview_for(None, "linux", healthy("", "", ""));
        assert!(preview.command.is_none());
        assert_eq!(preview.blocking.len(), 1);
        assert!(preview.blocking[0].contains("Node"));
        // pi 的探测结果仍然展示，只是不能执行。
        assert_eq!(preview.pi_version, None);
    }

    #[test]
    fn old_node_blocks_without_command() {
        let outcome = ok(&probe_stdout("/usr/bin/node", "18.19.0", "9.0.0", "", ""));
        let preview = preview_for(None, "linux", outcome);
        assert!(preview.command.is_none());
        assert!(preview.blocking[0].contains("22.19.0"));
        assert!(preview.blocking[0].contains("18.19.0"));
    }

    #[test]
    fn missing_npm_blocks_without_command() {
        let outcome = ok(&probe_stdout("/usr/bin/node", "22.23.0", "", "", ""));
        let preview = preview_for(None, "linux", outcome);
        assert!(preview.command.is_none());
        assert!(preview.blocking.iter().any(|reason| reason.contains("npm")));
    }

    #[test]
    fn missing_pi_offers_install() {
        let preview = preview_for(None, "linux", healthy("/usr/bin/node", "", ""));
        let command = preview.command.expect("未安装时给出安装命令");
        assert_eq!(command.action, InstallAction::Install);
        assert_eq!(script_of(&command), "curl -fsSL https://pi.dev/install.sh | sh");
        assert_eq!(preview.install_kind, None);
        assert_eq!(preview.node_version.as_deref(), Some("22.23.0"));
    }

    #[test]
    fn managed_marker_selects_pi_update() {
        let dir = TempDir::new("managed");
        dir.write("install/managed-install.json");

        let preview = preview_for(
            Some(dir.path()),
            "linux",
            healthy("/usr/bin/node", "/usr/local/bin/pi", "1.5.2"),
        );
        assert_eq!(preview.install_kind, Some(InstallKind::Managed));
        let command = preview.command.expect("托管安装给出升级命令");
        assert_eq!(command.action, InstallAction::Upgrade);
        assert_eq!(script_of(&command), "pi update");
    }

    #[test]
    fn node_modules_path_selects_npm_upgrade() {
        let preview = preview_for(
            None,
            "linux",
            healthy(
                "/usr/bin/node",
                "/usr/lib/node_modules/@earendil-works/pi-coding-agent/bin/pi",
                "1.5.2",
            ),
        );
        assert_eq!(preview.install_kind, Some(InstallKind::Npm));
        let command = preview.command.expect("npm 安装给出 npm 升级命令");
        assert_eq!(
            script_of(&command),
            "npm i -g @earendil-works/pi-coding-agent@latest"
        );
    }

    #[test]
    fn probe_failures_block() {
        let timeout = preview_for(None, "linux", CommandOutcome::Timeout);
        assert!(timeout.command.is_none());
        assert!(timeout.blocking[0].contains("probe_timeout"));

        let unavailable =
            preview_for(None, "linux", CommandOutcome::Unavailable("no shell".to_string()));
        assert!(unavailable.command.is_none());
        assert!(unavailable.blocking[0].contains("no shell"));

        let empty = preview_for(None, "linux", ok("shell noise only\n"));
        assert!(empty.command.is_none());
        assert!(empty.blocking[0].contains("tool_broken"));
    }

    #[test]
    fn unsupported_platform_blocks_with_reason() {
        let preview = preview_for(None, "freebsd", healthy("/usr/bin/node", "", ""));
        assert!(preview.command.is_none());
        assert!(preview.blocking[0].contains("freebsd"));
    }

    #[test]
    fn commands_contain_no_secret_markers() {
        let commands = [
            select_command("linux", false, InstallKind::Unknown).unwrap(),
            select_command("linux", true, InstallKind::Managed).unwrap(),
            select_command("linux", true, InstallKind::Npm).unwrap(),
            select_command("windows", false, InstallKind::Unknown).unwrap(),
        ];

        for command in commands {
            let line = command.argv.join(" ");
            assert_eq!(doctor::redact(&line), line);
        }
    }

    #[test]
    fn output_keeps_only_last_hundred_lines() {
        let stdout: String = (0..300).map(|index| format!("line {index}\n")).collect();
        let (output, truncated) = tail_output(&stdout, "");
        assert!(truncated);
        assert_eq!(output.lines().count(), OUTPUT_TAIL_LINES);
        assert!(output.starts_with("line 200"));
        assert!(output.ends_with("line 299"));
    }

    #[test]
    fn output_is_capped_at_200_kb() {
        let stdout = "x".repeat(OUTPUT_LIMIT_BYTES * 2);
        let (output, truncated) = tail_output(&stdout, "");
        assert!(truncated);
        assert!(output.len() <= OUTPUT_LIMIT_BYTES);
        assert_eq!(output.len(), OUTPUT_LIMIT_BYTES);
    }

    #[test]
    fn output_is_redacted() {
        let stdout = "export PI_API_KEY=sk-live-1234567890\nauthorization: Bearer abc.def\nok\n";
        let (output, _) = tail_output(stdout, "");
        assert!(output.contains("<redacted>"));
        assert!(!output.contains("sk-live-1234567890"));
        assert!(!output.contains("abc.def"));
    }

    #[test]
    fn result_reports_exit_code_and_timeout() {
        let failed = result_of(
            InstallAction::Install,
            CommandOutcome::Failed {
                code: Some(3),
                stdout: "boom\n".to_string(),
                stderr: String::new(),
            },
            Duration::from_millis(1200),
        );
        assert_eq!(failed.status, InstallStatus::Failed);
        assert_eq!(failed.exit_code, Some(3));
        assert_eq!(failed.output, "boom");
        assert_eq!(failed.elapsed_ms, 1200);

        let timeout = result_of(InstallAction::Upgrade, CommandOutcome::Timeout, Duration::ZERO);
        assert_eq!(timeout.status, InstallStatus::Timeout);
        assert_eq!(timeout.exit_code, None);
    }

    #[test]
    fn execute_refuses_without_confirm() {
        let probes = FakeProbe(healthy("/usr/bin/node", "", ""));
        let runner = FakeInstall::new(ok(""));

        let error = execute(None, InstallAction::Install, false, &probes, &runner).unwrap_err();
        assert!(error.contains("install_declined"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn execute_refuses_when_blocked() {
        let probes = FakeProbe(healthy("", "", ""));
        let runner = FakeInstall::new(ok(""));

        let error = execute(None, InstallAction::Install, true, &probes, &runner).unwrap_err();
        assert!(error.contains("Node"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn execute_refuses_action_mismatch() {
        let probes = FakeProbe(healthy("/usr/bin/node", "", ""));
        let runner = FakeInstall::new(ok(""));

        let error = execute(None, InstallAction::Upgrade, true, &probes, &runner).unwrap_err();
        assert!(error.contains("安装"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn execute_runs_the_previewed_command() {
        let probes = FakeProbe(healthy("/usr/bin/node", "", ""));
        let runner = FakeInstall::new(ok("installed\n"));

        let result = execute(None, InstallAction::Install, true, &probes, &runner).unwrap();
        assert_eq!(result.status, InstallStatus::Succeeded);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.output, "installed");
        assert!(!result.truncated);

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].last().map(String::as_str),
            Some("curl -fsSL https://pi.dev/install.sh | sh")
        );
    }

    #[cfg(unix)]
    #[test]
    fn group_runner_reports_nonzero_exit_without_touching_disk() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo failing >&2; exit 7".to_string(),
        ];
        match GroupRunner.run(&argv, Duration::from_secs(10)) {
            CommandOutcome::Failed { code, stderr, .. } => {
                assert_eq!(code, Some(7));
                assert!(stderr.contains("failing"));
            }
            other => panic!("期望非零退出，实际 {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn group_runner_times_out_and_kills_the_group() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "sleep 30 & wait".to_string(),
        ];
        let started = Instant::now();
        let outcome = GroupRunner.run(&argv, Duration::from_millis(150));
        assert_eq!(outcome, CommandOutcome::Timeout);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}

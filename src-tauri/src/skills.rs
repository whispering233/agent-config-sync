//! 技能盘点：起宿主 RPC 读 `get_commands`，只列出宿主实际加载的用户级技能。
//!
//! 规则来源：`docs/design/20-env-doctor.md` 的「技能盘点」。
//! 只在用户显式点击后执行，不随页面加载自动跑：RPC 会启动宿主，宿主可能在缺失时
//! 创建空的 `auth.json`/`models-store.json`。这里在执行前后对比存在性，把新出现的
//! 文件报给 UI；不删除、不改写，也不替宿主做技能发现。

use crate::config::{self, AGENT_DIR_ENV_VAR};
use crate::doctor::{self, CommandOutcome, CommandRunner, ShellRunner};
use crate::platform::{self, HostOs, ProgramPath};
use crate::scope::{self, RootId};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// 宿主 RPC 的超时：要启动 Node 与宿主本身，比普通探测宽。超时不重试。
pub const RPC_TIMEOUT: Duration = Duration::from_secs(15);

/// 宿主目录里可能被本次 RPC 新建的空文件。只检测它们的出现，不清理。
const HOST_SIDE_EFFECT_FILES: [&str; 2] = ["auth.json", "models-store.json"];

/// 一次请求一行 JSON：宿主按行读 stdin。
const GET_COMMANDS_REQUEST: &str = "{\"type\":\"get_commands\"}\n";

/// POSIX 登录 shell 里「找不到 pi」的自报标记：脚本自己退出码不可靠，
/// 只有 stderr 的标记能区分「没装宿主」与「宿主跑失败」。
const POSIX_MISSING_MARKER: &str = "__PI_SKILLS_NO_PI__";

/// POSIX 脚本里兜底找 `<agent_dir>/bin/pi` 用的变量名，由 [`HostLauncher::command`] 注入。
const POSIX_AGENT_DIR_ENV: &str = "PI_SKILLS_AGENT_DIR";

/// 宿主 RPC 的参数。`--no-extensions` 会漏掉扩展动态注册的技能，UI 需注明。
const RPC_ARGS: [&str; 6] = [
    "--mode",
    "rpc",
    "--offline",
    "--no-session",
    "--no-extensions",
    "--no-approve",
];

/// 盘点方法的固有边界：始终随结果返回，避免 UI 把「没列出来」当成「没有技能」。
const STATIC_WARNINGS: [&str; 2] = [
    "--no-extensions 会漏掉扩展在运行时注册的技能；这类技能本来也不在快照内。",
    "只列宿主实际加载的技能：frontmatter 无效或名称冲突被跳过的 SKILL.md 不会出现，但导出仍按文件收录。",
];

/// 技能与快照的关系。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    /// 用户级受管根内，随快照。
    User,
    /// 由扩展包提供，随包重装恢复。
    Package,
    /// 项目级目录，本应用不纳管。
    Project,
    /// 其它来源：CLI 内置、受管根之外的路径等。
    Other,
}

/// 一个技能条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillEntry {
    pub name: String,
    pub description: Option<String>,
    pub path: Option<String>,
    /// 随快照时的受管根；不随快照为 `None`。
    pub root: Option<RootId>,
    pub snapshot: bool,
    pub source: SkillSource,
    /// 不随快照的原因；随快照时为 `None`。
    pub reason: Option<String>,
}

/// 一次盘点结果。RPC 失败也保留已观察到的副作用，不静默丢弃。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInventory {
    /// RPC 失败时的可读原因；成功为 `None`（此时 `skills` 才是权威清单）。
    pub error: Option<String>,
    pub skills: Vec<SkillEntry>,
    pub warnings: Vec<String>,
    /// 本次 RPC 在宿主目录新出现的文件。
    pub created_files: Vec<String>,
}

/// 起宿主 RPC 的执行器。单测注入假实现，不启动真实 `pi`。
pub trait RpcLauncher {
    /// 把 `request` 写进 stdin 并读到 EOF，返回宿主的原始输出。
    fn get_commands(&self, request: &str, timeout: Duration) -> CommandOutcome;
}

/// 生产执行器：POSIX 交给登录 shell 解析 `pi`，Windows 用解析到的入口拼命令行。
pub struct HostLauncher {
    host: HostOs,
    agent_dir: Option<PathBuf>,
    search: ProgramPath,
    /// 宿主的工作目录。必须是不含项目配置的中性目录：宿主按 cwd 找项目配置与项目级技能，
    /// 用用户当前项目的话清单里会混进本应用不纳管的条目。
    cwd: PathBuf,
    /// 只用于 Windows 上探测 `npm prefix -g`，给 `pi.cmd` 兜底；POSIX 走登录 shell 不需要。
    runner: ShellRunner,
}

impl HostLauncher {
    pub fn from_env(agent_dir: Option<PathBuf>, cwd: PathBuf) -> Self {
        Self {
            host: HostOs::current(),
            agent_dir,
            search: ProgramPath::from_env(),
            cwd,
            runner: ShellRunner::from_env(),
        }
    }

    /// 起宿主进程的命令行与工作目录。抽成方法后单测能直接读 cwd，不必真跑 `pi`。
    fn command(&self) -> Result<Command, String> {
        let argv = match self.host {
            HostOs::Posix => crate::installer::shell_argv(&posix_script()),
            HostOs::Windows => windows_argv(&self.search, self.agent_dir.as_deref(), &self.runner)?,
        };
        let mut command = platform::build_command(&argv)?;
        if let Some(dir) = &self.agent_dir {
            // 宿主按 PI_CODING_AGENT_DIR 决定读哪个宿主目录。应用里可以配置非默认目录，
            // 不传的话清单读的是另一个目录，副作用检测也会盯错位置。
            command.env(AGENT_DIR_ENV_VAR, dir);
            if self.host == HostOs::Posix {
                // POSIX 脚本用它兜底找 `<agent_dir>/bin/pi`；Windows 的入口由 Rust 侧按路径解析。
                command.env(POSIX_AGENT_DIR_ENV, dir);
            }
        }
        command.current_dir(&self.cwd);
        Ok(command)
    }
}

impl RpcLauncher for HostLauncher {
    fn get_commands(&self, request: &str, timeout: Duration) -> CommandOutcome {
        let command = match self.command() {
            Ok(command) => command,
            Err(detail) => return CommandOutcome::Unavailable(detail),
        };
        // 独立进程组：超时按组终止，宿主的子进程不会留在后台。
        doctor::run_child_with_input(command, timeout, true, Some(request))
    }
}

/// POSIX 走登录 shell：GUI 进程的 PATH 里没有 nvm/fnm/mise 的路径。
/// 入口按医生同一顺序兜底：`PATH` → `<agent_dir>/bin/pi` → `npm prefix -g` 下的 `bin/pi`，
/// 都没有才自报标记退出，免得把「没装」说成「宿主跑失败」。
/// `exec "$p"` 让宿主直接接下 stdin/stdout，不必经 shell 转发。
fn posix_script() -> String {
    format!(
        r#"p="$(command -v pi 2>/dev/null)"
if [ -z "$p" ] && [ -n "${POSIX_AGENT_DIR_ENV}" ] && [ -x "${POSIX_AGENT_DIR_ENV}/bin/pi" ]; then
  p="${POSIX_AGENT_DIR_ENV}/bin/pi"
fi
if [ -z "$p" ]; then
  np="$(npm prefix -g 2>/dev/null)"
  if [ -n "$np" ] && [ -x "$np/bin/pi" ]; then p="$np/bin/pi"; fi
fi
if [ -z "$p" ]; then
  printf '%s\n' '{POSIX_MISSING_MARKER}' >&2
  exit 127
fi
exec "$p" {}"#,
        RPC_ARGS.join(" ")
    )
}

/// Windows：`pi` 是 `.cmd` 中间层，入口与医生、安装页、包对齐走同一个 [`doctor::find_pi`]，
/// npm 全局前缀的兜底也一致；整条命令行由 [`platform::program_argv`] 按 `cmd /d /s /c` 拼。
fn windows_argv(
    search: &ProgramPath,
    agent_dir: Option<&Path>,
    runner: &dyn CommandRunner,
) -> Result<Vec<String>, String> {
    let npm_prefix = search
        .find("npm")
        .and_then(|npm| doctor::npm_global_prefix(runner, &npm));
    let program = doctor::find_pi(search, agent_dir, npm_prefix.as_deref())
        .ok_or_else(|| "tool_not_found：找不到 pi 命令，无法读取技能清单".to_string())?;
    platform::program_argv(HostOs::Windows, &program.display().to_string(), &RPC_ARGS)
}

/// 盘点一次。`agent_dir` 必须已解析；`home_dir` 缺失时外部根无法归类，会进 warnings。
pub fn inventory(
    agent_dir: &Path,
    home_dir: Option<&Path>,
    launcher: &dyn RpcLauncher,
) -> SkillInventory {
    let before = host_files(agent_dir);
    let outcome = launcher.get_commands(GET_COMMANDS_REQUEST, RPC_TIMEOUT);
    let mut inventory = SkillInventory {
        error: None,
        skills: Vec::new(),
        warnings: STATIC_WARNINGS
            .iter()
            .map(|text| (*text).to_string())
            .collect(),
        created_files: created_files(agent_dir, &before),
    };

    let roots = SkillRoots::new(agent_dir, home_dir);
    if roots.agents_skills.is_none() {
        inventory.warnings.push(
            "无法解析 home 目录，`~/.agents/skills` 下的技能无法识别是否随快照。".to_string(),
        );
    }

    match command_entries(&outcome) {
        Ok(entries) => {
            inventory.warnings.extend(entries.warnings);
            inventory.skills = summarize(&roots, entries.commands);
            let unclassified = inventory
                .skills
                .iter()
                .filter(|skill| !skill.snapshot)
                .count();
            if unclassified > 0 {
                inventory.warnings.push(format!(
                    "有 {unclassified} 个技能的路径不在受管根内（见「不随快照」），导出不会包含它们。"
                ));
            }
        }
        Err(error) => inventory.error = Some(error),
    }

    inventory
}

/// 宿主实际加载的两个技能目录。
struct SkillRoots {
    agent_skills: PathBuf,
    agents_skills: Option<PathBuf>,
}

impl SkillRoots {
    fn new(agent_dir: &Path, home_dir: Option<&Path>) -> Self {
        Self {
            agent_skills: agent_dir.join("skills"),
            agents_skills: home_dir.map(scope::agents_skills_path),
        }
    }

    /// 技能路径落在哪个受管根里。宿主的 `sourceInfo.path` 是绝对路径，按前缀比。
    fn root_of(&self, path: Option<&str>) -> Option<RootId> {
        let path = Path::new(path?);
        RootId::ALL
            .into_iter()
            .find(|root| self.dir_of(*root).is_some_and(|dir| path_under(path, dir)))
    }

    fn dir_of(&self, root: RootId) -> Option<&Path> {
        match root {
            RootId::Agent => Some(&self.agent_skills),
            RootId::AgentsSkills => self.agents_skills.as_deref(),
        }
    }
}

/// 路径是否在受管技能目录之下。宿主的写法可能是链接路径（受管根本身是链接），
/// 也可能只在大小写上不同（Windows），所以先比原路径，再比规范化后的真实路径；
/// 规范化失败（路径不存在）就只认原路径，不猜。
/// 含 `..` 的路径直接不认：宿主不该上报这种路径，规范化又会按目标位置放行，
/// 与快照落地的拒绝规则保持一致。
fn path_under(path: &Path, dir: &Path) -> bool {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return false;
    }
    if platform::path_starts_with(path, dir) {
        return true;
    }
    match (fs::canonicalize(path), fs::canonicalize(dir)) {
        (Ok(real_path), Ok(real_dir)) => platform::path_starts_with(&real_path, &real_dir),
        _ => false,
    }
}

/// 归类并排序：先随快照（`agent`、`agents-skills`），再包提供，最后不随快照的其它来源。
fn summarize(roots: &SkillRoots, commands: Vec<RawCommand>) -> Vec<SkillEntry> {
    let mut seen = HashSet::new();
    let mut skills: Vec<SkillEntry> = commands
        .iter()
        .filter(|command| command.source.as_deref() == Some("skill"))
        .map(|command| entry(roots, command))
        .filter(|entry| seen.insert((entry.name.clone(), entry.path.clone())))
        .collect();
    skills.sort_by_key(|skill| {
        let rank = match skill.source {
            SkillSource::User => 0,
            SkillSource::Package => 1,
            SkillSource::Project => 2,
            SkillSource::Other => 3,
        };
        (
            rank,
            skill.root.map(RootId::as_str).unwrap_or_default(),
            skill.name.clone(),
        )
    });
    skills
}

fn entry(roots: &SkillRoots, command: &RawCommand) -> SkillEntry {
    // 宿主把技能命令命名为 `skill:<name>`；展示用原名，别把内部前缀带给用户。
    let name = command
        .name
        .strip_prefix("skill:")
        .unwrap_or(&command.name)
        .to_string();

    if let Some(root) = roots.root_of(command.path.as_deref()) {
        return SkillEntry {
            name,
            description: command.description.clone(),
            path: command.path.clone(),
            root: Some(root),
            snapshot: true,
            source: SkillSource::User,
            reason: None,
        };
    }

    let (source, reason) = match command.origin.as_deref() {
        Some("package") => (
            SkillSource::Package,
            "包提供：随包重装恢复，不进快照".to_string(),
        ),
        _ => match command.scope.as_deref() {
            Some("project") => (
                SkillSource::Project,
                "项目级技能：本应用不纳管项目目录".to_string(),
            ),
            _ if command.path.is_none() => (
                SkillSource::Other,
                "宿主没有给出技能路径，无法判断是否随快照".to_string(),
            ),
            scope => (
                SkillSource::Other,
                format!(
                    "路径不在受管技能目录内（scope={}），不随快照",
                    scope.unwrap_or("未知")
                ),
            ),
        },
    };

    SkillEntry {
        name,
        description: command.description.clone(),
        path: command.path.clone(),
        root: None,
        snapshot: false,
        source,
        reason: Some(reason),
    }
}

/// 宿主 `get_commands` 响应里的一个命令条目，只留分类要用的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawCommand {
    name: String,
    description: Option<String>,
    source: Option<String>,
    path: Option<String>,
    scope: Option<String>,
    origin: Option<String>,
}

/// 逐行解析宿主输出：非 JSON 行是 shell 噪音与日志，忽略；
/// 一个 `get_commands` 响应都没有才算失败，不猜列表。
fn parse_commands(stdout: &str) -> Result<Vec<RawCommand>, String> {
    let mut responses = 0;
    let mut commands = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|item| item.as_str()) != Some("response")
            || value.get("command").and_then(|item| item.as_str()) != Some("get_commands")
        {
            continue;
        }
        responses += 1;

        if value.get("success").and_then(|item| item.as_bool()) == Some(false) {
            let detail = value
                .get("error")
                .and_then(|item| item.as_str())
                .unwrap_or("宿主没有给出原因");
            return Err(format!(
                "宿主拒绝了 get_commands：{}",
                doctor::redact(detail)
            ));
        }

        // 宿主把命令挂在 `data.commands` 下；没有这个列表就是无法读懂，不能当成「没有技能」。
        let items = value
            .get("data")
            .and_then(|data| data.get("commands"))
            .and_then(|items| items.as_array());
        let Some(items) = items else {
            return Err("get_commands 响应里没有 commands 列表".to_string());
        };
        commands.extend(items.iter().filter_map(raw_command));
    }

    if responses == 0 {
        return Err("宿主没有返回 get_commands 响应".to_string());
    }
    Ok(commands)
}

fn raw_command(value: &serde_json::Value) -> Option<RawCommand> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(|item| item.as_str())
            .map(str::to_string)
    };
    let info = value.get("sourceInfo");
    let info_text = |key: &str| {
        info.and_then(|info| info.get(key))
            .and_then(|item| item.as_str())
            .map(str::to_string)
    };

    Some(RawCommand {
        name: text("name")?,
        description: text("description"),
        source: text("source"),
        path: info_text("path"),
        scope: info_text("scope"),
        origin: info_text("origin"),
    })
}

/// 一次盘点读到的命令条目，以及读的过程中该转给 UI 的告警。
struct ParsedEntries {
    commands: Vec<RawCommand>,
    warnings: Vec<String>,
}

/// 把执行结果折成命令条目：宿主可能答完再非零退出，所以先看 stdout。
fn command_entries(outcome: &CommandOutcome) -> Result<ParsedEntries, String> {
    match outcome {
        CommandOutcome::Timeout => Err(format!(
            "宿主 RPC 超时（{} 秒），已按进程树终止，未重试",
            RPC_TIMEOUT.as_secs()
        )),
        CommandOutcome::Unavailable(detail) => Err(format!("宿主 RPC 无法启动（{detail}）")),
        CommandOutcome::Ok { stdout, .. } => parse_commands(stdout).map(|commands| ParsedEntries {
            commands,
            warnings: Vec::new(),
        }),
        CommandOutcome::Failed {
            code,
            stdout,
            stderr,
        } => {
            if stderr.contains(POSIX_MISSING_MARKER) {
                return Err("tool_not_found：找不到 pi 命令，无法读取技能清单".to_string());
            }
            parse_commands(stdout)
                .map(|commands| ParsedEntries {
                    commands,
                    // 清单读到了但宿主是异常退出的：能展示，但不能当它完整。
                    warnings: vec![exit_warning(*code)],
                })
                .map_err(|error| {
                    let mut message = error;
                    if let Some(code) = code {
                        message.push_str(&format!("（宿主退出码 {code}）"));
                    }
                    let detail = doctor::first_line(stderr);
                    if !detail.is_empty() {
                        message.push_str(&format!("：{}", doctor::redact(&detail)));
                    }
                    message
                })
        }
    }
}

/// 非零退出的告警文本。退出码缺失也照样告警：异常退出本身就是要展示的事实。
fn exit_warning(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("宿主退出码 {code}，清单可能不完整"),
        None => "宿主没有正常退出（无退出码），清单可能不完整".to_string(),
    }
}

fn host_files(agent_dir: &Path) -> Vec<bool> {
    HOST_SIDE_EFFECT_FILES
        .iter()
        .map(|name| agent_dir.join(name).exists())
        .collect()
}

/// 本次 RPC 新出现的文件：已存在的不算；宿主创建的空文件也不删除。
fn created_files(agent_dir: &Path, before: &[bool]) -> Vec<String> {
    HOST_SIDE_EFFECT_FILES
        .iter()
        .zip(before)
        .filter(|(name, existed)| !**existed && agent_dir.join(name).exists())
        .map(|(name, _)| (*name).to_string())
        .collect()
}

/// command 层入口：解析出生效宿主目录后盘点。会启动宿主进程，放阻塞线程池。
#[tauri::command]
pub async fn skills_inventory(app: tauri::AppHandle) -> Result<SkillInventory, String> {
    tauri::async_runtime::spawn_blocking(move || probe(&app))
        .await
        .map_err(|error| format!("技能盘点失败：{error}"))?
}

fn probe(app: &tauri::AppHandle) -> Result<SkillInventory, String> {
    use tauri::Manager;

    // 用户手动动作：与上传（含其内部导出）/下载/应用/回滚、自动上行串行，
    // 免得宿主刚创建的空 auth.json 被同时进行的那次导出带进快照。本地导出不取锁，
    // 这个窗口本身记在 backlog，不在这里拦。
    let _guard = crate::runtime::lock_operation();

    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("无法定位应用配置目录：{error}"))?;
    let config = config::load_config(&config_dir).map_err(|error| error.to_string())?;
    // 只读取这一个文档化的变量；错误信息里不回显其取值。
    let env_agent_dir = std::env::var_os(AGENT_DIR_ENV_VAR).map(PathBuf::from);
    let home_dir = app.path().home_dir().ok();
    let resolved =
        config::resolve_agent_dir(&config, env_agent_dir.as_deref(), home_dir.as_deref());

    let agent_dir = resolved.path.ok_or_else(|| {
        format!("无法确定宿主目录：请填写宿主目录，或设置环境变量 {AGENT_DIR_ENV_VAR}")
    })?;

    // 宿主的工作目录：应用配置目录本身，既不是用户项目，也不会随项目配置变化。
    // 用户还没保存过配置时这个目录可能不存在，先建出来，免得起进程就直接失败。
    fs::create_dir_all(&config_dir)
        .map_err(|error| format!("无法准备宿主工作目录（应用配置目录）：{error}"))?;
    let launcher = HostLauncher::from_env(Some(agent_dir.clone()), config_dir);
    Ok(inventory(&agent_dir, home_dir.as_deref(), &launcher))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;

    /// 临时目录夹具：不引入 tempfile，退出时自行清理。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-skills-{}-{}-{}",
                std::process::id(),
                label,
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("创建测试临时目录");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn make_dir(&self, relative: &str) -> PathBuf {
            let path = self.path.join(relative);
            fs::create_dir_all(&path).expect("建目录");
            path
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.path.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("建父目录");
            }
            fs::write(&path, contents).expect("写文件");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// 假宿主 RPC：按预置结果回答，并可模拟宿主创建文件。
    struct FakeLauncher {
        outcome: CommandOutcome,
        creates: Vec<String>,
        agent_dir: PathBuf,
        requests: RefCell<Vec<(String, Duration)>>,
    }

    impl FakeLauncher {
        fn new(agent_dir: &Path, outcome: CommandOutcome) -> Self {
            Self {
                outcome,
                creates: Vec::new(),
                agent_dir: agent_dir.to_path_buf(),
                requests: RefCell::new(Vec::new()),
            }
        }

        fn creating(mut self, names: &[&str]) -> Self {
            self.creates = names.iter().map(|name| (*name).to_string()).collect();
            self
        }

        fn requests(&self) -> Vec<(String, Duration)> {
            self.requests.borrow().clone()
        }
    }

    impl RpcLauncher for FakeLauncher {
        fn get_commands(&self, request: &str, timeout: Duration) -> CommandOutcome {
            self.requests
                .borrow_mut()
                .push((request.to_string(), timeout));
            for name in &self.creates {
                let path = self.agent_dir.join(name);
                // 宿主只在缺失时创建，已存在的内容不动。
                if !path.exists() {
                    fs::write(&path, "{}").expect("模拟宿主创建文件");
                }
            }
            self.outcome.clone()
        }
    }

    fn ok(stdout: &str) -> CommandOutcome {
        CommandOutcome::Ok {
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    /// 假探测执行器：记录跑过的参数，按预置结果回答。Windows 上解析 npm 全局前缀用它。
    struct FakeRunner {
        outcomes: RefCell<Vec<CommandOutcome>>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeRunner {
        fn new(outcomes: Vec<CommandOutcome>) -> Self {
            Self {
                outcomes: RefCell::new(outcomes),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn next(&self) -> CommandOutcome {
            let mut outcomes = self.outcomes.borrow_mut();
            if outcomes.is_empty() {
                CommandOutcome::Timeout
            } else {
                outcomes.remove(0)
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, script: &str, _timeout: Duration, _env: &[(&str, String)]) -> CommandOutcome {
            self.calls.borrow_mut().push(script.to_string());
            self.next()
        }

        fn run_program(
            &self,
            argv: &[String],
            _timeout: Duration,
            _env: &[(&str, String)],
        ) -> CommandOutcome {
            self.calls.borrow_mut().push(argv.join(" "));
            self.next()
        }
    }

    /// 一行 `get_commands` 响应，形状与真实宿主一致。`commands` 是命令条目的 JSON 数组文本。
    fn response(commands: &str) -> String {
        format!(
            "{{\"type\":\"response\",\"command\":\"get_commands\",\"success\":true,\"data\":{{\"commands\":[{commands}]}}}}"
        )
    }

    fn command_json(name: &str, path: &str, scope: &str, origin: &str) -> String {
        // 走 serde_json 拼接：测试里的路径含 Windows 反斜杠，手拼会写出非法转义。
        serde_json::json!({
            "name": name,
            "description": format!("{name} 的说明"),
            "source": "skill",
            "sourceInfo": {
                "path": path,
                "scope": scope,
                "origin": origin,
            },
        })
        .to_string()
    }

    #[test]
    fn parse_commands_ignores_noise_and_broken_lines() {
        let stdout = format!(
            "welcome to bash\nnot json at all\n{opened}\n{line}\n{{\"type\":\"response\",\"command\":\"other\"}}\n",
            opened = "{ this line is broken json",
            line = response(&command_json("skill:review", "/home/u/.pi/agent/skills/review/SKILL.md", "user", "user")),
        );

        let commands = parse_commands(&stdout).expect("应能解析出响应");

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "skill:review");
        assert_eq!(
            commands[0].path.as_deref(),
            Some("/home/u/.pi/agent/skills/review/SKILL.md")
        );
        assert_eq!(commands[0].scope.as_deref(), Some("user"));
    }

    #[test]
    fn parse_commands_requires_a_response() {
        let error = parse_commands("starting pi\nready\n").expect_err("没有响应必须报错");
        assert!(error.contains("get_commands"), "{error}");

        // 只有别的命令的响应，同样不算拿到清单。
        let other = "{\"type\":\"response\",\"command\":\"get_state\"}\n";
        assert!(parse_commands(other).is_err());
    }

    #[test]
    fn response_without_command_list_is_an_error() {
        let stdout = "{\"type\":\"response\",\"command\":\"get_commands\",\"success\":true}\n";

        let error = parse_commands(stdout).expect_err("没有 commands 列表必须报错");

        assert!(error.contains("commands"), "{error}");
    }

    #[test]
    fn rejected_request_reports_host_reason() {
        let stdout = "{\"type\":\"response\",\"command\":\"get_commands\",\"success\":false,\"error\":\"unknown command\"}\n";

        let error = parse_commands(stdout).expect_err("宿主拒绝时必须报错");

        assert!(error.contains("unknown command"), "{error}");
    }

    #[test]
    fn multiple_responses_are_merged() {
        let stdout = format!(
            "{}\n{}\n",
            response(&command_json(
                "skill:a",
                "/home/u/.pi/agent/skills/a/SKILL.md",
                "user",
                "user"
            )),
            response(&command_json(
                "skill:b",
                "/home/u/.agents/skills/b/SKILL.md",
                "user",
                "user"
            )),
        );

        let commands = parse_commands(&stdout).expect("两个响应都要读");

        assert_eq!(
            commands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            vec!["skill:a", "skill:b"]
        );
    }

    #[test]
    fn skills_are_classified_by_path_and_source_info() {
        let temp = TempDir::new("classify");
        let agent = temp.make_dir("agent");
        let home = temp.make_dir("home");
        let agent_skill = agent.join("skills/review/SKILL.md");
        let external_skill = home.join(".agents/skills/council/SKILL.md");

        let stdout = format!(
            "{}\n",
            response(&format!(
                "{},{},{},{},{}",
                command_json(
                    "skill:review",
                    &agent_skill.display().to_string(),
                    "user",
                    "user"
                ),
                command_json(
                    "skill:council",
                    &external_skill.display().to_string(),
                    "user",
                    "user"
                ),
                command_json(
                    "skill:lens",
                    "/home/u/.pi/agent/npm/node_modules/pi-lens/skills/lens/SKILL.md",
                    "user",
                    "package"
                ),
                command_json(
                    "skill:proj",
                    "/repo/.pi/skills/proj/SKILL.md",
                    "project",
                    "user"
                ),
                command_json(
                    "skill:main",
                    "/usr/lib/pi/skills/main/SKILL.md",
                    "cli",
                    "builtin"
                ),
            ))
        );
        let launcher = FakeLauncher::new(&agent, ok(&stdout));

        let inventory = inventory(&agent, Some(&home), &launcher);

        assert_eq!(inventory.error, None);
        assert!(inventory.created_files.is_empty());
        assert_eq!(
            inventory
                .skills
                .iter()
                .map(|skill| (
                    skill.name.as_str(),
                    skill.root,
                    skill.snapshot,
                    skill.source
                ))
                .collect::<Vec<_>>(),
            vec![
                ("review", Some(RootId::Agent), true, SkillSource::User),
                (
                    "council",
                    Some(RootId::AgentsSkills),
                    true,
                    SkillSource::User
                ),
                ("lens", None, false, SkillSource::Package),
                ("proj", None, false, SkillSource::Project),
                ("main", None, false, SkillSource::Other),
            ],
            "按随快照 → 包提供 → 其它排序，name 去掉 skill: 前缀"
        );

        let review = &inventory.skills[0];
        assert_eq!(review.description.as_deref(), Some("skill:review 的说明"));
        assert_eq!(review.reason, None);
        assert!(inventory.skills[..2]
            .iter()
            .all(|skill| skill.snapshot && skill.reason.is_none()));
        assert!(inventory.skills[2..]
            .iter()
            .all(|skill| !skill.snapshot && skill.reason.is_some()));

        let lens = &inventory.skills[2];
        assert!(lens
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("包提供"));
        let project = &inventory.skills[3];
        assert!(project
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("项目级"));
        let cli = &inventory.skills[4];
        assert!(cli
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("scope=cli"));

        // 非技能命令（source != skill）不进清单。
        assert!(inventory
            .warnings
            .iter()
            .any(|warning| warning.contains("--no-extensions")));
        assert!(inventory
            .warnings
            .iter()
            .any(|warning| warning.contains("3 个技能")));
    }

    #[test]
    fn non_skill_commands_are_filtered_out() {
        let temp = TempDir::new("filter");
        let agent = temp.make_dir("agent");
        let commands = format!(
            "{{\"name\":\"help\",\"source\":\"builtin\"}},{}",
            command_json(
                "skill:keep",
                &agent.join("skills/keep/SKILL.md").display().to_string(),
                "user",
                "user"
            )
        );
        let launcher = FakeLauncher::new(&agent, ok(&response(&commands)));

        let inventory = inventory(&agent, None, &launcher);

        assert_eq!(inventory.skills.len(), 1);
        assert_eq!(inventory.skills[0].name, "keep");
    }

    #[test]
    fn created_files_reports_only_what_the_rpc_created() {
        let temp = TempDir::new("created");
        let agent = temp.make_dir("agent");
        temp.write("agent/auth.json", "{\"existing\":true}");
        let launcher = FakeLauncher::new(&agent, ok(&response("")))
            .creating(&["auth.json", "models-store.json"]);

        let inventory = inventory(&agent, None, &launcher);

        // 已存在的 auth.json 内容不变，也不算本次创建。
        assert_eq!(
            inventory.created_files,
            vec!["models-store.json".to_string()]
        );
        assert_eq!(
            fs::read_to_string(agent.join("auth.json")).unwrap(),
            "{\"existing\":true}"
        );
        assert!(
            agent.join("models-store.json").exists(),
            "宿主创建的文件不删"
        );
    }

    #[test]
    fn request_is_one_get_commands_line_with_rpc_timeout() {
        let temp = TempDir::new("request");
        let agent = temp.make_dir("agent");
        let launcher = FakeLauncher::new(&agent, ok(&response("")));

        inventory(&agent, None, &launcher);

        let requests = launcher.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "{\"type\":\"get_commands\"}\n");
        assert_eq!(requests[0].1, RPC_TIMEOUT);
    }

    #[test]
    fn timeout_and_missing_host_are_readable_errors() {
        let temp = TempDir::new("errors");
        let agent = temp.make_dir("agent");

        let timeout = inventory(
            &agent,
            None,
            &FakeLauncher::new(&agent, CommandOutcome::Timeout),
        );
        let message = timeout.error.clone().expect("超时要报错");
        assert!(message.contains("超时"), "{message}");
        assert!(message.contains("15 秒"), "{message}");
        assert!(timeout.skills.is_empty());

        let missing = inventory(
            &agent,
            None,
            &FakeLauncher::new(
                &agent,
                CommandOutcome::Failed {
                    code: Some(127),
                    stdout: String::new(),
                    stderr: format!("{POSIX_MISSING_MARKER}\n"),
                },
            ),
        );
        assert!(missing
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("tool_not_found"));

        let broken = inventory(
            &agent,
            None,
            &FakeLauncher::new(
                &agent,
                CommandOutcome::Failed {
                    code: Some(1),
                    stdout: String::new(),
                    stderr: "boom: cannot start".to_string(),
                },
            ),
        );
        let message = broken.error.expect("非零退出要报错");
        assert!(message.contains("宿主退出码 1"), "{message}");
        assert!(message.contains("boom"), "{message}");

        let unavailable = inventory(
            &agent,
            None,
            &FakeLauncher::new(&agent, CommandOutcome::Unavailable("无法启动".to_string())),
        );
        assert!(unavailable
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("无法启动"));
    }

    #[test]
    fn failure_keeps_created_files() {
        let temp = TempDir::new("failure-created");
        let agent = temp.make_dir("agent");
        let launcher = FakeLauncher::new(&agent, CommandOutcome::Timeout).creating(&["auth.json"]);

        let inventory = inventory(&agent, None, &launcher);

        assert!(inventory.error.is_some());
        assert_eq!(inventory.created_files, vec!["auth.json".to_string()]);
    }

    #[test]
    fn missing_home_warns_about_external_root() {
        let temp = TempDir::new("no-home");
        let agent = temp.make_dir("agent");
        let launcher = FakeLauncher::new(
            &agent,
            ok(&response(&command_json(
                "skill:x",
                "/home/u/.agents/skills/x/SKILL.md",
                "user",
                "user",
            ))),
        );

        let inventory = inventory(&agent, None, &launcher);

        assert!(inventory
            .warnings
            .iter()
            .any(|warning| warning.contains("home 目录")));
        // 无法归类的外部路径按不随快照处理，不猜。
        assert_eq!(inventory.skills[0].source, SkillSource::Other);
        assert!(!inventory.skills[0].snapshot);
    }

    #[test]
    fn duplicate_commands_across_responses_are_listed_once() {
        let temp = TempDir::new("duplicates");
        let agent = temp.make_dir("agent");
        let skill = command_json(
            "skill:review",
            &agent.join("skills/review/SKILL.md").display().to_string(),
            "user",
            "user",
        );
        let stdout = format!("{}\n{}\n", response(&skill), response(&skill));
        let launcher = FakeLauncher::new(&agent, ok(&stdout));

        let inventory = inventory(&agent, None, &launcher);

        assert_eq!(inventory.skills.len(), 1);
    }

    #[test]
    fn posix_script_falls_back_to_agent_dir_then_npm_prefix() {
        let script = posix_script();

        // 先用 PATH，再用注入的宿主目录，最后问 npm 的全局前缀。
        assert!(script.contains("command -v pi"), "{script}");
        assert!(
            script.contains(&format!("\"${POSIX_AGENT_DIR_ENV}/bin/pi\"")),
            "{script}"
        );
        assert!(script.contains("npm prefix -g"), "{script}");
        // 入口解析到路径后就该直接 exec 那个路径，不能再让 shell 自己找一遍。
        assert!(
            script.contains("exec \"$p\" --mode rpc --offline"),
            "{script}"
        );
        assert!(script.contains("--no-session"), "{script}");
        assert!(script.contains("--no-extensions"), "{script}");
        assert!(script.contains("--no-approve"), "{script}");
        // 三级都找不到时才自报标记退出。
        assert!(script.contains(POSIX_MISSING_MARKER), "{script}");
        assert!(script.contains("exit 127"), "{script}");
        assert!(!script.contains("exec pi"), "{script}");
    }

    #[test]
    fn posix_command_passes_agent_dir_to_the_host() {
        let temp = TempDir::new("agent-env");
        let agent = temp.make_dir("agent");
        let launcher = HostLauncher {
            host: HostOs::Posix,
            agent_dir: Some(agent.clone()),
            search: ProgramPath::new(Vec::new(), Vec::new()),
            cwd: temp.make_dir("config"),
            runner: ShellRunner::from_env(),
        };

        let command = launcher.command().expect("应能拼出命令");
        let injected = |name: &str| {
            command
                .get_envs()
                .find(|(key, _)| key.to_string_lossy() == name)
                .and_then(|(_, value)| value)
                .map(PathBuf::from)
        };

        assert_eq!(injected(POSIX_AGENT_DIR_ENV), Some(agent.clone()));
        assert_eq!(injected(AGENT_DIR_ENV_VAR), Some(agent));
    }

    #[test]
    fn windows_argv_resolves_pi_entry_and_escapes_arguments() {
        let temp = TempDir::new("windows-argv");
        let bin = temp.make_dir("bin");
        fs::write(bin.join("pi.cmd"), "").expect("写入口文件");
        let search = ProgramPath::new(
            vec![bin.clone()],
            vec![".cmd".to_string(), ".exe".to_string()],
        );

        let argv =
            windows_argv(&search, None, &FakeRunner::new(Vec::new())).expect("应解析到 pi.cmd");

        assert_eq!(
            platform::cmd_line_of(&argv),
            platform::cmd_line(&bin.join("pi.cmd").display().to_string(), &RPC_ARGS)
        );

        let empty = ProgramPath::new(vec![temp.make_dir("empty")], vec![".cmd".to_string()]);
        assert!(windows_argv(&empty, None, &FakeRunner::new(Vec::new()))
            .expect_err("找不到入口要报错")
            .contains("tool_not_found"));
    }

    #[test]
    fn windows_argv_falls_back_to_npm_global_prefix() {
        let temp = TempDir::new("windows-npm-prefix");
        let bin = temp.make_dir("bin");
        fs::write(bin.join("npm.cmd"), "").expect("写 npm 入口");
        let prefix = temp.make_dir("npm-prefix");
        fs::write(prefix.join("pi.cmd"), "").expect("写前缀下的 pi 入口");
        let search = ProgramPath::new(
            vec![bin.clone()],
            vec![".cmd".to_string(), ".exe".to_string()],
        );
        let runner = FakeRunner::new(vec![ok(&format!("{}\n", prefix.display()))]);

        let argv = windows_argv(&search, None, &runner).expect("应兜底到 npm 全局前缀");

        assert_eq!(
            platform::cmd_line_of(&argv),
            platform::cmd_line(&prefix.join("pi.cmd").display().to_string(), &RPC_ARGS)
        );
        // 兜底前先问 npm 自己的全局前缀，与医生、安装页、包对齐同一条路径。
        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].contains("npm.cmd"));
        assert!(calls[0].contains("\"prefix\" \"-g\""), "{}", calls[0]);
    }

    #[test]
    fn rpc_command_runs_in_the_given_neutral_cwd() {
        let temp = TempDir::new("cwd");
        let neutral = temp.make_dir("config");
        let launcher = HostLauncher {
            host: HostOs::Posix,
            agent_dir: None,
            search: ProgramPath::new(Vec::new(), Vec::new()),
            cwd: neutral.clone(),
            runner: ShellRunner::from_env(),
        };

        let command = launcher.command().expect("应能拼出命令");

        assert_eq!(
            command.get_current_dir(),
            Some(neutral.as_path()),
            "宿主必须在给定的中性目录里跑，否则清单会混进项目级技能"
        );
    }

    #[cfg(unix)]
    #[test]
    fn skills_under_a_linked_root_still_count_as_managed() {
        let temp = TempDir::new("linked-root");
        let agent = temp.make_dir("agent");
        let real = temp.make_dir("real-skills");
        fs::create_dir_all(real.join("review")).expect("建技能目录");
        std::os::unix::fs::symlink(&real, agent.join("skills")).expect("建根链接");
        let linked = agent.join("skills/review/SKILL.md");
        let resolved = real.join("review/SKILL.md");
        fs::write(&resolved, "---\nname: review\n---\n").expect("写技能文件");
        let stdout = format!(
            "{}\n",
            response(&format!(
                "{},{}",
                command_json(
                    "skill:via-link",
                    &linked.display().to_string(),
                    "user",
                    "user"
                ),
                command_json(
                    "skill:via-real",
                    &resolved.display().to_string(),
                    "user",
                    "user"
                ),
            ))
        );
        let launcher = FakeLauncher::new(&agent, ok(&stdout));

        let inventory = inventory(&agent, None, &launcher);

        assert_eq!(
            inventory
                .skills
                .iter()
                .map(|skill| (skill.name.as_str(), skill.root, skill.snapshot))
                .collect::<Vec<_>>(),
            vec![
                ("via-link", Some(RootId::Agent), true),
                ("via-real", Some(RootId::Agent), true),
            ],
            "链接根与其真实路径都要归到同一个受管根"
        );
    }

    #[test]
    fn host_exiting_non_zero_after_a_valid_response_warns() {
        let temp = TempDir::new("non-zero");
        let agent = temp.make_dir("agent");
        let stdout = response(&command_json(
            "skill:review",
            &agent.join("skills/review/SKILL.md").display().to_string(),
            "user",
            "user",
        ));
        let launcher = FakeLauncher::new(
            &agent,
            CommandOutcome::Failed {
                code: Some(3),
                stdout,
                stderr: "shutdown failed".to_string(),
            },
        );

        let inventory = inventory(&agent, None, &launcher);

        assert_eq!(inventory.error, None, "清单读到了就不算失败");
        assert_eq!(inventory.skills.len(), 1);
        assert!(
            inventory
                .warnings
                .iter()
                .any(|warning| warning.contains("宿主退出码 3") && warning.contains("可能不完整")),
            "{:?}",
            inventory.warnings
        );
    }

    /// 真机冒烟：默认不跑。会启动本机的 `pi`，但只读一个临时宿主目录：
    /// 宿主按注入的 `PI_CODING_AGENT_DIR` 在那里找技能，缺失时创建的空 `auth.json`
    /// 也落在临时目录里，跟临时目录一起清理。
    #[test]
    #[ignore = "需要真实 pi：会启动本机宿主（只读临时宿主目录）"]
    fn smoke_real_host_lists_a_skill_in_a_temp_agent_dir() {
        let temp = TempDir::new("smoke");
        let agent = temp.make_dir("agent");
        temp.write("agent/settings.json", "{}");
        temp.write(
            "agent/skills/demo/SKILL.md",
            "---\nname: demo\ndescription: 冒烟测试技能\n---\n\n# demo\n",
        );
        let launcher = HostLauncher::from_env(Some(agent.clone()), temp.make_dir("cwd"));

        let inventory = inventory(&agent, None, &launcher);

        assert_eq!(inventory.error, None, "{:?}", inventory.error);
        // 清单只收录 `source == "skill"` 的命令，落到受管根就是 User。
        let demo = inventory
            .skills
            .iter()
            .find(|skill| skill.source == SkillSource::User && skill.name.contains("demo"))
            .unwrap_or_else(|| {
                panic!("临时宿主目录里的 demo 技能没列出来：{:?}", inventory.skills)
            });
        assert!(
            demo.path.as_deref().is_some_and(|path| !path.is_empty()),
            "{demo:?}"
        );
        // 只看非空会把「宿主报了别的目录」也算通过：确认路径就在临时宿主目录的技能目录下。
        let path = demo.path.clone().unwrap_or_default();
        let skills_dir = agent.join("skills");
        assert!(
            Path::new(&path).starts_with(&skills_dir),
            "demo 技能应在 {} 下：{path}",
            skills_dir.display()
        );
    }

    #[test]
    fn paths_with_parent_segments_are_not_managed() {
        let temp = TempDir::new("parent-segments");
        let agent = temp.make_dir("agent");
        let skills = agent.join("skills");
        let roots = SkillRoots::new(&agent, None);
        let path_of = |relative: &str| skills.join(relative).display().to_string();

        // 宿主上报的路径本来就不该带 `..`；带上就无法按前缀判定真实位置，直接不认。
        assert_eq!(
            roots.root_of(Some(path_of("review/../demo/SKILL.md").as_str())),
            None
        );
        assert_eq!(
            roots.root_of(Some(path_of("../outside/SKILL.md").as_str())),
            None
        );
        // 同一个位置不带 `..` 时仍归到受管根。
        assert_eq!(
            roots.root_of(Some(path_of("demo/SKILL.md").as_str())),
            Some(RootId::Agent)
        );
    }

    #[test]
    fn inventory_serializes_with_frontend_field_names() {
        let temp = TempDir::new("serialize");
        let agent = temp.make_dir("agent");
        let launcher = FakeLauncher::new(
            &agent,
            ok(&response(&command_json(
                "skill:review",
                &agent.join("skills/review/SKILL.md").display().to_string(),
                "user",
                "user",
            ))),
        );

        let value = serde_json::to_value(inventory(&agent, Some(temp.path()), &launcher))
            .expect("结果应可序列化");

        let keys: HashSet<&str> = value
            .as_object()
            .expect("结果应为 JSON 对象")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            HashSet::from(["error", "skills", "warnings", "createdFiles"])
        );
        assert_eq!(value["skills"][0]["root"], serde_json::json!("agent"));
        assert_eq!(value["skills"][0]["snapshot"], serde_json::json!(true));
        assert_eq!(value["skills"][0]["source"], serde_json::json!("user"));
        assert_eq!(value["error"], serde_json::Value::Null);
    }
}

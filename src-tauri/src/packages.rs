//! 包对齐：比对 `settings.json` 的 `packages` 与 `pi list` 实装，按用户确认逐个安装缺失项。
//!
//! 规则来源：`docs/design/20-env-doctor.md` 的 `packages` 探测与「执行约束」。
//! 命令由本模块拼接，前端只能提交包名数组与确认位；包名必须在后端重新计算出的缺失清单里。
//! 失败即停：不自动重试、不切换方案、不自动删除多余包。

use crate::doctor::{self, CommandOutcome, CommandRunner, ShellRunner};
use crate::installer::{self, GroupRunner, InstallRunner, InstallStatus};
use crate::platform::{HostOs, ProgramPath};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// `pi list` 里找不到命令时的标记，避免与登录 shell 启动脚本的噪音混淆。
const PI_MISSING_MARKER: &str = "__PI_PACKAGES_MISSING__";
/// 声明段标题；其后的两空格行是来源，四空格行是安装目录。
const USER_HEADER: &str = "User packages:";

/// `settings.json` 里声明的一个包。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredPackage {
    pub source: String,
    pub enabled: bool,
}

/// `pi list` 里的一个实装包、安装目录与读到的版本号。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledPackage {
    pub source: String,
    pub path: String,
    /// 安装目录下 `package.json` 的 `version`；读不到时为 `None`，不影响其余结果。
    pub version: Option<String>,
}

/// 一次比对的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageStatus {
    pub declared: Vec<DeclaredPackage>,
    pub installed: Vec<InstalledPackage>,
    pub missing: Vec<String>,
    pub extra: Vec<String>,
}

/// 单个包的安装结果。`output` 已截断并脱敏。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlignResult {
    pub source: String,
    pub status: InstallStatus,
    pub exit_code: Option<i32>,
    pub output: String,
    pub truncated: bool,
}

/// 一次对齐的结果。`completed` 为 false 表示中途失败、后面的包没执行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlignReport {
    pub results: Vec<AlignResult>,
    pub completed: bool,
}

/// 比对声明与实装。只读：读 `settings.json` 并执行一次 `pi list`。
/// `host` 由调用方给定：生产传 [`HostOs::current`]，单测显式指定。
pub fn status(
    agent_dir: &Path,
    runner: &dyn CommandRunner,
    host: HostOs,
) -> Result<PackageStatus, String> {
    let declared = declared_packages(agent_dir)?;
    let installed = installed_packages(agent_dir, runner, host)?;

    // 只有启用的声明算「应当装」；disabled 的包只展示，不自动装回来。
    let installed_sources: BTreeSet<&str> =
        installed.iter().map(|item| item.source.as_str()).collect();
    let mut seen = BTreeSet::new();
    let missing: Vec<String> = declared
        .iter()
        .filter(|item| item.enabled && !installed_sources.contains(item.source.as_str()))
        .filter(|item| seen.insert(item.source.clone()))
        .map(|item| item.source.clone())
        .collect();

    let declared_sources: BTreeSet<&str> =
        declared.iter().map(|item| item.source.as_str()).collect();
    let extra: Vec<String> = installed
        .iter()
        .filter(|item| !declared_sources.contains(item.source.as_str()))
        .map(|item| item.source.clone())
        .collect();

    Ok(PackageStatus {
        declared,
        installed,
        missing,
        extra,
    })
}

/// `settings.json` 的 `packages`：字符串与 `{source, enabled}` 两种写法都认。
fn declared_packages(agent_dir: &Path) -> Result<Vec<DeclaredPackage>, String> {
    let path = agent_dir.join("settings.json");
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("settings_unreadable：无法读取 settings.json（{error}）"))?;
    let settings: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("settings_unreadable：settings.json 无法解析（{error}）"))?;

    let packages = match settings.get("packages") {
        Some(value) if !value.is_array() => {
            return Err("settings_unreadable：settings.json 的 packages 不是数组".to_string())
        }
        Some(packages) => packages.as_array().cloned().unwrap_or_default(),
        None => Vec::new(),
    };

    Ok(packages
        .iter()
        .filter_map(|item| match item {
            serde_json::Value::String(source) => Some(DeclaredPackage {
                source: source.clone(),
                enabled: true,
            }),
            serde_json::Value::Object(map) => {
                let source = map.get("source")?.as_str()?;
                Some(DeclaredPackage {
                    source: source.to_string(),
                    enabled: map
                        .get("enabled")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(true),
                })
            }
            _ => None,
        })
        .collect())
}

fn pi_list_script() -> String {
    r#"p="$(command -v pi 2>/dev/null)"
if [ -z "$p" ]; then
  echo __PI_PACKAGES_MISSING__
else
  "$p" list 2>&1
fi"#
    .to_string()
}

fn installed_packages(
    agent_dir: &Path,
    runner: &dyn CommandRunner,
    host: HostOs,
) -> Result<Vec<InstalledPackage>, String> {
    let stdout = if host == HostOs::Windows {
        pi_list_windows(agent_dir, runner, &ProgramPath::from_env())?
    } else {
        pi_list_scripted(runner)?
    };

    parse_user_packages(&stdout).map(|mut installed| {
        for item in &mut installed {
            item.version = installed_package_version(agent_dir, &item.path);
        }
        installed
    })
}

/// POSIX：一条登录 shell 脚本，找不到 pi 时打印标记，输出里混着脚本噪音。
fn pi_list_scripted(runner: &dyn CommandRunner) -> Result<String, String> {
    let stdout = match runner.run(&pi_list_script(), doctor::PROBE_TIMEOUT, &[]) {
        CommandOutcome::Ok { stdout, .. } | CommandOutcome::Failed { stdout, .. } => stdout,
        CommandOutcome::Timeout => {
            return Err("probe_timeout：pi list 超时（5 秒），未重试".to_string())
        }
        CommandOutcome::Unavailable(detail) => {
            return Err(format!("tool_not_found：无法启动登录 shell（{detail}）"))
        }
    };

    if stdout.trim() == PI_MISSING_MARKER {
        return Err("tool_not_found：找不到 pi 命令，无法读取实装列表".to_string());
    }

    Ok(stdout)
}

/// Windows：解析到 `pi.cmd` 就直接跑 `pi list`，没有标记可用。
fn pi_list_windows(
    agent_dir: &Path,
    runner: &dyn CommandRunner,
    search: &ProgramPath,
) -> Result<String, String> {
    let Some(program) = doctor::find_pi(search, Some(agent_dir), None) else {
        return Err("tool_not_found：找不到 pi 命令，无法读取实装列表".to_string());
    };

    match doctor::probe_program(runner, &program, &["list"]) {
        CommandOutcome::Ok { stdout, .. } | CommandOutcome::Failed { stdout, .. } => Ok(stdout),
        CommandOutcome::Timeout => Err("probe_timeout：pi list 超时（5 秒），未重试".to_string()),
        CommandOutcome::Unavailable(detail) => {
            Err(format!("tool_broken：无法执行 pi list（{detail}）"))
        }
    }
}

/// 版本号读自安装目录的 `package.json`。
///
/// `pi list` 也会列出 git 源等落在宿主目录之外的包，而受管范围之外的文件不读取内容，
/// 所以先确认安装路径在宿主目录内，否则版本留空。
/// 单个包读不到版本不应让整次状态读取失败，因此这里把错误全部折成 `None`。
fn installed_package_version(agent_dir: &Path, path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let root = fs::canonicalize(agent_dir).ok()?;
    let install_dir = fs::canonicalize(path).ok()?;
    if !install_dir.starts_with(&root) {
        return None;
    }
    let text = fs::read_to_string(install_dir.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&text).ok()?;
    manifest
        .get("version")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// 解析 `pi list` 的 `User packages:` 段：两空格缩进是来源，四空格缩进是安装目录。
pub fn parse_user_packages(list: &str) -> Result<Vec<InstalledPackage>, String> {
    if list.trim().is_empty() {
        return Err("package_list_unparsable：pi list 没有输出".to_string());
    }

    let mut installed: Vec<InstalledPackage> = Vec::new();
    let mut in_user = false;
    let mut seen_header = false;

    for line in list.lines() {
        let line = line.trim_end();
        if !in_user {
            if line.trim() == USER_HEADER {
                in_user = true;
                seen_header = true;
            }
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }

        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            // 下一个段（例如 Project packages:）开始。
            break;
        }
        let text = line.trim_start();
        match indent {
            2 => installed.push(InstalledPackage {
                source: text.to_string(),
                path: String::new(),
                version: None,
            }),
            4 => {
                if let Some(last) = installed.last_mut() {
                    last.path = text.to_string();
                }
            }
            _ => {}
        }
    }

    if !seen_header {
        return Err(format!(
            "package_list_unparsable：pi list 输出里没有「{USER_HEADER}」段"
        ));
    }

    Ok(installed)
}

/// 逐个安装缺失包。未确认或包名不在缺失清单里时一行命令都不执行。
pub fn align(
    agent_dir: &Path,
    sources: &[String],
    confirm: bool,
    probes: &dyn CommandRunner,
    runner: &dyn InstallRunner,
    host: HostOs,
) -> Result<AlignReport, String> {
    if !confirm {
        return Err("not_confirmed：未确认，未执行任何命令".to_string());
    }

    // 后端重新计算，前端传不了缺失清单之外的包。
    let current = status(agent_dir, probes, host)?;
    for source in sources {
        if !current.missing.iter().any(|missing| missing == source) {
            return Err(format!(
                "source_not_missing：{source} 不在缺失清单里，请刷新状态后重试"
            ));
        }
    }

    // 包名先全部按平台转义好：有一条过不了就一行命令都不执行。
    let mut commands = Vec::with_capacity(sources.len());
    for source in sources {
        commands.push((source, installer::install_package_argv(host, source)?));
    }

    let mut results = Vec::new();
    let mut completed = true;

    for (source, argv) in commands {
        let result = align_result(source, runner.run(&argv, installer::EXEC_TIMEOUT));
        if result.status != InstallStatus::Succeeded {
            completed = false;
            results.push(result);
            break;
        }
        results.push(result);
    }

    Ok(AlignReport { results, completed })
}

fn align_result(source: &str, outcome: CommandOutcome) -> AlignResult {
    let (status, exit_code, stdout, stderr) = match outcome {
        CommandOutcome::Ok { stdout, stderr } => {
            (InstallStatus::Succeeded, Some(0), stdout, stderr)
        }
        CommandOutcome::Failed {
            code,
            stdout,
            stderr,
        } => (InstallStatus::Failed, code, stdout, stderr),
        CommandOutcome::Timeout => (InstallStatus::Timeout, None, String::new(), String::new()),
        CommandOutcome::Unavailable(detail) => {
            (InstallStatus::Unavailable, None, String::new(), detail)
        }
    };
    let (output, truncated) = installer::tail_output(&stdout, &stderr);

    AlignResult {
        source: source.to_string(),
        status,
        exit_code,
        output,
        truncated,
    }
}

/// command 层入口：解析出生效宿主目录。
fn resolve_agent_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    installer::resolve_agent_dir(app)?.ok_or_else(|| {
        "agent_dir_unknown：无法确定宿主目录，请在设置里填写或设置环境变量 PI_CODING_AGENT_DIR"
            .to_string()
    })
}

/// 读状态要跑一次 `pi list`，放阻塞线程池。
#[tauri::command]
pub async fn package_status(app: tauri::AppHandle) -> Result<PackageStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let agent_dir = resolve_agent_dir(&app)?;
        status(&agent_dir, &ShellRunner::from_env(), HostOs::current())
    })
    .await
    .map_err(|error| format!("读取包状态失败：{error}"))?
}

/// 确认后执行 `pi install`。每包最长 10 分钟，同样不占 UI 线程。
#[tauri::command]
pub async fn align_packages(
    app: tauri::AppHandle,
    sources: Vec<String>,
    confirm: bool,
) -> Result<AlignReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let agent_dir = resolve_agent_dir(&app)?;
        align(
            &agent_dir,
            &sources,
            confirm,
            &ShellRunner::from_env(),
            &GroupRunner,
            HostOs::current(),
        )
    })
    .await
    .map_err(|error| format!("执行包安装失败：{error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    static NEXT_DIR_ID: AtomicU32 = AtomicU32::new(0);

    /// 临时目录夹具：不引入 tempfile，退出时自行清理。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = NEXT_DIR_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-packages-{}-{}-{}",
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

    /// 假探测执行器：按调用顺序回放 `pi list` 的结果，并记录收到的脚本与 argv。
    struct FakeProbe {
        outcomes: RefCell<Vec<CommandOutcome>>,
        calls: RefCell<Vec<String>>,
        programs: RefCell<Vec<Vec<String>>>,
    }

    impl FakeProbe {
        fn new(outcomes: Vec<CommandOutcome>) -> Self {
            Self {
                outcomes: RefCell::new(outcomes),
                calls: RefCell::new(Vec::new()),
                programs: RefCell::new(Vec::new()),
            }
        }

        /// 只有一次 `pi list` 输出的场景。
        fn list(stdout: &str) -> Self {
            Self::new(vec![ok(stdout)])
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn programs(&self) -> Vec<Vec<String>> {
            self.programs.borrow().clone()
        }
    }

    impl CommandRunner for FakeProbe {
        fn run(&self, script: &str, _timeout: Duration, _env: &[(&str, String)]) -> CommandOutcome {
            self.calls.borrow_mut().push(script.to_string());
            let mut outcomes = self.outcomes.borrow_mut();
            if outcomes.is_empty() {
                CommandOutcome::Timeout
            } else {
                outcomes.remove(0)
            }
        }

        fn run_program(
            &self,
            argv: &[String],
            _timeout: Duration,
            _env: &[(&str, String)],
        ) -> CommandOutcome {
            self.programs.borrow_mut().push(argv.to_vec());
            let mut outcomes = self.outcomes.borrow_mut();
            if outcomes.is_empty() {
                CommandOutcome::Timeout
            } else {
                outcomes.remove(0)
            }
        }
    }

    /// 假安装执行器：记录收到的命令行，回放预置结果。
    struct FakeInstall {
        outcomes: RefCell<Vec<CommandOutcome>>,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl FakeInstall {
        fn new(outcomes: Vec<CommandOutcome>) -> Self {
            Self {
                outcomes: RefCell::new(outcomes),
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

    fn failed(code: i32, stdout: &str, stderr: &str) -> CommandOutcome {
        CommandOutcome::Failed {
            code: Some(code),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    fn user_list(entries: &[(&str, &str)]) -> String {
        let mut list = format!("{USER_HEADER}\n");
        for (source, path) in entries {
            list.push_str(&format!("  {source}\n    {path}\n"));
        }
        list
    }

    fn settings_with(packages: &str) -> String {
        format!(r#"{{"packages":{packages}}}"#)
    }

    #[test]
    fn parse_user_packages_reads_sources_and_paths() {
        let list = format!(
            "{}\nProject packages:\n  npm:project-pkg\n    /tmp/project-pkg",
            user_list(&[
                ("npm:pi-lens", "/home/u/.pi/agent/npm/node_modules/pi-lens"),
                ("git:github.com/u/r", "/home/u/r"),
            ])
        );

        let installed = parse_user_packages(&list).unwrap();

        assert_eq!(
            installed,
            vec![
                InstalledPackage {
                    source: "npm:pi-lens".to_string(),
                    path: "/home/u/.pi/agent/npm/node_modules/pi-lens".to_string(),
                    version: None,
                },
                InstalledPackage {
                    source: "git:github.com/u/r".to_string(),
                    path: "/home/u/r".to_string(),
                    version: None,
                },
            ]
        );
    }

    #[test]
    fn parse_user_packages_handles_empty_section() {
        assert_eq!(
            parse_user_packages("User packages:\n").unwrap(),
            Vec::<InstalledPackage>::new()
        );
    }

    #[test]
    fn parse_user_packages_rejects_unparsable_text() {
        let empty = parse_user_packages("   \n").unwrap_err();
        assert!(empty.contains("package_list_unparsable"), "{empty}");

        let noise = parse_user_packages("error: unknown command 'list'").unwrap_err();
        assert!(noise.contains("User packages:"), "{noise}");
    }

    #[test]
    fn settings_packages_support_string_and_object_forms() {
        let dir = TempDir::new("declared");
        dir.write(
            "settings.json",
            &settings_with(
                r#"["npm:pi-lens",{"source":"npm:pi-web-access","enabled":false},{"source":"npm:pi-docs"}]"#,
            ),
        );

        let status = status(
            dir.path(),
            &FakeProbe::list("User packages:\n"),
            HostOs::Posix,
        )
        .unwrap();

        assert_eq!(
            status.declared,
            vec![
                DeclaredPackage {
                    source: "npm:pi-lens".to_string(),
                    enabled: true,
                },
                DeclaredPackage {
                    source: "npm:pi-web-access".to_string(),
                    enabled: false,
                },
                DeclaredPackage {
                    source: "npm:pi-docs".to_string(),
                    enabled: true,
                },
            ]
        );
        // 停用的包只展示，不自动装回来。
        assert_eq!(status.missing, vec!["npm:pi-lens", "npm:pi-docs"]);
    }

    #[test]
    fn installed_version_comes_from_install_dir_manifest() {
        let dir = TempDir::new("installed-version");
        let manifest_path = dir.write(
            "npm/node_modules/pi-lens/package.json",
            r#"{"name":"pi-lens","version":"1.4.2"}"#,
        );
        let install_dir = manifest_path.parent().unwrap().to_str().unwrap();
        let list = user_list(&[("npm:pi-lens", install_dir)]);

        let installed =
            installed_packages(dir.path(), &FakeProbe::list(&list), HostOs::Posix).unwrap();

        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].version.as_deref(), Some("1.4.2"));
        assert_eq!(installed[0].path, install_dir);
    }

    #[test]
    fn installed_version_without_manifest_is_none() {
        let dir = TempDir::new("installed-no-manifest");
        let install_dir = dir.path().join("pi-lens");
        fs::create_dir_all(&install_dir).unwrap();
        let list = user_list(&[("npm:pi-lens", install_dir.to_str().unwrap())]);

        let installed =
            installed_packages(dir.path(), &FakeProbe::list(&list), HostOs::Posix).unwrap();

        assert_eq!(installed[0].version, None);
    }

    #[test]
    fn installed_version_without_version_field_is_none() {
        let dir = TempDir::new("installed-no-version");
        let manifest_path = dir.write(
            "npm/node_modules/pi-lens/package.json",
            r#"{"name":"pi-lens"}"#,
        );
        let install_dir = manifest_path.parent().unwrap().to_str().unwrap();
        let list = user_list(&[("npm:pi-lens", install_dir)]);

        let installed =
            installed_packages(dir.path(), &FakeProbe::list(&list), HostOs::Posix).unwrap();

        assert_eq!(installed[0].version, None);
    }

    #[test]
    fn installed_version_with_empty_path_is_none() {
        let dir = TempDir::new("installed-empty-path");
        let list = user_list(&[("npm:pi-lens", "")]);

        let installed =
            installed_packages(dir.path(), &FakeProbe::list(&list), HostOs::Posix).unwrap();

        assert_eq!(installed[0].path, "");
        assert_eq!(installed[0].version, None);
    }

    #[test]
    fn installed_version_outside_agent_dir_is_none() {
        let agent = TempDir::new("installed-agent-scope");
        let elsewhere = TempDir::new("installed-elsewhere");
        let manifest_path = elsewhere.write("package.json", r#"{"version":"9.9.9"}"#);
        let install_dir = manifest_path.parent().unwrap().to_str().unwrap();
        let list = user_list(&[("git:elsewhere", install_dir)]);

        let installed =
            installed_packages(agent.path(), &FakeProbe::list(&list), HostOs::Posix).unwrap();

        assert_eq!(installed[0].version, None, "宿主目录之外的包不读内容");
    }

    #[test]
    fn installed_version_with_invalid_manifest_is_none() {
        let dir = TempDir::new("installed-bad-json");
        let manifest_path = dir.write("npm/node_modules/pi-lens/package.json", "{");
        let install_dir = manifest_path.parent().unwrap().to_str().unwrap();
        let list = user_list(&[("npm:pi-lens", install_dir)]);

        let installed =
            installed_packages(dir.path(), &FakeProbe::list(&list), HostOs::Posix).unwrap();

        assert_eq!(installed[0].version, None);
    }

    #[test]
    fn status_reports_missing_and_extra() {
        let dir = TempDir::new("status");
        dir.write(
            "settings.json",
            &settings_with(r#"["npm:pi-lens","npm:pi-docs"]"#),
        );
        let list = user_list(&[
            ("npm:pi-lens", "/home/u/.pi/agent/npm/node_modules/pi-lens"),
            ("npm:pi-old", "/home/u/.pi/agent/npm/node_modules/pi-old"),
        ]);

        let status = status(dir.path(), &FakeProbe::list(&list), HostOs::Posix).unwrap();

        assert_eq!(status.missing, vec!["npm:pi-docs"]);
        assert_eq!(status.extra, vec!["npm:pi-old"]);
        assert_eq!(status.installed.len(), 2);
    }

    #[test]
    fn status_requires_settings_json() {
        let dir = TempDir::new("no-settings");
        let error = status(
            dir.path(),
            &FakeProbe::list("User packages:\n"),
            HostOs::Posix,
        )
        .unwrap_err();
        assert!(error.contains("settings_unreadable"), "{error}");
    }

    #[test]
    fn pi_list_missing_tool_and_timeout_are_readable() {
        let dir = TempDir::new("list-failures");
        dir.write("settings.json", &settings_with(r#"["npm:pi-lens"]"#));

        let missing = status(
            dir.path(),
            &FakeProbe::list(&format!("{PI_MISSING_MARKER}\n")),
            HostOs::Posix,
        )
        .unwrap_err();
        assert!(missing.contains("tool_not_found"), "{missing}");

        let timeout = status(
            dir.path(),
            &FakeProbe::new(vec![CommandOutcome::Timeout]),
            HostOs::Posix,
        )
        .unwrap_err();
        assert!(timeout.contains("probe_timeout"), "{timeout}");
    }

    #[test]
    fn windows_pi_list_runs_directly() {
        let dir = TempDir::new("windows-pi-list");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).expect("建目录");
        // pi 只在 `<agent_dir>/bin` 下：PATH 里没有。
        fs::write(bin.join("pi.cmd"), "").expect("写文件");
        let search = ProgramPath::new(vec![], vec![".cmd".to_string(), ".exe".to_string()]);

        let runner = FakeProbe::new(vec![ok(&user_list(&[("npm:pi-lens", "C:\\npm\\pi-lens")]))]);
        let stdout = pi_list_windows(dir.path(), &runner, &search).unwrap();

        assert!(stdout.contains("npm:pi-lens"));
        assert!(runner.calls().is_empty(), "Windows 上不拼脚本");
        let programs = runner.programs();
        assert_eq!(programs.len(), 1);
        assert_eq!(
            crate::platform::cmd_line_of(&programs[0]),
            crate::platform::cmd_line(&bin.join("pi.cmd").display().to_string(), &["list"])
        );
    }

    #[test]
    fn windows_pi_list_without_pi_is_tool_not_found() {
        let dir = TempDir::new("windows-pi-list-missing");
        let search = ProgramPath::new(vec![], vec![".cmd".to_string()]);

        let error = pi_list_windows(dir.path(), &FakeProbe::new(Vec::new()), &search).unwrap_err();

        assert!(error.contains("tool_not_found"), "{error}");
    }

    #[test]
    fn align_commands_are_quoted_per_platform() {
        let windows = installer::install_package_argv(HostOs::Windows, "npm:pi-lens").unwrap();
        assert_eq!(&windows[..4], ["cmd", "/d", "/s", "/c"]);
        assert_eq!(
            windows[4],
            crate::platform::cmd_line("pi", &["install", "npm:pi-lens"]),
            "整行以外层引号包住，内部逐元素加引号"
        );
        assert_eq!(
            installer::install_package_argv(HostOs::Posix, "npm:o'brien").unwrap(),
            vec![
                std::env::var_os("SHELL")
                    .unwrap_or_else(|| "sh".into())
                    .to_string_lossy()
                    .into_owned(),
                "-lc".to_string(),
                "pi install 'npm:o'\\''brien'".to_string()
            ]
        );
    }

    #[test]
    fn align_rejects_package_names_cmd_would_expand() {
        for source in ["npm:a%b", "npm:a\"b", "npm:a&b", "npm:a|b", "npm:a^b"] {
            let error = installer::install_package_argv(HostOs::Windows, source).unwrap_err();
            assert!(error.contains("拒绝执行"), "{source}: {error}");
        }
    }

    #[test]
    fn align_requires_confirmation() {
        let dir = TempDir::new("align-declined");
        dir.write("settings.json", &settings_with(r#"["npm:pi-lens"]"#));
        let probes = FakeProbe::list("User packages:\n");
        let runner = FakeInstall::new(vec![ok("")]);

        let error = align(
            dir.path(),
            &["npm:pi-lens".to_string()],
            false,
            &probes,
            &runner,
            HostOs::Posix,
        )
        .unwrap_err();

        assert!(error.contains("not_confirmed"), "{error}");
        assert!(runner.calls().is_empty());
        assert!(probes.calls().is_empty());
    }

    #[test]
    fn align_rejects_sources_outside_missing() {
        let dir = TempDir::new("align-not-missing");
        dir.write("settings.json", &settings_with(r#"["npm:pi-lens"]"#));
        let list = user_list(&[("npm:pi-lens", "/tmp/pi-lens")]);
        let probes = FakeProbe::list(&list);
        let runner = FakeInstall::new(vec![ok("")]);

        let error = align(
            dir.path(),
            &["npm:pi-lens".to_string()],
            true,
            &probes,
            &runner,
            HostOs::Posix,
        )
        .unwrap_err();

        assert!(error.contains("source_not_missing"), "{error}");
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn align_commands_only_come_from_declared_sources() {
        let dir = TempDir::new("align-commands");
        dir.write(
            "settings.json",
            &settings_with(
                r#"["npm:pi; rm -rf ~",{"source":"npm:pi $(whoami)"},{"source":"npm:o'brien"}]"#,
            ),
        );
        let probes = FakeProbe::new(vec![
            ok("User packages:\n"),
            ok("User packages:\n"),
            ok("User packages:\n"),
            ok("User packages:\n"),
        ]);
        let runner = FakeInstall::new(vec![ok(""), ok(""), ok("")]);

        let report = align(
            dir.path(),
            &[
                "npm:pi; rm -rf ~".to_string(),
                "npm:pi $(whoami)".to_string(),
                "npm:o'brien".to_string(),
            ],
            true,
            &probes,
            &runner,
            HostOs::Posix,
        )
        .unwrap();

        assert!(report.completed);
        let scripts: Vec<String> = runner
            .calls()
            .into_iter()
            .map(|argv| argv.last().cloned().unwrap_or_default())
            .collect();
        assert_eq!(
            scripts,
            vec![
                "pi install 'npm:pi; rm -rf ~'",
                "pi install 'npm:pi $(whoami)'",
                "pi install 'npm:o'\\''brien'",
            ]
        );
    }

    #[test]
    fn align_stops_after_first_failure() {
        let dir = TempDir::new("align-stop");
        dir.write(
            "settings.json",
            &settings_with(r#"["npm:pi-lens","npm:pi-docs"]"#),
        );
        let probes = FakeProbe::list("User packages:\n");
        let runner = FakeInstall::new(vec![
            failed(1, "", "npm ERR! 404 not found"),
            ok("should not run"),
        ]);

        let report = align(
            dir.path(),
            &["npm:pi-lens".to_string(), "npm:pi-docs".to_string()],
            true,
            &probes,
            &runner,
            HostOs::Posix,
        )
        .unwrap();

        assert!(!report.completed);
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].status, InstallStatus::Failed);
        assert_eq!(report.results[0].source, "npm:pi-lens");
        assert_eq!(report.results[0].exit_code, Some(1));
        assert!(report.results[0].output.contains("404 not found"));
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn align_output_is_truncated_and_redacted() {
        let dir = TempDir::new("align-output");
        dir.write("settings.json", &settings_with(r#"["npm:pi-lens"]"#));
        let probes = FakeProbe::list("User packages:\n");
        let long: String = (0..150)
            .map(|index| format!("line {index} token=abc123\n"))
            .collect();
        let runner = FakeInstall::new(vec![failed(1, &long, "api_key=sk-live-123456")]);

        let report = align(
            dir.path(),
            &["npm:pi-lens".to_string()],
            true,
            &probes,
            &runner,
            HostOs::Posix,
        )
        .unwrap();

        let result = &report.results[0];
        assert!(result.truncated);
        assert!(!result.output.contains("sk-live-123456"));
        assert!(!result.output.contains("token=abc123"));
        assert!(result.output.contains("<redacted>"));
        assert!(result.output.lines().count() <= installer::OUTPUT_TAIL_LINES);
    }

    #[test]
    fn align_timeout_is_reported_not_retried() {
        let dir = TempDir::new("align-timeout");
        dir.write("settings.json", &settings_with(r#"["npm:pi-lens"]"#));
        let probes = FakeProbe::list("User packages:\n");
        let runner = FakeInstall::new(vec![CommandOutcome::Timeout]);

        let report = align(
            dir.path(),
            &["npm:pi-lens".to_string()],
            true,
            &probes,
            &runner,
            HostOs::Posix,
        )
        .unwrap();

        assert!(!report.completed);
        assert_eq!(report.results[0].status, InstallStatus::Timeout);
        assert_eq!(report.results[0].exit_code, None);
        assert!(report.results[0].output.is_empty());
        assert_eq!(runner.calls().len(), 1);
    }
}

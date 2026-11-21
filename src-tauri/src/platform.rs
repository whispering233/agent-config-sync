//! 平台差异：文件原语的权限位与链接语义，宿主命令的执行形态（登录 shell / `cmd /C`）。
//!
//! 只放原语：业务判断（哪些路径受管、失败码怎么报）留在调用方，
//! 平台差异用编译期分段或按 [`HostOs`] 分段的并列实现，不留空实现占位。

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 读权限位的低 9 位。Windows 没有 POSIX 权限位，返回 `None`：
/// 备份与还原清单因此记 `null`，还原时跳过权限还原。
#[cfg(unix)]
pub(crate) fn mode_of(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
pub(crate) fn mode_of(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

/// 设置权限位。Windows 上没有对应语义，不设权限：宿主文件与凭据的保护由
/// 用户配置目录的默认 ACL 承担，硬改属性反而会破坏用户设置。
#[cfg(unix)]
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub(crate) fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// 递归建目录（已存在不报错），Unix 下创建时即带 `mode`，不经过 umask。
#[cfg(unix)]
pub(crate) fn create_dir_all_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)
}

#[cfg(not(unix))]
pub(crate) fn create_dir_all_mode(path: &Path, _mode: u32) -> io::Result<()> {
    fs::create_dir_all(path)
}

/// 「链接类」判定：链接自身的 `is_symlink` 与 `read_link` 的结果二选一为真即可。
/// Windows 上 junction 未必被 `FileType::is_symlink()` 认出来，但读得到目标；
/// 普通文件与目录读不到目标，所以不会误判。抽成纯函数是因为 junction 在 Windows 之外
/// 造不出来，只有这样才能覆盖这条分支。
pub(crate) fn link_like(is_symlink: bool, target: &io::Result<PathBuf>) -> bool {
    is_symlink || target.is_ok()
}

/// 这条路径是不是「链接类」。
pub(crate) fn is_link_like(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) => link_like(metadata.file_type().is_symlink(), &fs::read_link(path)),
        Err(_) => false,
    }
}

/// 入口是否落在 `node_modules` 下。Windows 的路径用 `\` 且大小写不敏感，
/// 所以按分隔符切段再逐段比，而不是做子串匹配。
pub(crate) fn is_npm_install_path(entry: &str) -> bool {
    entry
        .split(['/', '\\'])
        .any(|segment| segment.eq_ignore_ascii_case("node_modules"))
}

/// 删掉链接本身，不跟进去。Unix 上任何链接都走 `remove_file`；
/// Windows 上目录型链接（junction、目录符号链接）只能按目录删，否则报拒绝访问。
#[cfg(not(windows))]
pub(crate) fn remove_link(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

#[cfg(windows)]
pub(crate) fn remove_link(path: &Path) -> io::Result<()> {
    // 跟得过去就看目标类型；断链跟不过去，只能看链接自身的属性。
    let directory_link = match fs::metadata(path) {
        Ok(target) => target.is_dir(),
        Err(_) => fs::symlink_metadata(path).is_ok_and(|link| link.is_dir()),
    };
    if directory_link {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// 建一条链接。Windows 必须按目标类型分开调，`is_dir` 由调用方按目标判定；
/// 默认权限下建链接会失败（`ERROR_PRIVILEGE_NOT_HELD`），由调用方退回物化写入。
#[cfg(unix)]
pub(crate) fn create_link(target: &str, path: &Path, _is_dir: bool) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(windows)]
pub(crate) fn create_link(target: &str, path: &Path, is_dir: bool) -> io::Result<()> {
    if is_dir {
        std::os::windows::fs::symlink_dir(target, path)
    } else {
        std::os::windows::fs::symlink_file(target, path)
    }
}

/// 宿主平台。命令形态按它分支：生产用 [`HostOs::current`]，单测显式指定，
/// 这样 Windows 的命令行形态也能在任意平台上断言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostOs {
    Posix,
    Windows,
}

impl HostOs {
    pub(crate) fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }
}

/// cmd 会展开或换义的字符：`%` / `!` 是变量引用，其余是命令分隔、管道、重定向与转义符；
/// `\r` / `\n` 会把一条命令行拆成两条。
const CMD_UNSAFE_CHARS: [char; 10] = ['"', '%', '!', '&', '|', '<', '>', '^', '\r', '\n'];

/// 单引号包裹并转义，避免参数里的字符逃出引号。
pub(crate) fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 按平台给一个来自配置的参数加引号。POSIX 用单引号；Windows 用双引号，并拒绝 cmd 会展开
/// 或换义的字符——参数来自用户配置（provider id、包名），不能原样拼进命令行。
pub(crate) fn quote_arg(os: HostOs, value: &str) -> Result<String, String> {
    match os {
        HostOs::Posix => Ok(sh_quote(value)),
        HostOs::Windows => {
            if let Some(bad) = value.chars().find(|c| CMD_UNSAFE_CHARS.contains(c)) {
                return Err(format!(
                    "参数 {value:?} 含 cmd 会展开或换义的字符 {bad:?}，已拒绝执行"
                ));
            }
            Ok(format!("\"{value}\""))
        }
    }
}

/// 程序路径只加引号，不过拒绝名单：路径由本机文件系统解析得出，不是配置里的自由文本，
/// `C:\Users\a&b\…` 这类目录名在双引号内不会构成命令分隔；套同一份名单会让整个功能拒绝执行。
pub(crate) fn quote_program(os: HostOs, program: &str) -> String {
    match os {
        HostOs::Posix => program.to_string(),
        HostOs::Windows => format!("\"{program}\""),
    }
}

/// 拼一次「逐命令」调用。POSIX 直接给程序与参数；Windows 上 `npm` / `pi` 是 `.cmd` 中间层，
/// 只能经 `cmd /d /s /c` 启动：程序路径与参数各自加引号后拼成整行，整行再套一层引号，
/// 由 [`build_command`] 用 `raw_arg` 原样交给 cmd。
///
/// 外层这对引号不能省：`/c` 之后引号数 ≥ 3 时 cmd 会剥掉首个引号再删掉最后一个，
/// `cmd /c "C:\node\pi.cmd" "list"` 于是变成 `C:\node\pi.cmd" "list`；`/s` 只去掉最外层
/// 一对引号，内部原样交给程序——这是 `cmd.exe` 的标准形状。
pub(crate) fn program_argv(
    os: HostOs,
    program: &str,
    args: &[&str],
) -> Result<Vec<String>, String> {
    match os {
        HostOs::Posix => {
            let mut argv = Vec::with_capacity(args.len() + 1);
            argv.push(program.to_string());
            argv.extend(args.iter().map(|arg| arg.to_string()));
            Ok(argv)
        }
        HostOs::Windows => {
            let mut parts = Vec::with_capacity(args.len() + 1);
            parts.push(quote_program(os, program));
            for arg in args {
                parts.push(quote_arg(os, arg)?);
            }
            Ok(vec![
                "cmd".to_string(),
                "/d".to_string(),
                "/s".to_string(),
                "/c".to_string(),
                format!("\"{}\"", parts.join(" ")),
            ])
        }
    }
}

/// 把 argv 变成可启动的 `Command`。`cmd /d /s /c <整行>` 必须走 `raw_arg`：
/// 若先交给 `Command` 的常规参数序列化，Rust 会再补一层引号转义，cmd 收到的
/// 就不再是准备好的那条命令行。
pub(crate) fn build_command(argv: &[String]) -> Result<Command, String> {
    let Some((program, args)) = argv.split_first() else {
        return Err("命令为空".to_string());
    };

    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if is_cmd_line(program, args) {
            command.args(&args[..3]).raw_arg(args[3].clone());
            return Ok(command);
        }
    }
    command.args(args);
    Ok(command)
}

/// 是不是 [`program_argv`] 拼出来的 `cmd /d /s /c <整行>`。
#[cfg(windows)]
fn is_cmd_line(program: &str, args: &[String]) -> bool {
    const FLAGS: [&str; 3] = ["/d", "/s", "/c"];
    program.eq_ignore_ascii_case("cmd")
        && args.len() == 4
        && args[..3]
            .iter()
            .zip(FLAGS)
            .all(|(arg, flag)| arg.eq_ignore_ascii_case(flag))
}

/// 单测拼期望值：`cmd /d /s /c` 整行 = 外层一对引号包住内部逐个加引号的元素。
#[cfg(test)]
pub(crate) fn cmd_line(program: &str, args: &[&str]) -> String {
    let mut parts = vec![format!("\"{program}\"")];
    parts.extend(args.iter().map(|arg| format!("\"{arg}\"")));
    format!("\"{}\"", parts.join(" "))
}

/// 单测校验一条 argv 是 `cmd /d /s /c` 形态，并取出整行原文。
#[cfg(test)]
pub(crate) fn cmd_line_of(argv: &[String]) -> String {
    assert_eq!(argv[0], "cmd", "Windows 上程序名必须是 cmd");
    assert_eq!(
        argv[1..4],
        ["/d", "/s", "/c"],
        "Windows 上必须是 cmd /d /s /c"
    );
    let line = argv[4].clone();
    assert!(
        line.starts_with('"') && line.ends_with('"'),
        "整行必须由外层引号包住：{line}"
    );
    line
}

/// 安装命令在 POSIX 下独立成进程组，超时按组终止，避免 `curl | sh` 的孙进程残留。
/// Windows 不需要这一步：超时按父子关系用 `taskkill /T` 杀整棵树。
#[cfg(unix)]
pub(crate) fn isolate_group(command: &mut Command, group: bool) {
    if group {
        std::os::unix::process::CommandExt::process_group(command, 0);
    }
}

#[cfg(not(unix))]
pub(crate) fn isolate_group(_command: &mut Command, _group: bool) {}

/// 杀整个进程组，`pid` 是组长（由 [`isolate_group`] 建出来的组）。
/// Windows 没有进程组，用 [`kill_tree`]。
#[cfg(unix)]
pub(crate) fn kill_group(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg("--")
        .arg(format!("-{pid}"))
        .status();
}

#[cfg(not(unix))]
pub(crate) fn kill_group(_pid: u32) {}

/// 按父子关系杀整棵进程树。Windows 上 `child.kill()` 只杀 `cmd.exe`，npm 派生的
/// 子进程会留在后台，所以超时走 `taskkill /T /F`。
#[cfg(windows)]
pub(crate) fn kill_tree(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(windows))]
pub(crate) fn kill_tree(_pid: u32) {}

/// PATHEXT 缺失时的默认后缀表，顺序与 Windows 默认值一致。
const DEFAULT_PATHEXT: [&str; 4] = [".com", ".exe", ".bat", ".cmd"];

/// 按 PATH 与 PATHEXT 找可执行文件。目录表与后缀表抽成值，单测才能喂替身路径：
/// 真实候选只在装了 Node 与宿主的机器上才凑得齐。
pub(crate) struct ProgramPath {
    dirs: Vec<PathBuf>,
    extensions: Vec<String>,
}

impl ProgramPath {
    pub(crate) fn new(dirs: Vec<PathBuf>, extensions: Vec<String>) -> Self {
        Self { dirs, extensions }
    }

    pub(crate) fn from_env() -> Self {
        Self::new(
            path_dirs(std::env::var_os("PATH")),
            extensions_from(std::env::var_os("PATHEXT")),
        )
    }

    /// 按 PATH 顺序找，目录内按后缀顺序找；找不到就是没装，不猜。
    pub(crate) fn find(&self, name: &str) -> Option<PathBuf> {
        self.dirs.iter().find_map(|dir| self.find_in(dir, name))
    }

    /// 只在指定目录里找：`<agent_dir>/bin` 与 npm 全局前缀下的兜底入口用这个。
    pub(crate) fn find_in(&self, dir: &Path, name: &str) -> Option<PathBuf> {
        self.extensions.iter().find_map(|extension| {
            let candidate = dir.join(format!("{name}{extension}"));
            candidate.is_file().then_some(candidate)
        })
    }
}

/// PATH 条目偶尔被引号包着（`"C:\Program Files\nodejs"`），解析时去掉。
fn path_dirs(value: Option<OsString>) -> Vec<PathBuf> {
    let Some(value) = value else {
        return Vec::new();
    };
    std::env::split_paths(&value)
        .map(|dir| PathBuf::from(dir.to_string_lossy().trim_matches('"')))
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect()
}

/// PATHEXT 里的后缀大小写混杂，统一成小写：Windows 文件系统不分大小写，
/// 拼出来的候选名在真机上照样命中。
fn extensions_from(pathext: Option<OsString>) -> Vec<String> {
    let raw = pathext
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    let listed: Vec<String> = raw
        .split(';')
        .map(|item| item.trim().trim_matches('"'))
        .filter(|item| !item.is_empty())
        .map(|item| item.to_ascii_lowercase())
        .collect();

    if listed.is_empty() {
        DEFAULT_PATHEXT.iter().map(|ext| ext.to_string()).collect()
    } else {
        listed
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
                "agent-config-sync-platform-{}-{}-{}",
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

    #[test]
    fn ordinary_files_and_directories_are_not_links() {
        let temp = TempDir::new("plain");
        fs::write(temp.path().join("file.md"), "内容").expect("写文件");
        fs::create_dir_all(temp.path().join("tree/nested")).expect("建目录");

        assert!(!is_link_like(&temp.path().join("file.md")));
        assert!(!is_link_like(&temp.path().join("tree")));
        assert!(!is_link_like(&temp.path().join("tree/nested")));
        assert!(!is_link_like(&temp.path().join("missing")));
    }

    #[test]
    fn link_like_accepts_junctions_and_broken_symlinks() {
        let junction = Ok(PathBuf::from("C:\\target"));
        let unreadable = Err(io::Error::new(io::ErrorKind::InvalidInput, "不是链接"));

        // junction：`is_symlink()` 不认，但读得到目标。
        assert!(link_like(false, &junction));
        // 断链：`is_symlink()` 认，读不到目标。
        assert!(link_like(true, &unreadable));
        assert!(!link_like(false, &unreadable));
    }

    #[cfg(unix)]
    #[test]
    fn links_are_links_even_when_the_target_is_gone() {
        let temp = TempDir::new("link");
        fs::create_dir_all(temp.path().join("tree")).expect("建目录");
        std::os::unix::fs::symlink("tree", temp.path().join("tree-link")).expect("建目录链接");
        std::os::unix::fs::symlink("nowhere", temp.path().join("broken")).expect("建断链");

        assert!(is_link_like(&temp.path().join("tree-link")));
        assert!(is_link_like(&temp.path().join("broken")));
    }

    #[cfg(unix)]
    #[test]
    fn removing_a_link_leaves_its_target_alone() {
        let temp = TempDir::new("remove-link");
        fs::create_dir_all(temp.path().join("tree")).expect("建目录");
        fs::write(temp.path().join("tree/inside.md"), "内容").expect("写文件");
        std::os::unix::fs::symlink("tree", temp.path().join("tree-link")).expect("建目录链接");

        remove_link(&temp.path().join("tree-link")).expect("删链接");

        assert!(!temp.path().join("tree-link").exists());
        assert!(temp.path().join("tree/inside.md").is_file(), "目标不被删");
    }

    #[test]
    fn npm_install_path_is_recognized_on_both_separators() {
        assert!(is_npm_install_path(
            "/usr/lib/node_modules/@earendil-works/pi-coding-agent/bin/pi"
        ));
        assert!(is_npm_install_path(
            "C:\\Users\\u\\AppData\\Roaming\\npm\\node_modules\\@earendil-works\\pi-coding-agent\\bin\\pi.cmd"
        ));
        assert!(is_npm_install_path("C:\\npm\\Node_Modules\\pi\\pi.cmd"));
        assert!(!is_npm_install_path("/usr/local/bin/pi"));
        assert!(!is_npm_install_path(
            "C:\\Users\\u\\.pi\\agent\\bin\\pi.cmd"
        ));
    }

    #[test]
    fn quotes_depend_on_the_platform() {
        assert_eq!(sh_quote("npm:o'brien"), "'npm:o'\\''brien'");
        assert_eq!(
            quote_arg(HostOs::Posix, "npm:pi-lens"),
            Ok("'npm:pi-lens'".to_string())
        );
        assert_eq!(
            quote_arg(HostOs::Windows, "npm:pi-lens"),
            Ok("\"npm:pi-lens\"".to_string())
        );
        // 程序路径只加引号，不过拒绝名单。
        assert_eq!(
            quote_program(HostOs::Windows, "C:\\Users\\a&b\\pi.cmd"),
            "\"C:\\Users\\a&b\\pi.cmd\""
        );
        assert_eq!(quote_program(HostOs::Posix, "/usr/bin/pi"), "/usr/bin/pi");
    }

    #[test]
    fn windows_quoting_rejects_cmd_metacharacters() {
        for value in [
            "npm:a\"b", "npm:a%b", "npm:a!b", "npm:a&b", "npm:a|b", "npm:a<b", "npm:a>b",
            "npm:a^b", "npm:a\rb", "npm:a\nb",
        ] {
            let error = quote_arg(HostOs::Windows, value).unwrap_err();
            assert!(error.contains("拒绝执行"), "{value}: {error}");
        }
        // POSIX 侧这些字符被单引号包住，不需要拒绝。
        assert!(quote_arg(HostOs::Posix, "npm:a%b&c").is_ok());
    }

    #[test]
    fn windows_commands_go_through_cmd_with_one_quoted_line() {
        let argv = program_argv(HostOs::Windows, "C:\\nodejs\\pi.cmd", &["--version"]).unwrap();
        assert_eq!(&argv[..4], ["cmd", "/d", "/s", "/c"]);
        // 整行以引号开头、以引号结尾：/s 只去这一对外层引号，内部引号原样交给程序。
        let line = &argv[4];
        assert!(
            line.starts_with('"') && line.ends_with('"'),
            "整行必须由外层引号包住：{line}"
        );
        assert_eq!(
            line,
            &cmd_line("C:\\nodejs\\pi.cmd", &["--version"]),
            "外层引号 + 内部逐个加引号的元素"
        );

        // 程序路径只做引号包裹：cmd 元字符出现在路径里不该让整条命令被拒绝。
        let argv = program_argv(HostOs::Windows, "C:\\Users\\a&b\\pi.cmd", &["list"]).unwrap();
        assert_eq!(
            argv[4],
            cmd_line("C:\\Users\\a&b\\pi.cmd", &["list"]),
            "路径里的 & 在引号内不构成命令分隔"
        );

        // POSIX 不经 shell：逐命令直接给程序与参数。
        assert_eq!(
            program_argv(
                HostOs::Posix,
                "/usr/bin/node",
                &["-p", "process.versions.node"]
            )
            .unwrap(),
            vec!["/usr/bin/node", "-p", "process.versions.node"]
        );
    }

    #[test]
    fn program_lookup_follows_path_dirs_then_pathext_order() {
        let temp = TempDir::new("lookup");
        let bin = temp.path().join("bin");
        fs::create_dir_all(&bin).expect("建目录");
        fs::write(bin.join("npm.exe"), "").expect("写文件");
        fs::write(bin.join("npm.cmd"), "").expect("写文件");

        let search = ProgramPath::new(
            vec![temp.path().join("empty"), bin.clone()],
            vec![".cmd".to_string(), ".exe".to_string()],
        );

        assert_eq!(search.find("npm"), Some(bin.join("npm.cmd")));
        assert_eq!(search.find("node"), None);

        fs::write(bin.join("pi.cmd"), "").expect("写文件");
        assert_eq!(search.find_in(&bin, "pi"), Some(bin.join("pi.cmd")));
    }

    #[test]
    fn path_and_pathext_entries_are_normalized() {
        let joined = std::env::join_paths([Path::new("/opt/a"), Path::new("/opt/b")]).unwrap();
        assert_eq!(
            path_dirs(Some(joined)),
            vec![PathBuf::from("/opt/a"), PathBuf::from("/opt/b")]
        );
        assert_eq!(
            path_dirs(Some(OsString::from("\"/opt/a b\""))),
            vec![PathBuf::from("/opt/a b")]
        );
        assert_eq!(path_dirs(None), Vec::<PathBuf>::new());

        assert_eq!(
            extensions_from(None),
            vec![
                ".com".to_string(),
                ".exe".to_string(),
                ".bat".to_string(),
                ".cmd".to_string()
            ]
        );
        assert_eq!(
            extensions_from(Some(OsString::from(".CMD;.EXE"))),
            vec![".cmd".to_string(), ".exe".to_string()]
        );
    }
}

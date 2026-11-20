//! 密钥向导：列出 manifest 的 `requiredSecrets`、检测本机环境变量、写入本机字面值。
//!
//! 规则来源：`docs/design/30-secrets.md` 第 6 节。两条路各自独立：
//! 环境变量只生成命令交给用户复制，本模块不写 shell 启动文件、不改进程环境；
//! 字面值只写本机宿主目录，不回写快照，也不碰 `auth.json`。
//! 密钥值只作为内存中的替换内容出现，永不进入返回值、错误信息或日志。

use crate::config::{self, AGENT_DIR_ENV_VAR};
use crate::doctor::{CommandOutcome, CommandRunner, ShellRunner, PROBE_TIMEOUT};
use crate::scope;
use crate::snapshot::{self, Manifest};
use serde::Serialize;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 环境变量值的长度上限，防止误贴整份文件。
pub const MAX_VALUE_LEN: usize = 8192;

/// 探测输出里包住结果串的哨兵，避免登录 shell 的启动噪音被当成结果。
const ENV_MARKER: &str = "__PI_WIZARD_ENV__";

/// 一条待补密钥的当前状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretStatus {
    pub name: String,
    /// 用到该密钥的本地文件，源自 manifest 的 `usedBy`，形如 `snapshot/web-search.json`。
    pub used_by: Vec<String>,
    pub hint: Option<String>,
    /// 本机登录 shell 里是否已有该变量。探测失败按缺失处理。
    pub env_present: bool,
}

/// 一次字面值写入的结果。只报数量，不带值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteReport {
    pub name: String,
    /// 实际改写的文件数。
    pub files: usize,
}

/// 向导失败。`code` 供 UI 分支，`message` 面向用户且绝不含密钥值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WizardFailure {
    pub code: &'static str,
    pub message: String,
}

impl WizardFailure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// 读出 manifest 里的待补密钥，并逐个探测本机环境变量。
///
/// `agent_dir` 只在写入时用到；状态与宿主目录无关，保留该参数是为了让两个入口的
/// 调用方式一致（command 层一次解析出两个路径）。
pub fn secret_status(
    _agent_dir: &Path,
    sync_root: &Path,
    runner: &dyn CommandRunner,
) -> Result<Vec<SecretStatus>, WizardFailure> {
    let manifest = read_manifest(sync_root)?;

    manifest
        .required_secrets
        .into_iter()
        .map(|secret| {
            if !is_valid_env_name(&secret.name) {
                return Err(WizardFailure::new(
                    "secret_name_invalid",
                    format!("manifest 里的密钥名不合法：{}", secret.name),
                ));
            }
            Ok(SecretStatus {
                env_present: env_present(&secret.name, runner),
                name: secret.name,
                used_by: secret.used_by,
                hint: secret.hint,
            })
        })
        .collect()
}

/// 把本机文件里的引用换回字面值。只写 `agent_dir` 内受管范围内的普通文件。
///
/// 至少命中一处引用才写；已是字面值的文件原样保留。返回只含名称与文件数量。
pub fn secret_write_literal(
    agent_dir: &Path,
    sync_root: &Path,
    name: &str,
    value: &str,
) -> Result<WriteReport, WizardFailure> {
    let manifest = read_manifest(sync_root)?;

    if !is_valid_env_name(name) {
        return Err(WizardFailure::new(
            "secret_name_invalid",
            format!("密钥名不合法：{name}"),
        ));
    }
    let secret = manifest
        .required_secrets
        .iter()
        .find(|secret| secret.name == name)
        .ok_or_else(|| {
            WizardFailure::new(
                "secret_unknown",
                format!("manifest 的 requiredSecrets 里没有 {name}"),
            )
        })?;

    if value.is_empty() {
        return Err(WizardFailure::new("secret_value_empty", "密钥值不能为空"));
    }
    if value.len() > MAX_VALUE_LEN {
        return Err(WizardFailure::new(
            "secret_value_too_long",
            format!("密钥值超过 {MAX_VALUE_LEN} 字节，请确认没有整段粘贴"),
        ));
    }
    // 换行/制表等控制字符会直接破坏 JSON 文件结构。
    if value.chars().any(char::is_control) {
        return Err(WizardFailure::new(
            "secret_value_invalid",
            "密钥值含换行或控制字符，请确认没有整段粘贴",
        ));
    }

    let mut files = 0;
    for used_by in &secret.used_by {
        let target = local_path(agent_dir, used_by)?;
        let text = fs::read_to_string(&target).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => WizardFailure::new(
                "file_missing",
                format!("本机缺少 {used_by}，请先在快照页应用后再补密钥"),
            ),
            _ => WizardFailure::new("file_unreadable", format!("读取 {used_by} 失败：{error}")),
        })?;

        let (updated, hits) = replace_references(&text, name, value);
        if hits == 0 {
            continue;
        }
        write_preserving_mode(&target, &updated).map_err(|error| {
            WizardFailure::new("write_failed", format!("写入 {used_by} 失败：{error}"))
        })?;
        files += 1;
    }

    if files == 0 {
        return Err(WizardFailure::new(
            "reference_not_found",
            format!("{name} 的引用已不存在（可能已是字面值），本机文件没有改动"),
        ));
    }

    Ok(WriteReport {
        name: name.to_string(),
        files,
    })
}

fn read_manifest(sync_root: &Path) -> Result<Manifest, WizardFailure> {
    let path = sync_root.join(snapshot::MANIFEST_FILE_NAME);
    let text = fs::read_to_string(&path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => WizardFailure::new(
            "manifest_missing",
            "同步文件夹里没有 manifest.json，请先在快照页导出一次",
        ),
        _ => WizardFailure::new("manifest_unreadable", format!("读取 manifest 失败：{error}")),
    })?;

    serde_json::from_str(&text)
        .map_err(|error| WizardFailure::new("manifest_invalid", format!("manifest 无法解析：{error}")))
}

/// 环境变量名限定为 POSIX 形式：既保证 `$NAME` 引用合法，也避免把名字拼进 shell 脚本时被解释。
fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// 在登录 shell 里问一次「该变量是否已定义」。
///
/// shortcut: 探测失败（超时、shell 起不来）按「缺失」处理，UI 会显示可补而不会误报已配置。
fn env_present(name: &str, runner: &dyn CommandRunner) -> bool {
    let script = format!("printf '{ENV_MARKER}%s{ENV_MARKER}\\n' \"${{{name}+x}}\"");
    match runner.run(&script, PROBE_TIMEOUT, &[]) {
        CommandOutcome::Ok { stdout, .. } => marker_value(&stdout) == Some("x"),
        _ => false,
    }
}

fn marker_value(stdout: &str) -> Option<&str> {
    let start = stdout.find(ENV_MARKER)? + ENV_MARKER.len();
    let rest = &stdout[start..];
    let end = rest.find(ENV_MARKER)?;
    Some(&rest[..end])
}

/// `usedBy` 形如 `snapshot/web-search.json`：去掉快照前缀后落到宿主目录，越界与排除名一律拒绝。
fn local_path(agent_dir: &Path, used_by: &str) -> Result<PathBuf, WizardFailure> {
    let relative = used_by
        .strip_prefix(&format!("{}/", snapshot::SNAPSHOT_DIR_NAME))
        .ok_or_else(|| {
            WizardFailure::new("used_by_invalid", format!("usedBy 不在快照目录下：{used_by}"))
        })?;

    let relative = Path::new(relative);
    // `auth.json` 等排除名属于受管范围之外：不读内容也不写入。
    let excluded = relative.components().any(|component| match component {
        Component::Normal(name) => scope::is_excluded_name(&name.to_string_lossy()),
        _ => false,
    });
    if excluded {
        return Err(WizardFailure::new(
            "used_by_invalid",
            format!("usedBy 指向不受管的文件：{used_by}"),
        ));
    }

    scope::resolve_within(agent_dir, relative)
        .map_err(|_| WizardFailure::new("used_by_invalid", format!("usedBy 越出宿主目录：{used_by}")))
}

/// 三种引用语法一起换，单趟扫描：`${NAME}` 优先，其次 `$NAME`（后面不能紧跟标识符字符，
/// 避免 `$FOO` 吃掉 `$FOOBAR`）。写入的值不参与后续匹配，因此值里出现 `$NAME` 也不会被二次替换。
fn replace_references(text: &str, name: &str, value: &str) -> (String, usize) {
    let braced = format!("${{{name}}}");
    let plain = format!("${name}");
    let mut out = String::with_capacity(text.len());
    let mut hits = 0;
    let mut rest = text;

    while let Some(index) = rest.find('$') {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        if tail.starts_with(&braced) {
            out.push_str(value);
            hits += 1;
            rest = &tail[braced.len()..];
        } else if tail.starts_with(&plain)
            && tail[plain.len()..]
                .chars()
                .next()
                .is_none_or(|ch| !(ch.is_ascii_alphanumeric() || ch == '_'))
        {
            out.push_str(value);
            hits += 1;
            rest = &tail[plain.len()..];
        } else {
            out.push('$');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);

    (out, hits)
}

/// 同目录临时文件 + rename；覆盖普通文件时沿用原 mode，符号链接与目录一律不写。
fn write_preserving_mode(target: &Path, text: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(target)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "目标不是普通文件",
        ));
    }
    let mode = metadata.permissions().mode() & 0o777;

    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "目标路径没有文件名"))?;
    let mut temp_name = OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(".tmp-{}", now_nanos()));
    let temp = target.with_file_name(temp_name);

    let written = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(text.as_bytes())?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(mode))?;
        fs::rename(&temp, target)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
}

/// 两个入口共用的路径解析：应用配置里的同步根目录 + 生效宿主目录。
fn paths(app: &tauri::AppHandle) -> Result<(PathBuf, PathBuf), WizardFailure> {
    use tauri::Manager;

    let config_dir = app.path().app_config_dir().map_err(|error| {
        WizardFailure::new(
            "app_config_unavailable",
            format!("无法定位应用配置目录：{error}"),
        )
    })?;
    let config = config::load_config(&config_dir)
        .map_err(|error| WizardFailure::new("app_config_unavailable", error.to_string()))?;

    let sync_root = config.sync_root.clone().ok_or_else(|| {
        WizardFailure::new(
            "sync_root_missing",
            "还没有设置同步文件夹，请先在设置页选择",
        )
    })?;

    // 只读取这一个文档化的环境变量；错误信息里不回显其内容。
    let env_agent_dir = std::env::var_os(AGENT_DIR_ENV_VAR).map(PathBuf::from);
    let home_dir = app.path().home_dir().ok();
    let agent_dir = config::resolve_agent_dir(&config, env_agent_dir.as_deref(), home_dir.as_deref())
        .path
        .ok_or_else(|| {
            WizardFailure::new("agent_dir_missing", "无法确定宿主目录，请先在设置页填写")
        })?;

    Ok((agent_dir, sync_root))
}

/// 探测要跑登录 shell，放阻塞线程池，别占住 UI 线程。
#[tauri::command]
pub async fn check_secrets(app: tauri::AppHandle) -> Result<Vec<SecretStatus>, WizardFailure> {
    tauri::async_runtime::spawn_blocking(move || {
        let (agent_dir, sync_root) = paths(&app)?;
        secret_status(&agent_dir, &sync_root, &ShellRunner::from_env())
    })
    .await
    .map_err(|error| WizardFailure::new("secret_status_failed", format!("读取密钥状态失败：{error}")))?
}

/// 写入本机字面值。写文件与读 manifest 都是阻塞操作。
#[tauri::command]
pub async fn write_secret(
    app: tauri::AppHandle,
    name: String,
    value: String,
) -> Result<WriteReport, WizardFailure> {
    tauri::async_runtime::spawn_blocking(move || {
        let (agent_dir, sync_root) = paths(&app)?;
        secret_write_literal(&agent_dir, &sync_root, &name, &value)
    })
    .await
    .map_err(|error| WizardFailure::new("secret_write_failed", format!("写入密钥失败：{error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    static NEXT_DIR_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    const SECRET: &str = "Zx7Qv2Lm9Rt4Bc8Wn3Yp6Kd1";

    /// 临时目录夹具：不引入 tempfile，Drop 时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = NEXT_DIR_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-wizard-{}-{}-{}",
                label,
                std::process::id(),
                unique
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("创建测试临时目录");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("创建父目录");
            }
            fs::write(path, contents).expect("写入夹具文件");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 假命令执行器：按调用顺序返回预置结果，并记录收到的脚本。
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
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            script: &str,
            _timeout: std::time::Duration,
            _env: &[(&str, String)],
        ) -> CommandOutcome {
            self.calls.borrow_mut().push(script.to_string());
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

    /// 真实的探测输出：哨兵夹住结果，前后可能还有 shell 启动噪音。
    fn probe_output(marker: &str) -> CommandOutcome {
        ok(&format!("welcome\n{ENV_MARKER}{marker}{ENV_MARKER}\ntrailing\n"))
    }

    /// 写一份最小可解析的 manifest；`secrets` 是 (name, usedBy)。
    fn write_manifest(sync_root: &Path, secrets: &[(&str, &[&str], Option<&str>)]) {
        let manifest = Manifest {
            format_version: snapshot::FORMAT_VERSION,
            profile_id: snapshot::PROFILE_ID.to_string(),
            created_at: "2026-11-20T05:30:00Z".to_string(),
            fingerprint: "a1b2c3d4e5f6".to_string(),
            source: snapshot::SourceInfo {
                app: "0.1.0".to_string(),
                os: "linux".to_string(),
                arch: "x64".to_string(),
                pi: None,
                node: None,
            },
            requirements: snapshot::Requirements {
                node: ">=22.19.0".to_string(),
                pi: ">=1.5.0".to_string(),
            },
            files: Vec::new(),
            symlinks: Vec::new(),
            required_secrets: secrets
                .iter()
                .map(|(name, used_by, hint)| crate::secrets::RequiredSecret {
                    name: name.to_string(),
                    used_by: used_by.iter().map(|path| path.to_string()).collect(),
                    hint: hint.map(str::to_string),
                })
                .collect(),
            required_packages: Vec::new(),
            plaintext_secrets: false,
        };

        let mut text = serde_json::to_string_pretty(&manifest).expect("序列化 manifest");
        text.push('\n');
        fs::create_dir_all(sync_root).expect("创建同步目录");
        fs::write(sync_root.join(snapshot::MANIFEST_FILE_NAME), text).expect("写 manifest");
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("读取元数据").permissions().mode() & 0o777
    }

    #[test]
    fn status_parses_env_presence_from_marker_only() {
        let runner = FakeRunner::new(vec![
            probe_output("x"),
            probe_output(""),
            CommandOutcome::Failed {
                code: Some(1),
                stdout: String::new(),
                stderr: "boom".to_string(),
            },
        ]);

        assert!(env_present("EXA_API_KEY", &runner));
        assert!(!env_present("DOCS_TOKEN", &runner));
        assert!(!env_present("OTHER", &runner));

        // 脚本只带变量名，不带任何值；并用哨兵把结果夹住。
        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[0].contains("${EXA_API_KEY+x}"), "{}", calls[0]);
        assert!(calls[0].contains(ENV_MARKER));
        assert!(!calls[0].contains(SECRET));
    }

    #[test]
    fn status_reports_each_required_secret_with_hint() {
        let temp = TempDir::new("status");
        let sync_root = temp.path().join("cloud");
        write_manifest(
            &sync_root,
            &[
                (
                    "EXA_API_KEY",
                    &["snapshot/web-search.json"],
                    Some("https://exa.ai"),
                ),
                ("DOCS_TOKEN", &["snapshot/mcp.json"], None),
            ],
        );

        let runner = FakeRunner::new(vec![probe_output("x"), probe_output("")]);
        let statuses =
            secret_status(temp.path(), &sync_root, &runner).expect("应读出两个待补密钥");

        assert_eq!(
            statuses,
            vec![
                SecretStatus {
                    name: "EXA_API_KEY".to_string(),
                    used_by: vec!["snapshot/web-search.json".to_string()],
                    hint: Some("https://exa.ai".to_string()),
                    env_present: true,
                },
                SecretStatus {
                    name: "DOCS_TOKEN".to_string(),
                    used_by: vec!["snapshot/mcp.json".to_string()],
                    hint: None,
                    env_present: false,
                },
            ]
        );
    }

    #[test]
    fn status_reports_missing_and_broken_manifest() {
        let temp = TempDir::new("status-bad");
        let sync_root = temp.path().join("cloud");
        fs::create_dir_all(&sync_root).expect("创建同步目录");
        let runner = FakeRunner::new(Vec::new());

        let failure =
            secret_status(temp.path(), &sync_root, &runner).expect_err("缺 manifest 应报错");
        assert_eq!(failure.code, "manifest_missing");
        assert!(failure.message.contains("manifest.json"));

        fs::write(sync_root.join(snapshot::MANIFEST_FILE_NAME), "{not json")
            .expect("写坏 manifest");
        let failure =
            secret_status(temp.path(), &sync_root, &runner).expect_err("坏 manifest 应报错");
        assert_eq!(failure.code, "manifest_invalid");
        assert!(failure.message.contains("manifest"));
    }

    #[test]
    fn status_rejects_invalid_secret_name_in_manifest() {
        let temp = TempDir::new("status-name");
        let sync_root = temp.path().join("cloud");
        write_manifest(&sync_root, &[("BAD NAME; rm -rf", &["snapshot/mcp.json"], None)]);

        let runner = FakeRunner::new(Vec::new());
        let failure =
            secret_status(temp.path(), &sync_root, &runner).expect_err("非法名应报错");
        assert_eq!(failure.code, "secret_name_invalid");
        // 名字没被拼进 shell。
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn references_are_replaced_for_all_three_syntaxes() {
        let text = r#"{"a": "$EXA_API_KEY", "b": "${EXA_API_KEY}", "c": "Bearer ${EXA_API_KEY}"}"#;
        let (updated, hits) = replace_references(text, "EXA_API_KEY", SECRET);

        assert_eq!(hits, 3);
        assert_eq!(
            updated,
            format!(r#"{{"a": "{SECRET}", "b": "{SECRET}", "c": "Bearer {SECRET}"}}"#)
        );
        assert!(!updated.contains("EXA_API_KEY"));
    }

    #[test]
    fn plain_form_respects_identifier_boundary() {
        // `$FOO` 不能吃掉 `$FOOBAR` / `$FOO_2`；`${FOO}` 与裸 `$FOO` 各自命中一次。
        let text = r#"["$FOO", "$FOOBAR", "$FOO_2", "${FOO}", "${FOOBAR}"]"#;
        let (updated, hits) = replace_references(text, "FOO", SECRET);

        assert_eq!(hits, 2);
        assert_eq!(
            updated,
            format!(r#"["{SECRET}", "$FOOBAR", "$FOO_2", "{SECRET}", "${{FOOBAR}}"]"#)
        );
    }

    #[test]
    fn literal_value_that_looks_like_a_reference_is_not_rescanned() {
        let text = r#"{"a": "$FOO", "b": "${FOO}"}"#;
        let (updated, hits) = replace_references(text, "FOO", "$FOO$FOO");

        assert_eq!(hits, 2);
        assert_eq!(updated, r#"{"a": "$FOO$FOO", "b": "$FOO$FOO"}"#);
    }

    #[test]
    fn write_keeps_mode_and_only_touches_referencing_files() {
        let temp = TempDir::new("write");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        write_manifest(
            &sync_root,
            &[
                ("EXA_API_KEY", &["snapshot/web-search.json"], None),
                ("OPENROUTER_API_KEY", &["snapshot/models.json"], None),
            ],
        );
        let web_search = agent.join("web-search.json");
        fs::create_dir_all(&agent).expect("创建宿主目录");
        fs::write(&web_search, r#"{"exaApiKey": "$EXA_API_KEY"}"#).expect("写夹具");
        fs::set_permissions(&web_search, fs::Permissions::from_mode(0o600)).expect("设置权限");

        let report = secret_write_literal(&agent, &sync_root, "EXA_API_KEY", SECRET)
            .expect("应写入成功");

        assert_eq!(
            report,
            WriteReport {
                name: "EXA_API_KEY".to_string(),
                files: 1,
            }
        );
        assert_eq!(
            fs::read_to_string(&web_search).expect("读结果"),
            format!(r#"{{"exaApiKey": "{SECRET}"}}"#)
        );
        assert_eq!(mode_of(&web_search), 0o600);
        // 没有临时文件残留。
        let names: Vec<String> = fs::read_dir(&agent)
            .expect("读目录")
            .map(|item| item.expect("目录项").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["web-search.json"]);
        // 序列化结果里没有值。
        assert!(!serde_json::to_string(&report).expect("序列化").contains(SECRET));
    }

    #[test]
    fn write_rejects_unknown_name_empty_and_oversized_values() {
        let temp = TempDir::new("write-bad");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        write_manifest(
            &sync_root,
            &[("EXA_API_KEY", &["snapshot/web-search.json"], None)],
        );
        fs::create_dir_all(&agent).expect("创建宿主目录");
        fs::write(
            agent.join("web-search.json"),
            r#"{"exaApiKey": "$EXA_API_KEY"}"#,
        )
        .expect("写夹具");

        let failure = secret_write_literal(&agent, &sync_root, "NOPE", SECRET)
            .expect_err("未知 name 应拒绝");
        assert_eq!(failure.code, "secret_unknown");

        let failure = secret_write_literal(&agent, &sync_root, "EXA_API_KEY", "")
            .expect_err("空值应拒绝");
        assert_eq!(failure.code, "secret_value_empty");

        let long = "a".repeat(MAX_VALUE_LEN + 1);
        let failure = secret_write_literal(&agent, &sync_root, "EXA_API_KEY", &long)
            .expect_err("超长应拒绝");
        assert_eq!(failure.code, "secret_value_too_long");

        let failure = secret_write_literal(&agent, &sync_root, "EXA_API_KEY", ".env 里贴进来\nX=1")
            .expect_err("含换行应拒绝");
        assert_eq!(failure.code, "secret_value_invalid");

        let failure = secret_write_literal(&agent, &sync_root, "bad name", SECRET)
            .expect_err("非法 name 应拒绝");
        assert_eq!(failure.code, "secret_name_invalid");

        // 所有错误信息都不含值。
        for failure in [
            secret_write_literal(&agent, &sync_root, "NOPE", SECRET).unwrap_err(),
            secret_write_literal(&agent, &sync_root, "EXA_API_KEY", &long).unwrap_err(),
        ] {
            assert!(!failure.message.contains(SECRET), "{}", failure.message);
            assert!(!failure.message.contains(&long), "{}", failure.message);
        }

        // 夹具文件没有被改动。
        assert_eq!(
            fs::read_to_string(agent.join("web-search.json")).expect("读夹具"),
            r#"{"exaApiKey": "$EXA_API_KEY"}"#
        );
    }

    #[test]
    fn write_rejects_used_by_outside_managed_scope() {
        let temp = TempDir::new("write-scope");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        fs::create_dir_all(&agent).expect("创建宿主目录");
        fs::write(agent.join("auth.json"), "{}").expect("写夹具");

        for used_by in [
            "snapshot/../auth.json",
            "snapshot/auth.json",
            "snapshot/",
            "snapshot//etc/passwd",
            "auth.json",
            "snapshot/skills/auth.json",
        ] {
            write_manifest(&sync_root, &[("EXA_API_KEY", &[used_by], None)]);
            let failure = secret_write_literal(&agent, &sync_root, "EXA_API_KEY", SECRET)
                .expect_err("越界 usedBy 应拒绝");
            assert_eq!(failure.code, "used_by_invalid", "usedBy={used_by}");
        }

        // 越界路径没有被写入任何内容。
        assert_eq!(fs::read_to_string(agent.join("auth.json")).expect("读夹具"), "{}");
    }

    #[test]
    fn write_reports_reference_not_found_and_keeps_file() {
        let temp = TempDir::new("write-literal");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        write_manifest(
            &sync_root,
            &[("EXA_API_KEY", &["snapshot/web-search.json"], None)],
        );
        fs::create_dir_all(&agent).expect("创建宿主目录");
        let contents = format!(r#"{{"exaApiKey": "{SECRET}"}}"#);
        fs::write(agent.join("web-search.json"), &contents).expect("写夹具");

        let failure = secret_write_literal(&agent, &sync_root, "EXA_API_KEY", "new-value")
            .expect_err("已是字面值应报引用不存在");
        assert_eq!(failure.code, "reference_not_found");
        assert!(!failure.message.contains(SECRET));
        assert_eq!(
            fs::read_to_string(agent.join("web-search.json")).expect("读夹具"),
            contents
        );
    }

    #[test]
    fn write_reports_missing_local_file() {
        let temp = TempDir::new("write-missing");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        write_manifest(
            &sync_root,
            &[("EXA_API_KEY", &["snapshot/web-search.json"], None)],
        );
        fs::create_dir_all(&agent).expect("创建宿主目录");

        let failure = secret_write_literal(&agent, &sync_root, "EXA_API_KEY", SECRET)
            .expect_err("缺本机文件应报错");
        assert_eq!(failure.code, "file_missing");
        assert!(!failure.message.contains(SECRET));
    }

    #[test]
    fn env_name_validation_is_posix_only() {
        for name in ["EXA_API_KEY", "_X", "A1"] {
            assert!(is_valid_env_name(name), "{name}");
        }
        for name in ["", "1X", "A-B", "A B", "A;X", "${A}"] {
            assert!(!is_valid_env_name(name), "{name}");
        }
    }
}

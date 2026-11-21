//! 回滚：列出本机备份、按备份还原本机配置，还原前再备份一次。
//!
//! 规则来源：`docs/design/10-snapshot-sync.md` 第 7 节。
//! 备份由 [`crate::apply`] 写入（`backups/<时间戳>/restore.json` + `files/`），这里只按它反做：
//! 先重建记录里的链接，再写回文件与权限，最后删除应用新增的路径。
//! 失败停在原地：任一条失败即停，已完成的部分留在原地，还原前的备份保留；不自动重试、不自动回滚。
//! 核心 [`read_backups`] / [`restore`] 不依赖 Tauri，可直接临时目录单测。

use crate::apply::{
    self, RestoreEntry, RestoreKind, RestoreManifest, BACKUPS_DIR_NAME, BACKUP_FILES_DIR_NAME,
    RESTORE_FILE_NAME, RESTORE_VERSION,
};
use crate::config;
use crate::scope;
use serde::Serialize;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

/// 列表最多返回几份备份；更旧的只在磁盘上。
pub const BACKUP_LIST_LIMIT: usize = 20;
/// 备份里没记权限时的文件权限，与 [`crate::apply`] 新增文件一致。
const DEFAULT_FILE_MODE: u32 = 0o644;

/// 一份备份的摘要：列表用，不含条目细节。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSummary {
    /// 备份目录名，也是回滚时要的 `backupId`。
    pub id: String,
    pub created_at: String,
    /// 备份来源快照的指纹。
    pub fingerprint: String,
    /// 条目数。
    pub entries: usize,
}

/// 备份列表。读不出或损坏的备份目录只跳过并记一条提示，不删。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupList {
    pub backups: Vec<BackupSummary>,
    pub warnings: Vec<String>,
}

/// 一次回滚的结果。失败时改由 [`RestoreFailure`] 返回，`failed_at` 因此总是 `None`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreReport {
    /// 被还原的备份目录名。
    pub backup_id: String,
    /// 还原前自动做的那份备份，可用来再滚回去。
    pub safety_backup_id: String,
    /// 按记录写回或重建的条数。
    pub restored: usize,
    /// 删除应用新增路径的条数。
    pub removed: usize,
    pub failed_at: Option<String>,
}

/// 回滚失败。失败码：`not_confirmed` / `backup_missing` / `backup_invalid` /
/// `path_out_of_bounds` / `backup_failed` / `restore_failed`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreFailure {
    pub code: &'static str,
    pub message: String,
    /// 失败时已完成的条目数。
    pub completed: usize,
    /// 还原前备份的目录名；还没建备份就失败则为 `None`。
    pub safety_backup_id: Option<String>,
}

impl RestoreFailure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            completed: 0,
            safety_backup_id: None,
        }
    }
}

/// core：列出最近 [`BACKUP_LIST_LIMIT`] 份备份，按时间倒序。
/// 目录名是 UTC 时间戳，按名字倒序即按时间倒序。
pub fn read_backups(app_data_dir: &Path) -> BackupList {
    let root = app_data_dir.join(BACKUPS_DIR_NAME);
    let mut warnings: Vec<String> = Vec::new();
    let mut ids: Vec<String> = match fs::read_dir(&root) {
        Ok(read_dir) => read_dir
            .filter_map(|item| item.ok())
            .filter(|item| item.path().is_dir())
            .map(|item| item.file_name().to_string_lossy().into_owned())
            .collect(),
        // 还没做过备份：空列表，不是错误。
        Err(_) => {
            return BackupList {
                backups: Vec::new(),
                warnings,
            }
        }
    };
    ids.sort_by(|left, right| right.cmp(left));

    let mut backups: Vec<BackupSummary> = Vec::new();
    for id in ids {
        if backups.len() == BACKUP_LIST_LIMIT {
            break;
        }
        match read_manifest(&root.join(&id)) {
            Ok(manifest) => backups.push(BackupSummary {
                id,
                created_at: manifest.created_at,
                fingerprint: manifest.fingerprint,
                entries: manifest.entries.len(),
            }),
            Err(error) => warnings.push(format!("备份 {id} 已跳过：{error}")),
        }
    }

    BackupList { backups, warnings }
}

/// 读一份备份的 `restore.json`。版本不认识也算损坏：宁可跳过，不按旧格式猜。
fn read_manifest(backup_dir: &Path) -> Result<RestoreManifest, String> {
    let path = backup_dir.join(RESTORE_FILE_NAME);
    let text =
        fs::read_to_string(&path).map_err(|error| format!("restore.json 读不出来：{error}"))?;
    let manifest: RestoreManifest =
        serde_json::from_str(&text).map_err(|error| format!("restore.json 无法解析：{error}"))?;
    if manifest.version != RESTORE_VERSION {
        return Err(format!("restore.json 版本 {} 不认识", manifest.version));
    }
    Ok(manifest)
}

/// core：按备份还原。`confirm` 必须为 true；还原前先把当前状态再备份一次。
pub fn restore(
    agent_dir: &Path,
    app_data_dir: &Path,
    backup_id: &str,
    confirm: bool,
) -> Result<RestoreReport, RestoreFailure> {
    if !confirm {
        return Err(RestoreFailure::new(
            "not_confirmed",
            "回滚会覆盖本机配置，需要先确认",
        ));
    }

    let root = app_data_dir.join(BACKUPS_DIR_NAME);
    // 备份目录名只允许一级目录名：`..` 与绝对路径在这里就被挡下。
    let backup_dir = scope::resolve_within(&root, Path::new(backup_id)).map_err(|error| {
        RestoreFailure::new(
            "backup_missing",
            format!("备份 {backup_id} 不可用：{error}"),
        )
    })?;
    if !backup_dir.is_dir() {
        return Err(RestoreFailure::new(
            "backup_missing",
            format!("备份 {backup_id} 不存在"),
        ));
    }
    let manifest = read_manifest(&backup_dir).map_err(|error| {
        RestoreFailure::new(
            "backup_invalid",
            format!("备份 {backup_id} 无法还原：{error}"),
        )
    })?;

    // 先整份校验：任何一条越出受管范围就整次拒绝，一个字都不写。
    for entry in &manifest.entries {
        if !in_scope(agent_dir, &entry.path) {
            return Err(RestoreFailure::new(
                "path_out_of_bounds",
                format!("{} 不在受管范围内，整次回滚已拒绝", entry.path),
            ));
        }
    }

    let touched: Vec<&str> = manifest
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    let (safety_backup_id, _) =
        apply::back_up_paths(agent_dir, app_data_dir, &touched, &manifest.fingerprint).map_err(
            |failure| {
                RestoreFailure::new(
                    "backup_failed",
                    format!("还原前备份失败，未改动任何文件：{}", failure.message),
                )
            },
        )?;

    let mut restored = 0;
    let mut removed = 0;
    for entry in &manifest.entries {
        let outcome = if entry.existed {
            restore_entry(agent_dir, &backup_dir, entry)
        } else {
            remove_added(agent_dir, entry)
        };
        if let Err(error) = outcome {
            return Err(RestoreFailure {
                code: "restore_failed",
                message: format!(
                    "还原 {} 时失败：{error}；已立即停止，还原前的备份是 {safety_backup_id}",
                    entry.path
                ),
                completed: restored + removed,
                safety_backup_id: Some(safety_backup_id),
            });
        }
        if entry.existed {
            restored += 1;
        } else {
            removed += 1;
        }
    }

    Ok(RestoreReport {
        backup_id: backup_id.to_string(),
        safety_backup_id,
        restored,
        removed,
        failed_at: None,
    })
}

/// 路径既要在宿主目录之内，也要落在受管范围内（`auth.json` 这类永不碰）。
fn in_scope(agent_dir: &Path, relative: &str) -> bool {
    let path = Path::new(relative);
    scope::resolve_within(agent_dir, path).is_ok() && apply::is_managed_link(path)
}

/// 按记录反做一条：链接重建、文件写回内容与权限、目录补齐。
fn restore_entry(agent_dir: &Path, backup_dir: &Path, entry: &RestoreEntry) -> io::Result<()> {
    let target = resolve(agent_dir, &entry.path)?;
    match entry.kind {
        RestoreKind::Symlink => {
            let link_target = entry
                .link_target
                .as_deref()
                .ok_or_else(|| io::Error::other(format!("{} 的记录里没有链接目标", entry.path)))?;
            remove_any(&target)?;
            apply::create_link(link_target, &target)
        }
        RestoreKind::Dir => ensure_dir(&target),
        RestoreKind::File => {
            let source = resolve(&backup_dir.join(BACKUP_FILES_DIR_NAME), &entry.path)?;
            let bytes = fs::read(&source).map_err(|error| {
                io::Error::other(format!("备份里没有 {} 的内容：{error}", entry.path))
            })?;
            if let Some(parent) = target.parent() {
                // 父目录可能是记录里要保留的链接：跟随它，不换成真实目录。
                fs::create_dir_all(parent)?;
            }
            // 绝不写穿链接：目标是链接先 unlink，是目录则停下让人看。
            match fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.is_dir() => {
                    return Err(io::Error::other(format!(
                        "{} 现在是目录，不覆盖",
                        entry.path
                    )))
                }
                Ok(metadata) if metadata.file_type().is_symlink() => fs::remove_file(&target)?,
                _ => {}
            }
            let mode = entry.mode.unwrap_or(DEFAULT_FILE_MODE) & 0o777;
            fs::write(&target, &bytes)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))
        }
    }
}

/// 删除应用新增的路径：只删文件或链接本身，目录一律拒绝，不递归删。
fn remove_added(agent_dir: &Path, entry: &RestoreEntry) -> io::Result<()> {
    if let Some(ancestor) = apply::symlink_ancestors(agent_dir, &entry.path)
        .into_iter()
        .next()
    {
        return Err(io::Error::other(format!(
            "路径经过符号链接 {ancestor}，删除会落到链接指向的外部位置，已拒绝"
        )));
    }
    let target = resolve(agent_dir, &entry.path)?;
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.is_dir() => Err(io::Error::other(format!(
            "{} 现在是目录，不递归删除",
            entry.path
        ))),
        Ok(_) => fs::remove_file(&target),
        // 已经不在了：按已还原算。
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// 目录条目：缺就建，链接换成真目录，是文件则报错。
fn ensure_dir(target: &Path) -> io::Result<()> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::remove_file(target)?,
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(io::Error::other(format!(
                "{} 现在是文件，无法建成目录",
                target.display()
            )))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::create_dir_all(target)
}

/// 清掉一条路径（目录整棵、其余只删自身），不存在也算清干净。
fn remove_any(target: &Path) -> io::Result<()> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(target),
        Ok(_) => fs::remove_file(target),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn resolve(root: &Path, relative: &str) -> io::Result<PathBuf> {
    scope::resolve_within(root, Path::new(relative))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| format!("无法定位应用数据目录：{error}"))
}

/// command 层入口：应用数据目录由 command 注入，核心不碰 Tauri。
#[tauri::command]
pub async fn list_backups(app: AppHandle) -> Result<BackupList, String> {
    let app_data_dir = app_data_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || read_backups(&app_data_dir))
        .await
        .map_err(|error| format!("列出备份失败：{error}"))
}

/// command 层入口：按备份还原；宿主目录与同步根目录取自应用配置。
#[tauri::command]
pub async fn restore_backup(
    app: AppHandle,
    backup_id: String,
    confirm: bool,
) -> Result<RestoreReport, RestoreFailure> {
    tauri::async_runtime::spawn_blocking(move || run(&app, &backup_id, confirm))
        .await
        .map_err(|error| RestoreFailure::new("restore_failed", format!("回滚失败：{error}")))?
}

fn run(app: &AppHandle, backup_id: &str, confirm: bool) -> Result<RestoreReport, RestoreFailure> {
    let app_data_dir = app_data_dir(app)
        .map_err(|message| RestoreFailure::new("app_data_unavailable", message))?;
    let config_dir = app.path().app_config_dir().map_err(|error| {
        RestoreFailure::new(
            "app_config_unavailable",
            format!("无法定位应用配置目录：{error}"),
        )
    })?;
    let config = config::load_config(&config_dir)
        .map_err(|error| RestoreFailure::new("app_config_unavailable", error.to_string()))?;

    // 只读取这一个文档化的环境变量；错误信息里不回显其内容。
    let env_agent_dir = std::env::var_os(config::AGENT_DIR_ENV_VAR).map(PathBuf::from);
    let home_dir = app.path().home_dir().ok();
    let resolved =
        config::resolve_agent_dir(&config, env_agent_dir.as_deref(), home_dir.as_deref());
    let agent_dir = resolved.path.ok_or_else(|| {
        RestoreFailure::new("agent_dir_missing", "无法确定宿主目录，请先在设置页填写")
    })?;

    restore(&agent_dir, &app_data_dir, backup_id, confirm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::BACKUP_RETENTION;
    use crate::doctor::{CommandOutcome, CommandRunner};
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// 临时目录夹具：Drop 时清理。测试全程不碰真实 HOME。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-rollback-{}-{}-{}",
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

    /// 固定版本的假命令：计划里的版本探测不起真实进程。
    struct FakeRunner;

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            _script: &str,
            _timeout: Duration,
            _env: &[(&str, String)],
        ) -> CommandOutcome {
            CommandOutcome::Ok {
                stdout: "node:22.23.0\npi:1.5.2\n".to_string(),
                stderr: String::new(),
            }
        }
    }

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("创建父目录");
        }
        fs::write(path, content).expect("写入夹具文件");
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path)
            .expect("读取文件信息")
            .permissions()
            .mode()
            & 0o777
    }

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("设置夹具权限");
    }

    fn app_data(temp: &TempDir) -> PathBuf {
        let dir = temp.path().join("app-data");
        fs::create_dir_all(&dir).expect("创建应用数据目录");
        dir
    }

    fn backup_ids(app_data: &Path) -> Vec<String> {
        let Ok(read_dir) = fs::read_dir(app_data.join(BACKUPS_DIR_NAME)) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = read_dir
            .filter_map(|item| item.ok())
            .filter(|item| item.path().is_dir())
            .map(|item| item.file_name().to_string_lossy().into_owned())
            .collect();
        ids.sort();
        ids
    }

    fn file_entry(path: &str, existed: bool, mode: u32) -> RestoreEntry {
        RestoreEntry {
            path: path.to_string(),
            kind: RestoreKind::File,
            existed,
            mode: Some(mode),
            link_target: None,
        }
    }

    fn manifest(entries: Vec<RestoreEntry>) -> RestoreManifest {
        RestoreManifest {
            version: RESTORE_VERSION,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            fingerprint: "0123456789ab".to_string(),
            entries,
        }
    }

    /// 手写一份备份：`restore.json` 加 `files/` 里的内容，格式与 apply 一致。
    fn craft_backup(app_data: &Path, id: &str, manifest: &RestoreManifest) {
        let dir = app_data.join(BACKUPS_DIR_NAME).join(id);
        fs::create_dir_all(dir.join(BACKUP_FILES_DIR_NAME)).expect("建备份目录");
        let mut text = serde_json::to_string_pretty(manifest).expect("序列化 restore.json");
        text.push('\n');
        fs::write(dir.join(RESTORE_FILE_NAME), text).expect("写 restore.json");
    }

    fn craft_content(app_data: &Path, id: &str, relative: &str, content: &str) {
        write_file(
            &app_data
                .join(BACKUPS_DIR_NAME)
                .join(id)
                .join(BACKUP_FILES_DIR_NAME)
                .join(relative),
            content,
        );
    }

    fn read_restore(backup_dir: &Path) -> RestoreManifest {
        let text = fs::read_to_string(backup_dir.join(RESTORE_FILE_NAME)).expect("读 restore.json");
        serde_json::from_str(&text).expect("restore.json 应是合法 JSON")
    }

    #[test]
    fn refuses_restore_without_confirmation() {
        let temp = TempDir::new("confirm");
        let agent = temp.path().join("agent");
        let data = app_data(&temp);
        write_file(&agent.join("settings.json"), "live");
        let id = "2026-01-01T00-00-00Z";
        craft_backup(
            &data,
            id,
            &manifest(vec![file_entry("settings.json", true, 0o600)]),
        );
        craft_content(&data, id, "settings.json", "backup");

        let failure = restore(&agent, &data, id, false).expect_err("未确认必须拒绝");

        assert_eq!(failure.code, "not_confirmed");
        assert_eq!(failure.safety_backup_id, None);
        assert_eq!(
            fs::read_to_string(agent.join("settings.json")).expect("读 live"),
            "live"
        );
        assert_eq!(backup_ids(&data).len(), 1, "拒绝时不得建还原前备份");
    }

    #[test]
    fn restores_file_content_and_mode() {
        let temp = TempDir::new("content");
        let agent = temp.path().join("agent");
        let data = app_data(&temp);
        write_file(&agent.join("settings.json"), "live");
        set_mode(&agent.join("settings.json"), 0o644);
        let id = "2026-01-01T00-00-00Z";
        craft_backup(
            &data,
            id,
            &manifest(vec![file_entry("settings.json", true, 0o600)]),
        );
        craft_content(&data, id, "settings.json", "backup");

        let report = restore(&agent, &data, id, true).expect("回滚应成功");

        assert_eq!(report.backup_id, id);
        assert_eq!(report.restored, 1);
        assert_eq!(report.removed, 0);
        assert_eq!(report.failed_at, None);
        assert_eq!(
            fs::read_to_string(agent.join("settings.json")).expect("读 live"),
            "backup"
        );
        assert_eq!(mode_of(&agent.join("settings.json")), 0o600);
    }

    #[test]
    fn safety_backup_holds_the_current_state() {
        let temp = TempDir::new("safety");
        let agent = temp.path().join("agent");
        let data = app_data(&temp);
        write_file(&agent.join("settings.json"), "live");
        set_mode(&agent.join("settings.json"), 0o640);
        let id = "2026-01-01T00-00-00Z";
        craft_backup(
            &data,
            id,
            &manifest(vec![file_entry("settings.json", true, 0o600)]),
        );
        craft_content(&data, id, "settings.json", "backup");

        let report = restore(&agent, &data, id, true).expect("回滚应成功");

        assert_eq!(backup_ids(&data).len(), 2, "回滚前应再备份一次");
        let safety_dir = data.join(BACKUPS_DIR_NAME).join(&report.safety_backup_id);
        assert!(safety_dir.is_dir(), "还原前备份目录应存在");
        assert_ne!(report.safety_backup_id, report.backup_id);
        let safety = read_restore(&safety_dir);
        assert_eq!(safety.fingerprint, "0123456789ab", "沿用被还原备份的指纹");
        assert_eq!(safety.entries.len(), 1);
        assert_eq!(safety.entries[0].path, "settings.json");
        assert!(safety.entries[0].existed);
        assert_eq!(safety.entries[0].mode, Some(0o640), "记下当前权限");
        assert_eq!(
            fs::read_to_string(safety_dir.join(BACKUP_FILES_DIR_NAME).join("settings.json"))
                .expect("读还原前备份"),
            "live",
            "还原前备份应包含当前内容"
        );
    }

    #[test]
    fn removes_paths_added_by_apply() {
        let temp = TempDir::new("removed");
        let agent = temp.path().join("agent");
        let data = app_data(&temp);
        write_file(&agent.join("skills/antd/SKILL.md"), "# 新增\n");
        let id = "2026-01-01T00-00-00Z";
        craft_backup(
            &data,
            id,
            &manifest(vec![file_entry("skills/antd/SKILL.md", false, 0o644)]),
        );

        let report = restore(&agent, &data, id, true).expect("回滚应成功");

        assert_eq!(report.restored, 0);
        assert_eq!(report.removed, 1);
        assert!(
            !agent.join("skills/antd/SKILL.md").exists(),
            "应用新增的文件应被删除"
        );
    }

    #[test]
    fn refuses_to_remove_a_directory_as_a_file() {
        let temp = TempDir::new("dir-remove");
        let agent = temp.path().join("agent");
        let data = app_data(&temp);
        write_file(&agent.join("skills/antd/SKILL.md"), "# 在目录里\n");
        let id = "2026-01-01T00-00-00Z";
        craft_backup(
            &data,
            id,
            &manifest(vec![file_entry("skills/antd", false, 0o644)]),
        );

        let failure = restore(&agent, &data, id, true).expect_err("目录不能当作新增文件删");

        assert_eq!(failure.code, "restore_failed", "{}", failure.message);
        assert_eq!(failure.completed, 0);
        assert!(
            failure.safety_backup_id.is_some(),
            "失败时也要报出还原前备份"
        );
        assert!(agent.join("skills/antd/SKILL.md").is_file(), "不递归删目录");
    }

    #[test]
    fn reports_progress_when_a_later_entry_fails() {
        let temp = TempDir::new("progress");
        let agent = temp.path().join("agent");
        let data = app_data(&temp);
        write_file(&agent.join("settings.json"), "live");
        // 记录说是文件，实际是目录：这一条必须失败，前面那条已经还原。
        write_file(&agent.join("skills/antd/SKILL.md"), "# 在目录里\n");
        let id = "2026-01-01T00-00-00Z";
        craft_backup(
            &data,
            id,
            &manifest(vec![
                file_entry("settings.json", true, 0o600),
                file_entry("skills/antd", true, 0o644),
            ]),
        );
        craft_content(&data, id, "settings.json", "backup");
        craft_content(&data, id, "skills/antd", "文件内容");

        let failure = restore(&agent, &data, id, true).expect_err("第二条必须失败");

        assert_eq!(failure.code, "restore_failed", "{}", failure.message);
        assert_eq!(failure.completed, 1, "第一条已完成");
        assert!(
            failure.message.contains("skills/antd"),
            "{}",
            failure.message
        );
        assert!(failure.safety_backup_id.is_some());
        assert_eq!(
            fs::read_to_string(agent.join("settings.json")).expect("读 live"),
            "backup",
            "已完成的部分留在原地"
        );
        assert!(
            agent.join("skills/antd/SKILL.md").is_file(),
            "失败停在原地：目录内容不动"
        );
    }

    #[test]
    fn rebuilds_links_from_the_recorded_target() {
        let temp = TempDir::new("link");
        let agent = temp.path().join("agent");
        let data = app_data(&temp);
        write_file(&agent.join("shared/target.md"), "共享\n");
        // 当前是真实文件，记录里是链接：先删掉再按 target 重建。
        write_file(&agent.join("prompts/linked.md"), "本地副本\n");
        let id = "2026-01-01T00-00-00Z";
        craft_backup(
            &data,
            id,
            &manifest(vec![RestoreEntry {
                path: "prompts/linked.md".to_string(),
                kind: RestoreKind::Symlink,
                existed: true,
                mode: None,
                link_target: Some("../shared/target.md".to_string()),
            }]),
        );

        let report = restore(&agent, &data, id, true).expect("回滚应成功");

        assert_eq!(report.restored, 1);
        let live = agent.join("prompts/linked.md");
        assert!(
            fs::symlink_metadata(&live)
                .expect("读链接信息")
                .file_type()
                .is_symlink(),
            "应重建为符号链接"
        );
        assert_eq!(
            fs::read_link(&live).expect("读链接目标"),
            Path::new("../shared/target.md")
        );
        assert_eq!(
            fs::read_to_string(&live).expect("读链接内容"),
            "共享\n",
            "链接应指回原目标"
        );
    }

    #[test]
    fn ensures_directory_entries() {
        let temp = TempDir::new("dir-entry");
        let agent = temp.path().join("agent");
        let data = app_data(&temp);
        fs::create_dir_all(&agent).expect("建宿主目录");
        let id = "2026-01-01T00-00-00Z";
        craft_backup(
            &data,
            id,
            &manifest(vec![RestoreEntry {
                path: "skills".to_string(),
                kind: RestoreKind::Dir,
                existed: true,
                mode: Some(0o755),
                link_target: None,
            }]),
        );

        let report = restore(&agent, &data, id, true).expect("回滚应成功");

        assert_eq!(report.restored, 1);
        assert!(agent.join("skills").is_dir(), "目录条目应补建");
    }

    #[test]
    fn rejects_paths_outside_the_managed_scope() {
        let temp = TempDir::new("scope");
        let agent = temp.path().join("agent");
        let data = app_data(&temp);
        write_file(&agent.join("settings.json"), "live");
        write_file(&agent.join("auth.json"), "{\"token\": \"sk-test\"}");
        write_file(&agent.join("sessions/old.json"), "{}");
        let id = "2026-01-01T00-00-00Z";

        for path in [
            "../escape.json",
            "/etc/passwd",
            "auth.json",
            "sessions/old.json",
            "skills/../../escape.json",
        ] {
            // 内容不必存在：越界路径在整份校验时就该被拒，根本读不到备份内容。
            craft_backup(&data, id, &manifest(vec![file_entry(path, true, 0o644)]));
            let failure = restore(&agent, &data, id, true)
                .err()
                .unwrap_or_else(|| panic!("{path} 必须被拒绝"));
            assert_eq!(
                failure.code, "path_out_of_bounds",
                "{path}：{}",
                failure.message
            );
            assert_eq!(failure.safety_backup_id, None, "拒绝时不得写任何文件");
        }

        assert_eq!(
            fs::read_to_string(agent.join("auth.json")).expect("读 auth.json"),
            "{\"token\": \"sk-test\"}"
        );
        assert!(agent.join("sessions/old.json").is_file());
        assert!(!agent.join("escape.json").exists());
    }

    #[test]
    fn skips_corrupt_backups_in_the_listing() {
        let temp = TempDir::new("corrupt");
        let data = app_data(&temp);
        craft_backup(&data, "2026-01-02T00-00-00Z", &manifest(vec![]));
        craft_backup(&data, "2026-01-03T00-00-00Z", &manifest(vec![]));
        craft_content(&data, "2026-01-02T00-00-00Z", "settings.json", "x");
        let broken = data.join(BACKUPS_DIR_NAME).join("2026-01-03T00-00-00Z");
        fs::write(broken.join(RESTORE_FILE_NAME), "{ 半截").expect("写坏 restore.json");

        let list = read_backups(&data);

        assert_eq!(list.backups.len(), 1, "损坏的备份被跳过");
        assert_eq!(list.backups[0].id, "2026-01-02T00-00-00Z");
        assert_eq!(list.warnings.len(), 1);
        assert!(
            list.warnings[0].contains("2026-01-03T00-00-00Z"),
            "{:?}",
            list.warnings
        );
        assert!(broken.join(RESTORE_FILE_NAME).is_file(), "损坏的备份不删");
    }

    #[test]
    fn lists_newest_first_and_caps_the_result() {
        let temp = TempDir::new("listing");
        let data = app_data(&temp);
        let total = BACKUP_RETENTION + 15;
        for day in 1..=total {
            let id = format!("2026-01-{day:02}T00-00-00Z");
            craft_backup(
                &data,
                &id,
                &manifest(vec![file_entry("settings.json", true, 0o644)]),
            );
        }

        let list = read_backups(&data);

        assert_eq!(list.backups.len(), BACKUP_LIST_LIMIT);
        assert_eq!(
            list.backups[0].id,
            format!("2026-01-{total:02}T00-00-00Z"),
            "最新在前"
        );
        assert_eq!(
            list.backups[BACKUP_LIST_LIMIT - 1].id,
            format!("2026-01-{:02}T00-00-00Z", total - BACKUP_LIST_LIMIT + 1)
        );
        assert_eq!(list.backups[0].entries, 1);
        assert!(list.warnings.is_empty());
    }

    #[test]
    fn apply_then_restore_returns_to_the_previous_state() {
        let temp = TempDir::new("apply-restore");
        let agent = temp.path().join("agent");
        let sync_root = temp.path().join("cloud");
        let data = app_data(&temp);
        write_file(&agent.join("settings.json"), "{\"theme\": \"dark\"}");
        write_file(&agent.join("AGENTS.md"), "# 指令\n");
        write_file(&agent.join("skills/antd/SKILL.md"), "# 技能\n");
        write_file(&agent.join("auth.json"), "{\"token\": \"sk-test\"}");
        fs::create_dir_all(&sync_root).expect("建同步目录");
        crate::snapshot::export(&agent, &sync_root, false).expect("导出夹具快照");
        let fingerprint = crate::plan::plan_snapshot(&agent, &sync_root, &FakeRunner)
            .expect("生成夹具计划")
            .manifest_info
            .fingerprint;

        // 改动本机：改内容与权限、删一个受管文件、留一个待删文件。
        write_file(&agent.join("settings.json"), "{\"theme\": \"light\"}");
        set_mode(&agent.join("settings.json"), 0o640);
        fs::remove_file(agent.join("skills/antd/SKILL.md")).expect("删 live 技能");
        write_file(&agent.join("prompts/leftover.md"), "留下");
        let expected = crate::plan::plan_snapshot(&agent, &sync_root, &FakeRunner)
            .expect("重算计划")
            .manifest_info
            .fingerprint;
        assert_eq!(
            expected, fingerprint,
            "改的是受管范围内的内容，指纹只看快照"
        );

        let applied = crate::apply::apply(
            &agent,
            &sync_root,
            &fingerprint,
            &["prompts/leftover.md".to_string()],
            &data,
            &FakeRunner,
        )
        .expect("应用应成功");
        assert_eq!(
            fs::read_to_string(agent.join("settings.json")).expect("读 settings"),
            "{\"theme\": \"dark\"}"
        );
        assert!(
            agent.join("skills/antd/SKILL.md").is_file(),
            "快照文件已落地"
        );
        assert!(!agent.join("prompts/leftover.md").exists());

        let report = restore(&agent, &data, &applied.backup_id, true).expect("回滚应成功");

        assert_eq!(report.restored, 2, "settings.json 与 leftover.md 写回");
        assert_eq!(report.removed, 1, "应用新增的 SKILL.md 被删");
        assert_eq!(
            fs::read_to_string(agent.join("prompts/leftover.md")).expect("读 leftover"),
            "留下",
            "被应用删掉的文件回到应用前的样子"
        );
        assert_eq!(
            fs::read_to_string(agent.join("settings.json")).expect("读 settings"),
            "{\"theme\": \"light\"}",
            "回滚后内容与回滚前一致"
        );
        assert_eq!(mode_of(&agent.join("settings.json")), 0o640, "权限也一致");
        assert!(
            !agent.join("skills/antd/SKILL.md").exists(),
            "应用新增的文件被删"
        );
        assert_eq!(
            fs::read_to_string(agent.join("auth.json")).expect("读 auth.json"),
            "{\"token\": \"sk-test\"}",
            "范围外文件从未被碰"
        );
    }

    #[test]
    fn unknown_backup_is_rejected() {
        let temp = TempDir::new("unknown");
        let agent = temp.path().join("agent");
        let data = app_data(&temp);
        fs::create_dir_all(&agent).expect("建宿主目录");

        let failure =
            restore(&agent, &data, "2026-01-01T00-00-00Z", true).expect_err("不存在的备份必须拒绝");
        assert_eq!(failure.code, "backup_missing");

        let failure = restore(&agent, &data, "../agent", true).expect_err("越界目录名必须拒绝");
        assert_eq!(failure.code, "backup_missing");
        assert!(backup_ids(&data).is_empty(), "拒绝时不得建还原前备份");
    }

    #[test]
    fn symlink_ancestors_are_never_written_through() {
        let temp = TempDir::new("write-through");
        let agent = temp.path().join("agent");
        let data = app_data(&temp);
        // 目标路径本身是链接：先 unlink 再写真实文件，不写穿到链接目标。
        write_file(&agent.join("shared/target.md"), "共享\n");
        fs::create_dir_all(agent.join("prompts")).expect("建 prompts");
        symlink("../shared/target.md", agent.join("prompts/linked.md")).expect("建夹具链接");
        let id = "2026-01-01T00-00-00Z";
        craft_backup(
            &data,
            id,
            &manifest(vec![file_entry("prompts/linked.md", true, 0o600)]),
        );
        craft_content(&data, id, "prompts/linked.md", "快照内容");

        restore(&agent, &data, id, true).expect("回滚应成功");

        assert_eq!(
            fs::read_to_string(agent.join("shared/target.md")).expect("读链接目标"),
            "共享\n",
            "链接目标不得被写穿"
        );
        assert_eq!(
            fs::read_to_string(agent.join("prompts/linked.md")).expect("读还原的文件"),
            "快照内容"
        );
    }
}

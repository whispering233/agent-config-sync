//! 快照包的取用：从存储根取包、解到本机暂存目录，以及计划与应用允许的根判定。
//!
//! 规则来源：`docs/design/10-snapshot-sync.md` 第 2、6 节，`docs/design/40-webdav.md` 第 6 节。
//! 这里不决定快照内容，也不碰 live：解包产物与同步根同构（`manifest.json` + `snapshot/`），
//! 直接交给 `plan` / `apply`，它们不认识「包」这个概念。

use crate::credentials;
use crate::package;
use crate::platform;
use crate::remote_state::{self, RemoteState};
use crate::snapshot::{self, Manifest};
use crate::store::{PackageStore, StoreError};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

/// 暂存目录名：位于应用数据目录下，与 `backups/` 同级，不进云盘也不进快照。
pub const STAGING_DIR_NAME: &str = "staging";
/// 暂存目录权限：快照可能含明文密钥，只允许本用户读写。
const STAGING_DIR_MODE: u32 = 0o700;

/// 一份已取到本机的包。`staging_root` 顶替同步根，交给计划与应用复用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedPackage {
    pub package_name: String,
    /// 包文件字节数。
    pub size: u64,
    /// 包内快照文件数（不含 manifest）。
    pub file_count: usize,
    /// manifest 里的完整指纹。
    pub fingerprint: String,
    /// manifest 里的 RFC 3339 UTC 创建时间。
    pub created_at: String,
    pub staging_root: PathBuf,
    /// 取包时看到的可解析包名集合：成功后直接记进本机状态，不必再列一次远端。
    pub packages: Vec<String>,
}

/// 取包失败。码取自 `10-snapshot-sync.md` 第 9 节的失败码表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadFailure {
    pub code: &'static str,
    pub message: String,
}

impl DownloadFailure {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DownloadFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// 一次下载的结论：包已解到本机，外加本机记录里上一次看到的远端时间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadOutcome {
    pub staged: StagedPackage,
    pub previous_seen_at: Option<String>,
}

/// 列举结果里的一份包：时间与指纹都取自包名，为此不下载包本身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedPackage {
    pub name: String,
    pub size: u64,
    /// RFC 3339 UTC，由包名里的时间戳换算。
    pub created_at: String,
    /// 包名里的指纹前 8 位：只用于显示与人工核对。
    pub fingerprint8: String,
    /// 名字最大的一份：UI 的默认选中项。
    pub is_newest: bool,
}

/// 一次列举：包按名字倒序（最新在前），无法识别的只留名字。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageListing {
    pub packages: Vec<ListedPackage>,
    pub unrecognized: Vec<String>,
    /// 本机记录里上一次看到的远端时间；没有记录或本地文件夹模式为 `None`。
    pub seen_at: Option<String>,
    /// 云端有、本机记录里没有的包数；本地文件夹模式没有记录可比，恒为 0。
    pub unseen_count: usize,
}

/// 一次清理上传残留的结论。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupOutcome {
    /// 删掉的项。
    pub deleted: Vec<String>,
    /// 删不掉的项：单项失败不影响其它项，也不把整次清理判成失败。
    pub failed: Vec<CleanupFailure>,
    /// 没动的无法识别文件：它们不在清理许可范围内，只列给用户看。
    pub skipped: Vec<String>,
}

/// 删不掉的一项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupFailure {
    pub name: String,
    pub reason: String,
}

fn store_failure(error: StoreError) -> DownloadFailure {
    DownloadFailure::new(error.code, error.message)
}

fn package_failure(error: package::PackageError) -> DownloadFailure {
    DownloadFailure::new(error.code, error.message)
}

/// 计划与应用允许的根：配置里的同步根，或应用数据目录下暂存目录的**子目录**。
///
/// `requested` 来自前端，不能当普通路径用：暂存目录不在同步根里，两种来源必须在这里
/// 一次性列清楚。比较前先 canonicalize，符号链接绕不过去。
pub fn resolve_root(
    configured_sync_root: Option<&Path>,
    app_data: &Path,
    requested: &str,
) -> Result<PathBuf, String> {
    let requested = requested.trim();
    if requested.is_empty() {
        return Err("指定的快照目录为空".to_string());
    }
    let candidate = fs::canonicalize(requested)
        .map_err(|error| format!("无法访问指定的快照目录 {requested}：{error}"))?;

    let in_sync_root = configured_sync_root
        .and_then(|root| fs::canonicalize(root).ok())
        .is_some_and(|root| candidate == root);
    // 暂存根自身不算：它下面不会有 manifest.json，只可能白跑一次检查。
    let in_staging = fs::canonicalize(app_data.join(STAGING_DIR_NAME))
        .ok()
        .is_some_and(|root| candidate.starts_with(&root) && candidate != root);

    if in_sync_root || in_staging {
        Ok(candidate)
    } else {
        Err(format!(
            "{requested} 既不是同步文件夹，也不在本机暂存目录里；拒绝在它之上生成计划"
        ))
    }
}

/// 列举存储里的包，按名字倒序，并把名字最大的那份标成「最新」。
///
/// 只读：不发 `GET`，不碰本机记录。空存储根返回空列表而不是错误——「还没有包」由 UI 说。
pub fn list_packages(store: &dyn PackageStore) -> Result<PackageListing, DownloadFailure> {
    let listing = store.list().map_err(store_failure)?;
    let mut packages: Vec<ListedPackage> = listing
        .packages
        .into_iter()
        // 存储层已按包名闸门筛过；这里再解析一次只为取指纹前 8 位，免得 `StoreEntry`
        // 多存一份与名字重复的字段。
        .filter_map(|entry| {
            let parsed = package::parse_name(&entry.name)?;
            Some(ListedPackage {
                created_at: snapshot::format_timestamp(entry.created_at),
                name: entry.name,
                size: entry.size,
                fingerprint8: parsed.fingerprint8,
                is_newest: false,
            })
        })
        .collect();
    packages.sort_by(|left, right| right.name.cmp(&left.name));
    if let Some(newest) = packages.first_mut() {
        newest.is_newest = true;
    }

    Ok(PackageListing {
        packages,
        unrecognized: listing.unrecognized,
        seen_at: None,
        unseen_count: 0,
    })
}

/// WebDAV 列举：HTTPS 门槛 → 列举 → 与本机记录比对出「云端有、本机没见过」的包数。
///
/// 只发 `PROPFIND`：不改远端，也**不写** `remote-state.json`——只读命令一写状态，
/// 「本机未见」的提示就会被用户随手一看抹掉。
pub fn list_from_webdav(
    store: &dyn PackageStore,
    endpoint: &str,
    app_data: &Path,
) -> Result<PackageListing, DownloadFailure> {
    // 门槛放在最前：非 HTTPS 的远端不只可能被读走凭据，还可能被塞进任意快照包，
    // 而落地语义是「快照为准」，所以一个请求都不发。
    if !credentials::is_secure_url(endpoint) {
        return Err(DownloadFailure::new(
            "remote_insecure",
            "WebDAV 地址必须是 HTTPS（本机回环地址例外），否则拒绝列举远端快照",
        ));
    }

    let mut listing = list_packages(store)?;
    let state = remote_state::load(app_data);
    listing.unseen_count = listing
        .packages
        .iter()
        .filter(|entry| !state.packages.contains(&entry.name))
        .count();
    // 空记录读出来的 `seenAt` 是空串：没有上一次就不该报一个时间。
    listing.seen_at = (!state.seen_at.is_empty()).then_some(state.seen_at);

    Ok(listing)
}

/// 清理上传残留：重新列举一次，只删名字匹配本应用临时名的项。
///
/// 不接收待删名单：候选每次都从新列举里算，因此重复调用是幂等的（第二遍没有候选），
/// 也不会因为界面上的列表已过期而删到别人的文件。合法包、`manifest.json`、`snapshot/`
/// 与任何外来文件都不在候选里；目录不删（`store.delete` 只删文件）。
pub fn cleanup_temp_files(store: &dyn PackageStore) -> Result<CleanupOutcome, DownloadFailure> {
    let listing = store.list().map_err(store_failure)?;
    let mut outcome = CleanupOutcome::default();
    for name in listing.unrecognized {
        if !package::is_own_temp_name(&name) {
            outcome.skipped.push(name);
            continue;
        }
        match store.delete(&name) {
            Ok(()) => outcome.deleted.push(name),
            Err(error) => outcome.failed.push(CleanupFailure {
                name,
                reason: error.message,
            }),
        }
    }

    Ok(outcome)
}

/// WebDAV 模式的清理：HTTPS 门槛 → 清理上传残留。
///
/// 门槛放在列举之前：非 HTTPS 远端连一个 `PROPFIND` 都不该发，更不用说 `DELETE`。
pub fn cleanup_from_webdav(
    store: &dyn PackageStore,
    endpoint: &str,
) -> Result<CleanupOutcome, DownloadFailure> {
    if !credentials::is_secure_url(endpoint) {
        return Err(DownloadFailure::new(
            "remote_insecure",
            "WebDAV 地址必须是 HTTPS（本机回环地址例外），否则拒绝清理远端",
        ));
    }

    cleanup_temp_files(store)
}

/// 取包到本机暂存：列举 → 选包（缺省取名字最大的一份）→ 取 → 解到 `staging/<包名>/` → 只留这一份。
///
/// 本地文件夹与 WebDAV 共用这一条链路，只换 [`PackageStore`] 实现。选中的包不在列表里
/// 就是「刚被别人删了」：宁可报错也不退回另一份，否则用户会应用一份没选过的快照。
pub fn stage_package(
    store: &dyn PackageStore,
    app_data: &Path,
    requested: Option<&str>,
) -> Result<StagedPackage, DownloadFailure> {
    let listing = store.list().map_err(store_failure)?;
    let chosen = match requested {
        Some(name) => listing
            .packages
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| {
                DownloadFailure::new(
                    "package_not_found",
                    format!("选中的快照包已不在存储里：{name}"),
                )
            })?,
        None => listing
            .packages
            .iter()
            .max_by(|left, right| left.name.cmp(&right.name))
            .ok_or_else(|| {
                DownloadFailure::new(
                    "remote_no_packages",
                    "还没有快照包：先导出一份，或等另一台机器的包到达",
                )
            })?,
    };

    // 体积先按列举值判：超限的包不该被整份取到内存里才发现。
    let limit = package::Limits::default().package_bytes;
    if chosen.size > limit {
        return Err(DownloadFailure::new(
            "package_too_large",
            format!("包体积 {} 字节，超过上限 {limit} 字节", chosen.size),
        ));
    }

    let root = app_data.join(STAGING_DIR_NAME);
    platform::create_dir_all_mode(&root, STAGING_DIR_MODE).map_err(|error| {
        DownloadFailure::new(
            "no_write_permission",
            format!("无法创建暂存目录 {}：{error}", root.display()),
        )
    })?;
    let target = root.join(&chosen.name);
    // 重新取同一份包时旧暂存还在，而解包拒绝已存在的落点：先整份删掉再解。
    if target.symlink_metadata().is_ok() {
        remove_path(&target)?;
    }

    let bytes = store
        .get(&chosen.name)
        .map_err(store_failure)?
        .ok_or_else(|| {
            DownloadFailure::new(
                "package_not_found",
                format!("取包时存储里已经没有这份包：{}", chosen.name),
            )
        })?;
    // `unpack` 只认路径：先把包落到暂存根下的临时名，解完立刻删。
    let archive = root.join(format!("{}.incoming", chosen.name));
    fs::write(&archive, &bytes).map_err(|error| {
        DownloadFailure::new(
            "no_write_permission",
            format!("无法写入临时包 {}：{error}", archive.display()),
        )
    })?;
    let unpacked = package::unpack(&archive, &target);
    let _ = fs::remove_file(&archive);
    unpacked.map_err(package_failure)?;
    // 暂存常驻只保留当前这一份：更早的解包结果已经不可能被应用。
    prune_staging(app_data, &target);

    let manifest = match read_staged_manifest(&target) {
        Ok(manifest) => manifest,
        // 不认识 manifest 的暂存交给计划也只会白跑：删掉，别让它冒充一份可用快照。
        Err(failure) => {
            let _ = fs::remove_dir_all(&target);
            return Err(failure);
        }
    };
    Ok(StagedPackage {
        package_name: chosen.name.clone(),
        size: bytes.len() as u64,
        file_count: manifest.files.len(),
        fingerprint: manifest.fingerprint,
        created_at: manifest.created_at,
        staging_root: target,
        packages: listing
            .packages
            .into_iter()
            .map(|entry| entry.name)
            .collect(),
    })
}

/// WebDAV 下载全流程：HTTPS 门槛 → 取包到暂存（列举、选包、体积、取、解包）→ 记录本机状态。
///
/// 下载不改动远端：不清理、不删除、不写任何远端文件。
pub fn download_from_webdav(
    store: &dyn PackageStore,
    endpoint: &str,
    app_data: &Path,
    requested: Option<&str>,
) -> Result<DownloadOutcome, DownloadFailure> {
    // 门槛放在最前：非 HTTPS 的远端不只可能被读走凭据，还可能被塞进任意快照包，
    // 而落地语义是「快照为准」，所以一个请求都不发。
    if !credentials::is_secure_url(endpoint) {
        return Err(DownloadFailure::new(
            "remote_insecure",
            "WebDAV 地址必须是 HTTPS（本机回环地址例外），否则拒绝下载",
        ));
    }

    let state = remote_state::load(app_data);
    let staged = stage_package(store, app_data, requested)?;

    let next = RemoteState {
        seen_at: snapshot::format_timestamp(snapshot::now_seconds()),
        packages: staged.packages.clone(),
        // 下载不改写「上次上传的指纹」：它只由上传决定。
        last_uploaded_fingerprint: state.last_uploaded_fingerprint,
    };
    remote_state::save(app_data, &next).map_err(|error| {
        DownloadFailure::new(
            "no_write_permission",
            format!("快照包已取到本机，但无法记录远端状态：{error}"),
        )
    })?;

    Ok(DownloadOutcome {
        staged,
        // 空记录读出来的 `seenAt` 是空串：没有上一次就不该报一个时间。
        previous_seen_at: (!state.seen_at.is_empty()).then_some(state.seen_at),
    })
}

/// 解出来的 manifest：给 UI 报创建时间与文件数，也顺带在进计划前挡掉不认识的格式版本。
fn read_staged_manifest(staging_root: &Path) -> Result<Manifest, DownloadFailure> {
    let path = staging_root.join(snapshot::MANIFEST_FILE_NAME);
    let bytes = fs::read(&path).map_err(|error| {
        DownloadFailure::new(
            "package_corrupt",
            format!("无法读取解包后的 manifest {}：{error}", path.display()),
        )
    })?;

    snapshot::parse_manifest(&bytes)
        .map_err(|failure| DownloadFailure::new(failure.code, failure.message))
}

/// 暂存里的旧包可能是目录，也可能是同名文件：两种都删掉。
fn remove_path(path: &Path) -> Result<(), DownloadFailure> {
    let metadata = path.symlink_metadata().map_err(|error| {
        DownloadFailure::new(
            "no_write_permission",
            format!("无法检查旧暂存 {}：{error}", path.display()),
        )
    })?;
    let result = if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };

    result.map_err(|error| {
        DownloadFailure::new(
            "no_write_permission",
            format!("无法清理旧暂存 {}：{error}", path.display()),
        )
    })
}

/// 只保留本次的暂存目录：旧暂存对应的是上一份远端快照，留着只会让人误以为还能用。
fn prune_staging(app_data: &Path, keep: &Path) {
    let Ok(entries) = fs::read_dir(app_data.join(STAGING_DIR_NAME)) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep {
            continue;
        }
        let _ = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{
        fingerprint, ManifestFile, Requirements, SourceInfo, FORMAT_VERSION, PROFILE_ID,
    };
    use crate::store::{LocalStore, WebdavStore};
    use crate::webdav::{
        DavError, DavRequest, DavResponse, DavTransport, ReqwestTransport, WebdavClient,
    };
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, BTreeSet};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    const BASE: &str = "https://dav.example.com/dav";

    /// 临时目录夹具：Drop 时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-transfer-{}-{}-{}",
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

    fn manifest_with(files: Vec<ManifestFile>) -> Manifest {
        Manifest {
            format_version: FORMAT_VERSION,
            profile_id: PROFILE_ID.to_string(),
            created_at: "2026-11-22T10:52:30Z".to_string(),
            fingerprint: fingerprint(FORMAT_VERSION, &files),
            source: SourceInfo {
                app: "0.1.0".to_string(),
                os: "linux".to_string(),
                arch: "x64".to_string(),
                pi: None,
                node: None,
            },
            requirements: Requirements {
                node: ">=22.19.0".to_string(),
                pi: ">=1.5.0".to_string(),
            },
            files,
            symlinks: Vec::new(),
            required_packages: Vec::new(),
            plaintext_secrets: false,
        }
    }

    /// 走真实的打包路径造一个包放进存储根：内容与 manifest 都对得上。
    fn write_package(store_root: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let temp = TempDir::new("source");
        let source = temp.path().join("snapshot");
        let mut entries = Vec::new();
        for (relative, content) in files {
            let path = source.join(relative);
            fs::create_dir_all(path.parent().expect("文件必须有父目录")).unwrap();
            fs::write(&path, content).unwrap();
            entries.push(ManifestFile {
                path: format!("snapshot/{relative}"),
                sha256: snapshot::sha256_hex(content.as_bytes()),
                size: content.len() as u64,
                kind: "text".to_string(),
            });
        }

        fs::create_dir_all(store_root).unwrap();
        let archive = store_root.join(name);
        let bytes = serde_json::to_vec_pretty(&manifest_with(entries)).unwrap();
        package::pack(&archive, &bytes, &source).unwrap();

        archive
    }

    fn staging_entries(app_data: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(app_data.join(STAGING_DIR_NAME))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn stage_package_unpacks_the_newest_package_by_default() {
        let temp = TempDir::new("stage-newest");
        let store_root = temp.path().join("cloud");
        let app_data = temp.path().join("data");
        let older = "20261121-091210-7f3e9c01.zip";
        let newest = "20261122-105230-deadbeef.zip";
        write_package(&store_root, older, &[("settings.json", "{\"v\":1}")]);
        write_package(&store_root, newest, &[("settings.json", "{\"v\":2}")]);
        // 无法识别的文件不参与「最新」，也不该被当成包。
        fs::write(store_root.join("notes.txt"), b"keep me").unwrap();

        let store = LocalStore::new(store_root.clone());
        let staged = stage_package(&store, &app_data, None).expect("取包应成功");

        assert_eq!(staged.package_name, newest);
        assert_eq!(staged.created_at, "2026-11-22T10:52:30Z");
        assert_eq!(staged.file_count, 1);
        assert_eq!(
            staged.size,
            fs::metadata(store_root.join(newest)).unwrap().len()
        );
        assert_eq!(staged.packages, vec![older.to_string(), newest.to_string()]);
        assert_eq!(
            fs::read_to_string(staged.staging_root.join("snapshot/settings.json")).unwrap(),
            "{\"v\":2}"
        );
        assert_eq!(staging_entries(&app_data), vec![newest.to_string()]);
    }

    #[test]
    fn stage_package_honours_the_named_package_and_keeps_only_it() {
        let temp = TempDir::new("stage-named");
        let store_root = temp.path().join("cloud");
        let app_data = temp.path().join("data");
        let older = "20261121-091210-7f3e9c01.zip";
        let newest = "20261122-105230-deadbeef.zip";
        write_package(&store_root, older, &[("settings.json", "{\"v\":1}")]);
        write_package(&store_root, newest, &[("settings.json", "{\"v\":2}")]);
        let store = LocalStore::new(store_root);

        stage_package(&store, &app_data, None).expect("先取最新一份");
        let staged = stage_package(&store, &app_data, Some(older)).expect("指定旧包");
        assert_eq!(staged.package_name, older);
        assert_eq!(staging_entries(&app_data), vec![older.to_string()]);

        // 同一份包再取一次：旧暂存要先被换掉，解包才不会被「已存在」拦住。
        let again = stage_package(&store, &app_data, Some(older)).expect("重复取同一份");
        assert_eq!(again.staging_root, staged.staging_root);
        assert_eq!(staging_entries(&app_data), vec![older.to_string()]);
    }

    #[test]
    fn stage_package_reports_empty_store_and_vanished_package() {
        let temp = TempDir::new("stage-missing");
        let store_root = temp.path().join("cloud");
        let app_data = temp.path().join("data");
        let store = LocalStore::new(store_root.clone());

        let error = stage_package(&store, &app_data, None).expect_err("空存储根必须报错");
        assert_eq!(error.code, "remote_no_packages");
        assert!(error.message.contains("还没有快照包"), "{}", error.message);

        write_package(
            &store_root,
            "20261122-105230-deadbeef.zip",
            &[("settings.json", "{}")],
        );
        let error = stage_package(&store, &app_data, Some("20261123-000000-aaaaaaaa.zip"))
            .expect_err("选中的包不存在必须报错");
        assert_eq!(error.code, "package_not_found");
        assert!(error.message.contains("已不在存储里"), "{}", error.message);
        assert!(staging_entries(&app_data).is_empty(), "报错时不留暂存");
    }

    #[test]
    fn corrupt_package_leaves_staging_empty() {
        let temp = TempDir::new("stage-corrupt");
        let store_root = temp.path().join("cloud");
        let app_data = temp.path().join("data");
        let name = "20261122-105230-deadbeef.zip";
        fs::create_dir_all(&store_root).unwrap();
        // 名字合法、内容不是 zip：云盘没传完的残片就是这个样子。
        fs::write(store_root.join(name), b"not a zip at all").unwrap();

        let store = LocalStore::new(store_root);
        let error = stage_package(&store, &app_data, None).expect_err("坏包必须报错");

        assert_eq!(error.code, "package_corrupt", "{}", error.message);
        assert!(
            staging_entries(&app_data).is_empty(),
            "失败不污染暂存：{}",
            error.message
        );
    }

    /// 按给定内容打一个包，返回包字节：远端只认字节。
    fn package_bytes(name: &str, files: &[(&str, &str)]) -> Vec<u8> {
        let temp = TempDir::new("pack");
        let archive = write_package(&temp.path().join("store"), name, files);
        fs::read(archive).unwrap()
    }

    /// manifest 里写的哈希与包内内容不符。
    fn hash_mismatch_package(name: &str) -> Vec<u8> {
        let temp = TempDir::new("mismatch");
        let source = temp.path().join("snapshot");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("settings.json"), b"{\"v\":1}").unwrap();
        let entries = vec![ManifestFile {
            path: "snapshot/settings.json".to_string(),
            sha256: snapshot::sha256_hex(b"{\"v\":2}"),
            size: 7,
            kind: "text".to_string(),
        }];
        let archive = temp.path().join(name);
        let manifest = serde_json::to_vec_pretty(&manifest_with(entries)).unwrap();
        package::pack(&archive, &manifest, &source).unwrap();
        fs::read(archive).unwrap()
    }

    /// 条目名越出 `snapshot/`：不能用 `package::pack` 造，只能直接写 zip。
    fn unsafe_entry_package() -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            use std::io::Write;
            let mut writer = zip::ZipWriter::new(&mut buffer);
            for (entry, content) in [
                ("manifest.json", &b"{}"[..]),
                ("snapshot/../evil.txt", &b"x"[..]),
            ] {
                writer
                    .start_file(entry, zip::write::SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(content).unwrap();
            }
            writer.finish().unwrap();
        }
        buffer.into_inner()
    }

    /// 假 WebDAV 服务端：只实现下载与清理链路用到的动词。
    struct DavFiles {
        requests: RefCell<Vec<DavRequest>>,
        files: RefCell<BTreeMap<String, Vec<u8>>>,
        /// 列举时报的体积：让超大包不必真塞 65 MB。
        sizes: RefCell<BTreeMap<String, u64>>,
        /// 删除时返回 500 的名字：用来验「逐项失败不影响其它项」。
        failing_deletes: RefCell<BTreeSet<String>>,
        root_exists: Cell<bool>,
    }

    #[derive(Clone)]
    struct FakeDav {
        state: Rc<DavFiles>,
    }

    impl FakeDav {
        fn new() -> Self {
            Self {
                state: Rc::new(DavFiles {
                    requests: RefCell::new(Vec::new()),
                    files: RefCell::new(BTreeMap::new()),
                    sizes: RefCell::new(BTreeMap::new()),
                    failing_deletes: RefCell::new(BTreeSet::new()),
                    root_exists: Cell::new(false),
                }),
            }
        }

        fn seed(&self, name: &str, body: &[u8]) {
            self.state.root_exists.set(true);
            self.state
                .files
                .borrow_mut()
                .insert(name.to_string(), body.to_vec());
        }

        fn claim_size(&self, name: &str, size: u64) {
            self.state.sizes.borrow_mut().insert(name.to_string(), size);
        }

        fn fail_delete(&self, name: &str) {
            self.state
                .failing_deletes
                .borrow_mut()
                .insert(name.to_string());
        }

        fn methods(&self) -> Vec<String> {
            self.state
                .requests
                .borrow()
                .iter()
                .map(|request| request.method.clone())
                .collect()
        }

        fn file_names(&self) -> Vec<String> {
            self.state.files.borrow().keys().cloned().collect()
        }
    }

    /// 工作根下的相对名字；工作根自身为空串。
    fn rel_of(url: &str) -> String {
        url.split_once("/PiAgentSync")
            .map(|(_, rest)| rest.trim_matches('/').to_string())
            .unwrap_or_default()
    }

    impl DavTransport for FakeDav {
        fn send(&self, request: DavRequest) -> Result<DavResponse, DavError> {
            self.state.requests.borrow_mut().push(request.clone());
            let rel = rel_of(&request.url);

            match request.method.as_str() {
                "PROPFIND" => {
                    if !self.state.root_exists.get() {
                        return Ok(DavResponse {
                            status: 404,
                            headers: Vec::new(),
                            body: Vec::new(),
                        });
                    }
                    let mut body = String::from(
                        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
                         <d:multistatus xmlns:d=\"DAV:\">\
                         <d:response><d:href>/dav/PiAgentSync/</d:href>\
                         <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype>\
                         </d:prop></d:propstat></d:response>",
                    );
                    for (name, bytes) in self.state.files.borrow().iter() {
                        let size = self
                            .state
                            .sizes
                            .borrow()
                            .get(name)
                            .copied()
                            .unwrap_or(bytes.len() as u64);
                        body.push_str(&format!(
                            "<d:response><d:href>/dav/PiAgentSync/{name}</d:href>\
                             <d:propstat><d:prop><d:resourcetype/>\
                             <d:getcontentlength>{size}</d:getcontentlength>\
                             </d:prop></d:propstat></d:response>"
                        ));
                    }
                    body.push_str("</d:multistatus>");
                    Ok(DavResponse {
                        status: 207,
                        headers: Vec::new(),
                        body: body.into_bytes(),
                    })
                }
                "GET" => {
                    let body = self.state.files.borrow().get(&rel).cloned();
                    Ok(DavResponse {
                        status: if body.is_some() { 200 } else { 404 },
                        headers: Vec::new(),
                        body: body.unwrap_or_default(),
                    })
                }
                "DELETE" => {
                    if self.state.failing_deletes.borrow().contains(&rel) {
                        return Ok(DavResponse {
                            status: 500,
                            headers: Vec::new(),
                            body: Vec::new(),
                        });
                    }
                    let removed = self.state.files.borrow_mut().remove(&rel);
                    Ok(DavResponse {
                        status: if removed.is_some() { 204 } else { 404 },
                        headers: Vec::new(),
                        body: Vec::new(),
                    })
                }
                other => Err(DavError::Other(format!("测试未实现的方法：{other}"))),
            }
        }
    }

    /// 一套下载夹具：应用数据目录 + 远端。
    struct Fixture {
        temp: TempDir,
        remote: FakeDav,
    }

    const PKG_OLD: &str = "20261121-091210-7f3e9c01.zip";
    const PKG_NEW: &str = "20261122-105230-deadbeef.zip";

    impl Fixture {
        fn new(label: &str) -> Self {
            Self {
                temp: TempDir::new(label),
                remote: FakeDav::new(),
            }
        }

        fn app_data(&self) -> PathBuf {
            self.temp.path().join("data")
        }

        fn agent(&self) -> PathBuf {
            self.temp.path().join("agent")
        }

        fn seed(&self, name: &str, files: &[(&str, &str)]) {
            self.remote.seed(name, &package_bytes(name, files));
        }

        fn download(
            &self,
            endpoint: &str,
            requested: Option<&str>,
        ) -> Result<DownloadOutcome, DownloadFailure> {
            let client = WebdavClient::new(self.remote.clone(), BASE);
            let store = WebdavStore::new(&client);
            download_from_webdav(&store, endpoint, &self.app_data(), requested)
        }

        fn list(&self, endpoint: &str) -> Result<PackageListing, DownloadFailure> {
            let client = WebdavClient::new(self.remote.clone(), BASE);
            let store = WebdavStore::new(&client);
            list_from_webdav(&store, endpoint, &self.app_data())
        }

        fn cleanup(&self, endpoint: &str) -> Result<CleanupOutcome, DownloadFailure> {
            let client = WebdavClient::new(self.remote.clone(), BASE);
            cleanup_from_webdav(&WebdavStore::new(&client), endpoint)
        }

        fn state(&self) -> RemoteState {
            remote_state::load(&self.app_data())
        }
    }

    #[test]
    fn download_refuses_a_non_https_endpoint_without_sending_a_request() {
        let fixture = Fixture::new("download-insecure");
        fixture.seed(PKG_NEW, &[("settings.json", "{}")]);

        let failure = fixture
            .download("http://dav.example.com/dav", None)
            .expect_err("非 HTTPS 必须拒绝");

        assert_eq!(failure.code, "remote_insecure");
        assert!(
            fixture.remote.methods().is_empty(),
            "门槛在最前：一个请求都不该发出"
        );
        assert!(staging_entries(&fixture.app_data()).is_empty());
        assert!(!fixture
            .app_data()
            .join(remote_state::REMOTE_STATE_FILE_NAME)
            .exists());
    }

    #[test]
    fn download_allows_loopback_http_endpoints() {
        for (index, endpoint) in ["http://127.0.0.1:8080/dav", "http://localhost/dav"]
            .into_iter()
            .enumerate()
        {
            let fixture = Fixture::new(&format!("download-loopback-{index}"));
            fixture.seed(PKG_NEW, &[("settings.json", "{}")]);

            let outcome = fixture.download(endpoint, None).expect("回环地址应放行");

            assert_eq!(outcome.staged.package_name, PKG_NEW, "{endpoint}");
        }
    }

    #[test]
    fn download_reports_an_empty_remote_and_a_vanished_name() {
        let fixture = Fixture::new("download-missing");

        let failure = fixture.download(BASE, None).expect_err("远端为空必须报错");
        assert_eq!(failure.code, "remote_no_packages", "{}", failure.message);
        assert_eq!(fixture.remote.methods(), vec!["PROPFIND"], "空远端只列一次");
        assert!(staging_entries(&fixture.app_data()).is_empty());

        fixture.seed(PKG_OLD, &[("settings.json", "{}")]);
        let failure = fixture
            .download(BASE, Some(PKG_NEW))
            .expect_err("选中的包不在列表里");
        assert_eq!(failure.code, "package_not_found", "{}", failure.message);
        assert!(
            staging_entries(&fixture.app_data()).is_empty(),
            "报错时不留暂存"
        );
    }

    #[test]
    fn oversized_package_is_rejected_before_the_get() {
        let fixture = Fixture::new("download-too-large");
        fixture.seed(PKG_NEW, &[("settings.json", "{}")]);
        fixture.remote.claim_size(PKG_NEW, 64 * 1024 * 1024 + 1);

        let failure = fixture.download(BASE, None).expect_err("超限必须拒绝");

        assert_eq!(failure.code, "package_too_large", "{}", failure.message);
        assert_eq!(
            fixture.remote.methods(),
            vec!["PROPFIND"],
            "体积先按列举值判，不发 GET"
        );
        assert!(staging_entries(&fixture.app_data()).is_empty());
    }

    #[test]
    fn download_unpacks_the_newest_package_and_leaves_the_remote_alone() {
        let fixture = Fixture::new("download-newest");
        fixture.seed(PKG_OLD, &[("settings.json", "{\"v\":1}")]);
        fixture.seed(
            PKG_NEW,
            &[("settings.json", "{\"v\":2}"), ("AGENTS.md", "# 指令\n")],
        );

        let outcome = fixture.download(BASE, None).expect("取包应成功");

        assert_eq!(outcome.staged.package_name, PKG_NEW);
        assert_eq!(outcome.staged.file_count, 2);
        assert_eq!(outcome.staged.created_at, "2026-11-22T10:52:30Z");
        assert!(outcome.staged.size > 0);
        assert_eq!(outcome.staged.fingerprint.len(), 12);
        assert!(outcome.staged.staging_root.join("manifest.json").is_file());
        assert_eq!(
            fs::read_to_string(outcome.staged.staging_root.join("snapshot/settings.json")).unwrap(),
            "{\"v\":2}"
        );
        assert_eq!(
            staging_entries(&fixture.app_data()),
            vec![PKG_NEW.to_string()],
            "暂存只留当前这一份"
        );

        // 下载不改动远端：只有列举与取包两个动词，包一份不少。
        assert_eq!(fixture.remote.methods(), vec!["PROPFIND", "GET"]);
        assert_eq!(fixture.remote.file_names(), vec![PKG_OLD, PKG_NEW]);
    }

    #[test]
    fn repeated_download_of_the_same_package_is_idempotent() {
        let fixture = Fixture::new("download-repeat");
        fixture.seed(PKG_NEW, &[("settings.json", "{\"v\":2}")]);

        let first = fixture.download(BASE, None).expect("第一次取包");
        let second = fixture.download(BASE, None).expect("第二次取包");

        assert_eq!(first.staged, second.staged);
        assert_eq!(
            staging_entries(&fixture.app_data()),
            vec![PKG_NEW.to_string()]
        );
        assert_eq!(
            fs::read_to_string(second.staged.staging_root.join("snapshot/settings.json")).unwrap(),
            "{\"v\":2}"
        );
    }

    #[test]
    fn corrupt_packages_leave_staging_empty() {
        let valid = package_bytes(PKG_NEW, &[("settings.json", "{\"v\":2}")]);
        let truncated = valid[..valid.len() / 2].to_vec();

        for (label, body) in [
            ("truncated", truncated),
            ("unsafe-entry", unsafe_entry_package()),
            ("garbage", b"not a zip at all".to_vec()),
        ] {
            let fixture = Fixture::new(&format!("download-corrupt-{label}"));
            fixture.remote.seed(PKG_NEW, &body);

            let failure = fixture.download(BASE, None).expect_err("坏包必须报错");

            assert!(
                matches!(failure.code, "package_corrupt" | "package_unsafe_entry"),
                "{label}：{}（{}）",
                failure.code,
                failure.message
            );
            assert!(
                staging_entries(&fixture.app_data()).is_empty(),
                "{label} 失败不能污染暂存"
            );
        }
    }

    /// 哈希不符只有拿到 manifest 才能发现，解包闸门看不到它：这一关由进计划时的校验把守。
    #[test]
    fn hash_mismatch_package_is_caught_by_the_plan() {
        let fixture = Fixture::new("download-hash");
        fixture
            .remote
            .seed(PKG_NEW, &hash_mismatch_package(PKG_NEW));
        let agent = fixture.agent();
        fs::create_dir_all(&agent).unwrap();

        let outcome = fixture.download(BASE, None).expect("取包本身成功");
        let failure = crate::plan::plan_snapshot(
            &agent,
            &outcome.staged.staging_root,
            &FakeRunner,
            crate::platform::HostOs::Posix,
        )
        .expect_err("哈希不符必须被计划挡下");

        assert_eq!(failure.code, "hash_mismatch", "{}", failure.message);
        assert!(!agent.join("settings.json").exists(), "校验失败不写 live");
    }

    #[test]
    fn download_records_the_package_set_without_touching_the_upload_fingerprint() {
        let fixture = Fixture::new("download-state");
        fixture.seed(PKG_OLD, &[("settings.json", "{}")]);
        fixture.seed(PKG_NEW, &[("settings.json", "{}")]);
        remote_state::save(
            &fixture.app_data(),
            &RemoteState {
                seen_at: "2026-11-20T00:00:00Z".to_string(),
                packages: vec![PKG_OLD.to_string()],
                last_uploaded_fingerprint: Some("deadbeefcafe".to_string()),
            },
        )
        .expect("写初始状态");

        let outcome = fixture.download(BASE, Some(PKG_NEW)).expect("指定包应成功");

        assert_eq!(
            outcome.previous_seen_at.as_deref(),
            Some("2026-11-20T00:00:00Z")
        );
        let state = fixture.state();
        assert_eq!(
            state.packages,
            vec![PKG_OLD.to_string(), PKG_NEW.to_string()],
            "记录的是这次列举看到的集合"
        );
        assert_eq!(
            state.last_uploaded_fingerprint.as_deref(),
            Some("deadbeefcafe"),
            "下载不改写上传指纹"
        );
        assert_ne!(state.seen_at, "2026-11-20T00:00:00Z");
        assert_eq!(state.seen_at.len(), 20, "RFC 3339 秒级：{}", state.seen_at);
    }

    /// 存储层保证只有包名能解析的条目进 `packages`；这里验的是排序、「最新」与两桶分类。
    #[test]
    fn list_orders_newest_first_and_buckets_unrecognized_names() {
        const PKG_MID: &str = "20261121-120000-0f0f0f0f.zip";
        let fixture = Fixture::new("list-order");
        // 按名字升序放入，列举结果必须反过来。
        fixture.seed(PKG_OLD, &[("settings.json", "{}")]);
        fixture.seed(PKG_MID, &[("settings.json", "{}")]);
        fixture.seed(PKG_NEW, &[("settings.json", "{}")]);
        fixture.remote.seed("notes.txt", b"keep me");
        fixture.remote.seed(".tmp-1700000000-deadbeef", b"partial");

        let listing = fixture.list(BASE).expect("列举应成功");

        let names: Vec<&str> = listing
            .packages
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, vec![PKG_NEW, PKG_MID, PKG_OLD]);
        let newest: Vec<&str> = listing
            .packages
            .iter()
            .filter(|entry| entry.is_newest)
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(newest, vec![PKG_NEW], "「最新」只落在名字最大的那份");
        assert_eq!(listing.packages[2].created_at, "2026-11-21T09:12:10Z");
        assert_eq!(listing.packages[0].fingerprint8, "deadbeef");
        assert!(listing.packages.iter().all(|entry| entry.size > 0));
        assert_eq!(
            listing.unrecognized,
            vec![
                ".tmp-1700000000-deadbeef".to_string(),
                "notes.txt".to_string()
            ]
        );
        // 列举只发 PROPFIND：不取包，不改远端。
        assert_eq!(fixture.remote.methods(), vec!["PROPFIND"]);
    }

    #[test]
    fn list_reports_an_empty_remote_without_an_error() {
        let fixture = Fixture::new("list-empty");

        let listing = fixture.list(BASE).expect("空远端不是错误");

        assert!(listing.packages.is_empty());
        assert!(listing.unrecognized.is_empty());
        assert_eq!(listing.unseen_count, 0);
        assert_eq!(listing.seen_at, None);
    }

    #[test]
    fn list_refuses_a_non_https_endpoint_without_sending_a_request() {
        let fixture = Fixture::new("list-insecure");
        fixture.seed(PKG_NEW, &[("settings.json", "{}")]);

        let failure = fixture
            .list("http://dav.example.com/dav")
            .expect_err("非 HTTPS 必须拒绝");

        assert_eq!(failure.code, "remote_insecure");
        assert!(
            fixture.remote.methods().is_empty(),
            "门槛在最前：一个请求都不该发出"
        );
    }

    /// 状态里有两个包、远端三个（多出一个新的）。
    #[test]
    fn list_counts_unseen_packages_and_leaves_the_state_file_alone() {
        const PKG_MID: &str = "20261121-120000-0f0f0f0f.zip";
        let fixture = Fixture::new("list-unseen");
        fixture.seed(PKG_OLD, &[("settings.json", "{}")]);
        fixture.seed(PKG_MID, &[("settings.json", "{}")]);
        fixture.seed(PKG_NEW, &[("settings.json", "{}")]);
        let state = RemoteState {
            seen_at: "2026-11-21T12:00:00Z".to_string(),
            packages: vec![PKG_OLD.to_string(), PKG_MID.to_string()],
            last_uploaded_fingerprint: None,
        };
        remote_state::save(&fixture.app_data(), &state).expect("写初始状态");

        let listing = fixture.list(BASE).expect("列举应成功");

        assert_eq!(listing.unseen_count, 1, "只算云端有、本机记录里没有的");
        assert_eq!(listing.seen_at.as_deref(), Some("2026-11-21T12:00:00Z"));
        assert_eq!(fixture.state(), state, "只读命令不能写本机记录");
    }

    #[test]
    fn list_creates_no_state_file_and_takes_no_package() {
        let fixture = Fixture::new("list-no-state");
        fixture.seed(PKG_NEW, &[("settings.json", "{}")]);

        fixture.list(BASE).expect("列举应成功");

        assert!(!fixture
            .app_data()
            .join(remote_state::REMOTE_STATE_FILE_NAME)
            .exists());
        assert!(
            staging_entries(&fixture.app_data()).is_empty(),
            "列举不取包"
        );
    }

    /// 清理只认本应用自己产出过的临时名：合法包、`manifest.json`、云盘冲突副本一律不动。
    #[test]
    fn cleanup_deletes_only_our_own_temp_names() {
        const TEMP_PACKAGE: &str = ".tmp-1795344750123456789-deadbeef";
        const TEMP_PLAIN: &str = ".tmp-1795344750123456789";
        const FOREIGN_TEMP: &str = ".tmp-notes.txt";
        const CONFLICT: &str = "a (冲突).zip";
        let fixture = Fixture::new("cleanup-only-own");
        fixture.seed(PKG_NEW, &[("settings.json", "{}")]);
        for name in [
            TEMP_PACKAGE,
            TEMP_PLAIN,
            FOREIGN_TEMP,
            "manifest.json",
            CONFLICT,
        ] {
            fixture.remote.seed(name, b"leftover");
        }

        let outcome = fixture.cleanup(BASE).expect("清理应成功");

        let mut deleted = outcome.deleted.clone();
        deleted.sort();
        assert_eq!(
            deleted,
            vec![TEMP_PLAIN.to_string(), TEMP_PACKAGE.to_string()]
        );
        let mut skipped = outcome.skipped.clone();
        skipped.sort();
        assert_eq!(
            skipped,
            vec![
                FOREIGN_TEMP.to_string(),
                CONFLICT.to_string(),
                "manifest.json".to_string(),
            ]
        );
        assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
        // 两项候选、两次 DELETE：不多删也不重删。
        assert_eq!(
            fixture.remote.methods(),
            vec!["PROPFIND", "DELETE", "DELETE"]
        );
        assert_eq!(
            fixture.remote.file_names(),
            vec![
                FOREIGN_TEMP.to_string(),
                PKG_NEW.to_string(),
                CONFLICT.to_string(),
                "manifest.json".to_string(),
            ],
            "合法包与外来文件必须还在"
        );
    }

    #[test]
    fn cleanup_reports_a_failed_delete_and_still_deletes_the_rest() {
        const OK_TEMP: &str = ".tmp-1795344750123456789-deadbeef";
        const BAD_TEMP: &str = ".tmp-1795344750123456790";
        let fixture = Fixture::new("cleanup-partial");
        fixture.remote.seed(OK_TEMP, b"leftover");
        fixture.remote.seed(BAD_TEMP, b"leftover");
        fixture.remote.fail_delete(BAD_TEMP);

        let outcome = fixture
            .cleanup(BASE)
            .expect("单项失败不能把整次清理判成失败");

        assert_eq!(outcome.deleted, vec![OK_TEMP.to_string()]);
        assert_eq!(outcome.failed.len(), 1, "{:?}", outcome.failed);
        assert_eq!(outcome.failed[0].name, BAD_TEMP);
        assert!(!outcome.failed[0].reason.is_empty());
        assert_eq!(
            fixture.remote.file_names(),
            vec![BAD_TEMP.to_string()],
            "删不掉的那项留在远端"
        );
    }

    #[test]
    fn cleanup_refuses_a_non_https_endpoint_without_sending_a_request() {
        const TEMP_PACKAGE: &str = ".tmp-1795344750123456789-deadbeef";
        let fixture = Fixture::new("cleanup-insecure");
        fixture.remote.seed(TEMP_PACKAGE, b"leftover");

        let failure = fixture
            .cleanup("http://dav.example.com/dav")
            .expect_err("非 HTTPS 必须拒绝");

        assert_eq!(failure.code, "remote_insecure");
        assert!(
            fixture.remote.methods().is_empty(),
            "门槛在最前：连 PROPFIND 都不该发出"
        );
        assert_eq!(fixture.remote.file_names(), vec![TEMP_PACKAGE.to_string()]);
    }

    /// 本地实现用真目录：清理后合法包与外来文件必须还在，再来一遍不报错也不删东西。
    #[test]
    fn local_cleanup_keeps_packages_and_foreign_files() {
        const TEMP_PACKAGE: &str = ".tmp-1795344750123456789-deadbeef";
        const TEMP_PLAIN: &str = ".tmp-1795344750123456789";
        let temp = TempDir::new("cleanup-local");
        let store_root = temp.path().join("cloud");
        write_package(&store_root, PKG_NEW, &[("settings.json", "{}")]);
        for name in [TEMP_PACKAGE, TEMP_PLAIN, ".tmp-notes.txt", "manifest.json"] {
            fs::write(store_root.join(name), b"leftover").unwrap();
        }
        fs::create_dir_all(store_root.join("snapshot")).unwrap();

        let store = LocalStore::new(store_root.clone());
        let outcome = cleanup_temp_files(&store).expect("清理应成功");

        assert_eq!(outcome.deleted.len(), 2, "{:?}", outcome.deleted);
        assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
        assert_eq!(outcome.skipped.len(), 3, "{:?}", outcome.skipped);
        assert!(store_root.join(PKG_NEW).is_file(), "合法包不能动");
        assert!(
            store_root.join(".tmp-notes.txt").is_file(),
            "外来文件不能动"
        );
        assert!(store_root.join("manifest.json").is_file());
        assert!(store_root.join("snapshot").is_dir(), "目录不在清理范围");
        assert!(!store_root.join(TEMP_PACKAGE).exists());
        assert!(!store_root.join(TEMP_PLAIN).exists());

        // 幂等：第二遍候选为空，不报错也不删东西。
        let again = cleanup_temp_files(&store).expect("重复清理应成功");
        assert!(again.deleted.is_empty(), "{:?}", again.deleted);
        assert!(again.failed.is_empty(), "{:?}", again.failed);
        assert_eq!(again.skipped.len(), 3);
        assert!(store_root.join(PKG_NEW).is_file());
        assert!(store_root.join(".tmp-notes.txt").is_file());
    }

    /// 指定旧包：不能退回「最新」那一份，否则用户会应用一份没选过的快照。
    #[test]
    fn download_honours_the_named_older_package() {
        let fixture = Fixture::new("download-older");
        fixture.seed(PKG_OLD, &[("settings.json", "{\"v\":1}")]);
        fixture.seed(PKG_NEW, &[("settings.json", "{\"v\":2}")]);

        let outcome = fixture.download(BASE, Some(PKG_OLD)).expect("指定包应成功");

        assert_eq!(outcome.staged.package_name, PKG_OLD);
        assert_eq!(
            fs::read_to_string(outcome.staged.staging_root.join("snapshot/settings.json")).unwrap(),
            "{\"v\":1}"
        );
        assert_eq!(
            staging_entries(&fixture.app_data()),
            vec![PKG_OLD.to_string()]
        );
        assert_eq!(
            fixture.remote.methods(),
            vec!["PROPFIND", "GET"],
            "只取选中那份"
        );
    }

    /// 固定版本的假命令：计划阶段会探测 node / pi 版本，不真的起进程。
    struct FakeRunner;

    impl crate::doctor::CommandRunner for FakeRunner {
        fn run(
            &self,
            _script: &str,
            _timeout: std::time::Duration,
            _env: &[(&str, String)],
        ) -> crate::doctor::CommandOutcome {
            crate::doctor::CommandOutcome::Ok {
                stdout: "node:22.23.0
pi:1.5.2
"
                .to_string(),
                stderr: String::new(),
            }
        }
    }

    /// 端到端：存储根里的包 → 暂存目录 → 只读计划 → 应用，一行都不用改计划与应用。
    #[test]
    fn staged_package_drives_the_existing_plan_and_apply() {
        let temp = TempDir::new("plan-apply");
        let store_root = temp.path().join("cloud");
        let app_data = temp.path().join("data");
        let agent_dir = temp.path().join("agent");
        fs::create_dir_all(&agent_dir).unwrap();

        let settings = "{\"theme\":\"dark\"}";
        write_package(
            &store_root,
            "20261122-105230-deadbeef.zip",
            &[("settings.json", settings)],
        );
        let store = LocalStore::new(store_root);

        let staged = stage_package(&store, &app_data, None).expect("取包应成功");
        let report = crate::plan::plan_snapshot(
            &agent_dir,
            &staged.staging_root,
            &FakeRunner,
            crate::platform::HostOs::Posix,
        )
        .expect("计划应成功");
        assert_eq!(report.verification.checked, 1);
        assert_eq!(report.plan.added.len(), 1);
        assert_eq!(report.plan.added[0].path, "settings.json");
        assert!(!agent_dir.join("settings.json").exists(), "计划不写 live");

        let applied = crate::apply::apply(
            &agent_dir,
            &staged.staging_root,
            &report.manifest_info.fingerprint,
            &[],
            &app_data,
            &FakeRunner,
            &crate::apply::PlatformLinkCreator,
        )
        .expect("应用应成功");
        assert_eq!(applied.written, 1);
        assert_eq!(
            fs::read_to_string(agent_dir.join("settings.json")).unwrap(),
            settings
        );
        assert!(applied.backup_dir.starts_with(app_data.join("backups")));
    }

    #[test]
    fn resolve_root_allows_only_sync_root_and_staging_children() {
        let temp = TempDir::new("root");
        let cloud = temp.path().join("cloud");
        let app_data = temp.path().join("data");
        let staging_root = app_data.join(STAGING_DIR_NAME).join("2026-11-20T05-30-00");
        fs::create_dir_all(&cloud).unwrap();
        fs::create_dir_all(&staging_root).unwrap();

        // 同步根：本地文件夹模式下的计划与应用照旧。
        assert_eq!(
            resolve_root(Some(&cloud), &app_data, cloud.to_str().unwrap()).unwrap(),
            fs::canonicalize(&cloud).unwrap()
        );
        // 暂存子目录：WebDAV 下载后走同一套计划与应用。
        assert_eq!(
            resolve_root(Some(&cloud), &app_data, staging_root.to_str().unwrap()).unwrap(),
            fs::canonicalize(&staging_root).unwrap()
        );
        // WebDAV 模式可以完全没有同步根。
        assert!(resolve_root(None, &app_data, staging_root.to_str().unwrap()).is_ok());
    }

    #[test]
    fn resolve_root_rejects_everything_else() {
        let temp = TempDir::new("root-reject");
        let cloud = temp.path().join("cloud");
        let evil = temp.path().join("evil");
        let app_data = temp.path().join("data");
        let staging = app_data.join(STAGING_DIR_NAME);
        fs::create_dir_all(&cloud).unwrap();
        fs::create_dir_all(&evil).unwrap();
        fs::create_dir_all(&staging).unwrap();

        for requested in [
            evil.to_str().unwrap().to_string(),
            // 暂存根自身不算：它下面没有 manifest。
            staging.to_str().unwrap().to_string(),
            temp.path().join("missing").to_string_lossy().into_owned(),
            "relative/path".to_string(),
            String::new(),
        ] {
            let rejected = resolve_root(Some(&cloud), &app_data, &requested);
            assert!(rejected.is_err(), "{requested} 应被拒绝：{rejected:?}");
        }

        // 同步根的子目录也不行：计划只接受根本身。
        let nested = cloud.join("snapshot");
        fs::create_dir_all(&nested).unwrap();
        assert!(resolve_root(Some(&cloud), &app_data, nested.to_str().unwrap()).is_err());
    }

    fn interop_credentials() -> Option<(String, ReqwestTransport)> {
        if let (Ok(url), Ok(user), Ok(pass)) = (
            std::env::var("DAV_URL"),
            std::env::var("DAV_USER"),
            std::env::var("DAV_PASS"),
        ) {
            return Some((
                url,
                ReqwestTransport::new(&user, &pass).expect("构造 transport"),
            ));
        }

        if let Ok(dir) = std::env::var("DAV_CREDENTIALS_DIR") {
            let credentials = crate::credentials::load(Path::new(&dir))
                .expect("读取凭据文件")
                .expect("凭据目录下应有 webdav.json");
            let transport = ReqwestTransport::new(&credentials.username, &credentials.password)
                .expect("构造 transport");
            return Some((credentials.url, transport));
        }

        println!("跳过真机测试：没有 DAV_URL/DAV_USER/DAV_PASS，也没有 DAV_CREDENTIALS_DIR");
        None
    }

    /// `PROPFIND` 只问会不会回元素，不问具体值，所以四个属性一次问全。
    const MOVE_PROBE_PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getetag/><d:getlastmodified/></d:prop></d:propfind>"#;

    /// `MOVE` 定案探测：上传定案是「`PUT` 临时名 → `MOVE` 正式名」，而 `MOVE` 与
    /// `Overwrite: F` 的具体行为依赖服务商实现，只能真机验证。
    ///
    /// 只写 `.tmp-probe-*`，不碰任何已有文件；结束删净并 `PROPFIND` 复核。
    /// 输出只有状态码、布尔值与长度：不带凭据，也不带 XML 原文。
    #[test]
    #[ignore = "需要真实 WebDAV 服务器与凭据"]
    fn interop_probes_move_semantics() {
        let Some((base, transport)) = interop_credentials() else {
            return;
        };

        let root = format!("{}/PiAgentSync", base.trim_end_matches('/'));
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("读时钟")
            .as_nanos();
        let stamp_text = stamp.to_string();
        let (name, moved, dup) = (
            format!(".tmp-probe-{stamp}"),
            format!(".tmp-probe-{stamp}-moved"),
            format!(".tmp-probe-{stamp}-dup"),
        );

        // 只在这一处拼请求：探针要的是原始状态码，不是 `WebdavClient` 的解释结果。
        let send = |method: &str,
                    path: &str,
                    headers: Vec<(String, String)>,
                    body: Option<Vec<u8>>,
                    timeout: std::time::Duration|
         -> DavResponse {
            let url = if path.is_empty() {
                root.clone()
            } else {
                format!("{root}/{path}")
            };
            transport
                .send(DavRequest {
                    method: method.to_string(),
                    url,
                    headers,
                    body,
                    timeout,
                })
                .expect("传输失败")
        };
        let propfind_root = || {
            send(
                "PROPFIND",
                "",
                vec![
                    ("Depth".to_string(), "1".to_string()),
                    (
                        "Content-Type".to_string(),
                        "application/xml; charset=utf-8".to_string(),
                    ),
                ],
                Some(MOVE_PROBE_PROPFIND_BODY.as_bytes().to_vec()),
                crate::webdav::CONTROL_TIMEOUT,
            )
        };
        let move_headers = |destination: &str| {
            vec![
                ("Overwrite".to_string(), "F".to_string()),
                ("Destination".to_string(), format!("{root}/{destination}")),
            ]
        };

        // 1. 工作根不存在就建；405/301/302 是「已存在」。
        let status = send(
            "MKCOL",
            "",
            Vec::new(),
            None,
            crate::webdav::CONTROL_TIMEOUT,
        )
        .status;
        let root_ready = matches!(status, 200 | 201 | 204 | 301 | 302 | 405);
        println!("1. MKCOL 工作根：{status}（可用或已存在：{root_ready}）");
        assert!(root_ready, "工作根既没建出来也不是已存在：HTTP {status}");

        // 2. 先 PUT 到临时名，正式名由下一步的 `MOVE` 产生。
        let status = send(
            "PUT",
            &name,
            Vec::new(),
            Some(b"probe-one".to_vec()),
            crate::webdav::TRANSFER_TIMEOUT,
        )
        .status;
        println!(
            "2. PUT 临时名：{status}（成功：{}）",
            (200..300).contains(&status)
        );
        assert!(
            (200..300).contains(&status),
            "PUT 临时名失败：HTTP {status}"
        );

        // 3. MOVE 到正式名：上传定案成立的前提之一。
        let move_status = send(
            "MOVE",
            &name,
            move_headers(&moved),
            None,
            crate::webdav::CONTROL_TIMEOUT,
        )
        .status;
        println!("3. MOVE → 正式名：{move_status}");
        // 上传定案就靠这一步：正式名下的包只能由 `MOVE` 原子产生。
        assert!(
            matches!(move_status, 201 | 204),
            "上传定案依赖服务商支持 MOVE（RFC 4918 必备方法）：临时名 → 正式名应回 201/204，实际 HTTP {move_status}"
        );

        // 4. 第二个临时文件：用于验证目标已存在时 `Overwrite: F` 是否真的生效。
        let status = send(
            "PUT",
            &dup,
            Vec::new(),
            Some(b"probe-two".to_vec()),
            crate::webdav::TRANSFER_TIMEOUT,
        )
        .status;
        println!(
            "4. PUT 第二个临时名：{status}（成功：{}）",
            (200..300).contains(&status)
        );

        // 5. 目标是已存在的名字：期望 409 或 412。
        let status = send(
            "MOVE",
            &dup,
            move_headers(&moved),
            None,
            crate::webdav::CONTROL_TIMEOUT,
        )
        .status;
        println!("5. MOVE → 已存在的名字（Overwrite: F）：{status}");
        // 上传定案依赖这条：同名包永不被覆盖，正是靠 `Overwrite: F` 被拒。
        assert!(
            matches!(status, 409 | 412),
            "上传定案依赖 Overwrite: F 生效：目标已存在时 `MOVE` 应回 409/412 且不覆盖，实际 HTTP {status}"
        );

        // 6. ETag 响应头：只报有无与长度，值本身不打印。
        let response = send(
            "GET",
            &moved,
            Vec::new(),
            None,
            crate::webdav::TRANSFER_TIMEOUT,
        );
        let etag = response
            .headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case("etag"))
            .map(|(_, value)| value.clone());
        println!(
            "6. GET 正式名：{}；ETag 响应头：{}（长度 {}）",
            response.status,
            if etag.is_some() { "Some" } else { "None" },
            etag.as_ref().map_or(0, String::len)
        );

        // 7. PROPFIND 是否回那两个后续要用的属性；只按元素名判断，不打印 XML。
        let response = propfind_root();
        let body = String::from_utf8_lossy(&response.body);
        println!(
            "7. PROPFIND Depth:1：{}；getetag 元素：{}；getlastmodified 元素：{}（响应 {} 字节）",
            response.status,
            body.contains("getetag"),
            body.contains("getlastmodified"),
            response.body.len()
        );

        // 8. 只删自己的三个探针名（`name` 可能已被 MOVE 走，404 不算失败），再列一次复核。
        for probe in [&name, &moved, &dup] {
            let status = send(
                "DELETE",
                probe,
                Vec::new(),
                None,
                crate::webdav::CONTROL_TIMEOUT,
            )
            .status;
            let gone = (200..300).contains(&status) || status == 404;
            println!("8. DELETE 探针：{status}（已清理：{gone}）");
        }

        let response = propfind_root();
        let body = String::from_utf8_lossy(&response.body);
        let leftover = body.contains(&stamp_text);
        println!(
            "8. 清理复核 PROPFIND：{}；仍有本次探针：{leftover}",
            response.status
        );
        // 403 或空 body 同样不含探针名：先要回列表，这条复核才有意义。
        assert!(
            matches!(response.status, 200 | 207),
            "清理复核 PROPFIND 应回 207/200，实际 HTTP {}",
            response.status
        );
        assert!(!leftover, "清理后 PROPFIND 仍列出本次探针");
    }
}

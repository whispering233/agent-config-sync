//! 快照包的取用：从存储根取包、解到本机暂存目录，以及计划与应用允许的根判定。
//!
//! 规则来源：`docs/design/10-snapshot-sync.md` 第 2、6 节，`docs/design/40-webdav.md` 第 6 节。
//! 这里不决定快照内容，也不碰 live：解包产物与同步根同构（`manifest.json` + `snapshot/`），
//! 直接交给 `plan` / `apply`，它们不认识「包」这个概念。

use crate::package;
use crate::platform;
use crate::snapshot::{self, Manifest};
use crate::store::{LocalStore, PackageStore};
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
    /// manifest 里的 RFC 3339 UTC 创建时间。
    pub created_at: String,
    pub file_count: usize,
    pub staging_root: PathBuf,
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

/// 取包到本机暂存：列举 → 选包（缺省取名字最大的一份）→ 解到 `staging/<包名>/` → 只留这一份。
///
/// 选中的包不在列表里就是「刚被别人删了」：宁可报错也不退回另一份，否则用户会应用一份
/// 没选过的快照。
pub fn stage_package(
    store: &LocalStore,
    app_data: &Path,
    requested: Option<&str>,
) -> Result<StagedPackage, String> {
    let listing = store
        .list()
        .map_err(|error| format!("无法列举快照包：{}", error.message))?;
    let chosen = match requested {
        Some(name) => listing
            .packages
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| format!("选中的快照包已不在存储目录里：{name}"))?,
        None => listing
            .packages
            .iter()
            .max_by(|left, right| left.name.cmp(&right.name))
            .ok_or_else(|| "同步文件夹里还没有快照包：请先导出一份".to_string())?,
    };

    let root = app_data.join(STAGING_DIR_NAME);
    platform::create_dir_all_mode(&root, STAGING_DIR_MODE)
        .map_err(|error| format!("无法创建暂存目录 {}：{error}", root.display()))?;
    let target = root.join(&chosen.name);
    // 重新取同一份包时旧暂存还在，而解包拒绝已存在的落点：先整份删掉再解。
    if target.symlink_metadata().is_ok() {
        remove_path(&target)?;
    }

    package::unpack(&store.root().join(&chosen.name), &target).map_err(|error| error.message)?;
    // 暂存常驻只保留当前这一份：更早的解包结果已经不可能被应用。
    prune_staging(app_data, &target);

    let manifest = read_staged_manifest(&target)?;
    Ok(StagedPackage {
        package_name: chosen.name.clone(),
        created_at: manifest.created_at,
        file_count: manifest.files.len(),
        staging_root: target,
    })
}

/// 解出来的 manifest：给 UI 报创建时间与文件数，也顺带在进计划前挡掉不认识的格式版本。
fn read_staged_manifest(staging_root: &Path) -> Result<Manifest, String> {
    let path = staging_root.join(snapshot::MANIFEST_FILE_NAME);
    let bytes = fs::read(&path)
        .map_err(|error| format!("无法读取解包后的 manifest {}：{error}", path.display()))?;

    snapshot::parse_manifest(&bytes).map_err(|failure| failure.message)
}

/// 暂存里的旧包可能是目录，也可能是同名文件：两种都删掉。
fn remove_path(path: &Path) -> Result<(), String> {
    let metadata = path
        .symlink_metadata()
        .map_err(|error| format!("无法检查旧暂存 {}：{error}", path.display()))?;
    let result = if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };

    result.map_err(|error| format!("无法清理旧暂存 {}：{error}", path.display()))
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
    use crate::webdav::{DavRequest, DavResponse, DavTransport, ReqwestTransport};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

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

        let store = LocalStore::new(store_root);
        let staged = stage_package(&store, &app_data, None).expect("取包应成功");

        assert_eq!(staged.package_name, newest);
        assert_eq!(staged.created_at, "2026-11-22T10:52:30Z");
        assert_eq!(staged.file_count, 1);
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
        assert!(error.contains("还没有快照包"), "{error}");

        write_package(
            &store_root,
            "20261122-105230-deadbeef.zip",
            &[("settings.json", "{}")],
        );
        let error = stage_package(&store, &app_data, Some("20261123-000000-aaaaaaaa.zip"))
            .expect_err("选中的包不存在必须报错");
        assert!(error.contains("已不在存储目录"), "{error}");
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

        assert!(!error.is_empty());
        assert!(
            staging_entries(&app_data).is_empty(),
            "失败不污染暂存：{error}"
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

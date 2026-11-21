//! 从 WebDAV 取回快照到本机暂存目录。
//!
//! 规则来源：`docs/design/40-webdav.md` 第 6、7 节。这里只做搬运与校验：读远端 manifest，
//! 逐文件下载到 `<应用数据目录>/staging/<时间戳>/`，逐个核对字节数与 sha256，全部通过后
//! 才写 manifest 与远端状态。计划、备份与落地仍走 `plan` / `apply`，本模块不碰 live。
//!
//! 远端 manifest 是不可信输入：路径必须落在暂存目录内、文件有体积上限、内容必须与哈希一致。
//! 任一失败都删掉本次暂存目录，远端状态保持原样。

use crate::config;
use crate::scope;
use crate::snapshot::{self, Manifest, FORMAT_VERSION, MANIFEST_FILE_NAME};
use crate::webdav::{DavError, DavTransport, WebdavClient};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 暂存目录名：位于应用数据目录下，与 `backups/` 同级，不进云盘也不进快照。
pub const STAGING_DIR_NAME: &str = "staging";
/// 远端状态文件名。
pub const REMOTE_STATE_FILE_NAME: &str = "remote-state.json";
/// 远端 manifest 上限：正常快照的 manifest 只有几 KB，超过即视为不可信。
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
/// 单个快照文件上限。
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// 暂存目录权限：快照可能含明文密钥，只允许本用户读写。
const STAGING_DIR_MODE: u32 = 0o700;

/// 本机记录的远端状态：上次看到的那份远端 manifest 与看到的时间。
///
/// 只认 `manifest.fingerprint`，不用时间戳判断内容是否相同（`40-webdav.md` 第 7 节）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteState {
    pub seen_at: String,
    pub manifest: Manifest,
}

/// 一次下载的结果。`staging_root` 顶替同步根，交给计划与应用复用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadOutcome {
    pub staging_root: PathBuf,
    pub manifest: Manifest,
    /// 本次下载之前记录的同步时间；本机没有记录时为 `None`。
    pub previous_seen_at: Option<String>,
}

/// 下载失败。网络类错误保留 [`DavError`]，command 层统一翻译成一句话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferError {
    /// 远端没有 manifest：还没上传过快照，或地址填错了。
    NotFound,
    /// manifest 无法解析、字段不完整，或 formatVersion 不是当前支持值。
    Corrupt(String),
    /// 体积超出上限：远端数据不可信，验不了就别往盘上写。
    TooLarge(String),
    /// manifest 里的路径越出暂存目录。
    OutOfBounds(String),
    /// manifest 列出，但远端取不到的文件。
    Missing(String),
    /// 字节数与 manifest 记录不一致。
    SizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    /// 内容哈希与 manifest 记录不一致。
    HashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    /// 协议层失败（无凭据、不可达、认证、配额等）。
    Network(DavError),
    /// 本机暂存目录或远端状态文件读写失败。
    Storage(String),
}

impl fmt::Display for TransferError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(
                formatter,
                "远端没有 manifest.json：还没有上传过快照，或 WebDAV 地址填错了"
            ),
            Self::Corrupt(detail) | Self::TooLarge(detail) | Self::Storage(detail) => {
                write!(formatter, "{detail}")
            }
            Self::OutOfBounds(path) => {
                write!(formatter, "远端 manifest 里的路径越出暂存目录：{path}")
            }
            Self::Missing(path) => write!(formatter, "远端缺少 manifest 列出的文件：{path}"),
            Self::SizeMismatch {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "{path} 与 manifest 不一致：记录 {expected} 字节，实际 {actual} 字节"
            ),
            Self::HashMismatch { path, .. } => {
                write!(formatter, "{path} 的内容与 manifest 记录的 sha256 不一致")
            }
            Self::Network(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for TransferError {}

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

/// 读远端状态。文件不存在（换机器、换地址）按「没有记录」处理。
pub fn load_state(app_data: &Path) -> Result<Option<RemoteState>, String> {
    let path = app_data.join(REMOTE_STATE_FILE_NAME);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("无法读取远端状态 {}：{error}", path.display())),
    };

    serde_json::from_str(&text).map(Some).map_err(|error| {
        format!(
            "远端状态文件无法解析（{}）：{error}；删除它可以重新记录",
            path.display()
        )
    })
}

/// 写远端状态：同目录临时文件 + rename 原子替换，文件创建即 0600。
///
/// 内容是非密钥的远端 manifest 副本，但里面列着文件路径，仍然只有本用户可读。
pub fn save_state(app_data: &Path, manifest: &Manifest) -> Result<(), String> {
    let target = app_data.join(REMOTE_STATE_FILE_NAME);
    let temp = app_data.join(format!("{REMOTE_STATE_FILE_NAME}.tmp"));

    let state = RemoteState {
        seen_at: snapshot::format_timestamp(now_seconds()),
        manifest: manifest.clone(),
    };
    let mut text = serde_json::to_string_pretty(&state)
        .map_err(|error| format!("远端状态序列化失败：{error}"))?;
    text.push('\n');

    fs::create_dir_all(app_data)
        .map_err(|error| format!("无法创建应用数据目录 {}：{error}", app_data.display()))?;
    config::write_private_file(&temp, text.as_bytes()).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("无法写入远端状态 {}：{error}", temp.display())
    })?;
    if let Err(error) = fs::rename(&temp, &target) {
        let _ = fs::remove_file(&temp);
        return Err(format!("无法替换远端状态 {}：{error}", target.display()));
    }

    Ok(())
}

/// 下载整份快照到新的暂存目录。
///
/// 顺序：读 manifest → 建暂存目录 → 逐文件校验落盘 → 原样写 manifest → 记录远端状态 →
/// 清理更早的暂存目录。任一步失败都删掉本次暂存目录，远端状态与 live 保持原样。
pub fn download<T: DavTransport>(
    client: &WebdavClient<T>,
    app_data: &Path,
) -> Result<DownloadOutcome, TransferError> {
    let (bytes, _etag) = client
        .get(MANIFEST_FILE_NAME)
        .map_err(TransferError::Network)?
        .ok_or(TransferError::NotFound)?;
    // 先看体积再解析：远端 manifest 是不可信输入，不该让超大 JSON 拖住整个流程。
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(TransferError::TooLarge(format!(
            "远端 manifest 有 {} 字节，超过 {MAX_MANIFEST_BYTES} 字节上限",
            bytes.len()
        )));
    }
    let manifest = parse_manifest(&bytes)?;
    // 状态文件损坏时先停：它记着「上次看到的远端」，覆盖掉就没人能判断冲突了。
    let previous = load_state(app_data).map_err(TransferError::Storage)?;

    let staging_root = create_staging_dir(app_data)?;
    let staged = fill_staging(client, &manifest, &staging_root)
        .and_then(|()| write_staged_manifest(&staging_root, &bytes))
        .and_then(|()| save_state(app_data, &manifest).map_err(TransferError::Storage));
    if let Err(error) = staged {
        let _ = fs::remove_dir_all(&staging_root);
        return Err(error);
    }

    prune_staging(app_data, &staging_root);

    Ok(DownloadOutcome {
        staging_root,
        manifest,
        previous_seen_at: previous.map(|state| state.seen_at),
    })
}

/// 解析远端 manifest。版本单独先看：结构不认识时也要报「版本不支持」而不是「文件损坏」。
fn parse_manifest(bytes: &[u8]) -> Result<Manifest, TransferError> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
        TransferError::Corrupt(format!("远端 manifest 不是合法 JSON：{error}"))
    })?;

    match value.get("formatVersion").and_then(serde_json::Value::as_u64) {
        Some(version) if version == u64::from(FORMAT_VERSION) => {}
        Some(version) => {
            return Err(TransferError::Corrupt(format!(
                "远端 manifest 的 formatVersion 是 {version}，本应用只支持 {FORMAT_VERSION}"
            )))
        }
        None => {
            return Err(TransferError::Corrupt(
                "远端 manifest 缺少 formatVersion 字段".to_string(),
            ))
        }
    }

    serde_json::from_value(value).map_err(|error| {
        TransferError::Corrupt(format!("远端 manifest 字段不完整：{error}"))
    })
}

/// 建本次暂存目录。命名与备份目录同一套：UTC 时间戳，同一秒内加序号。
fn create_staging_dir(app_data: &Path) -> Result<PathBuf, TransferError> {
    let root = app_data.join(STAGING_DIR_NAME);
    fs::create_dir_all(&root).map_err(storage_error)?;
    // create_dir_all 按 umask 建，补一次显式权限。
    fs::set_permissions(&root, fs::Permissions::from_mode(STAGING_DIR_MODE))
        .map_err(storage_error)?;

    let base = snapshot::format_timestamp(now_seconds()).replace(':', "-");
    let mut name = base.clone();
    let mut suffix = 1;
    while root.join(&name).exists() {
        name = format!("{base}-{suffix}");
        suffix += 1;
    }

    let dir = root.join(&name);
    DirBuilder::new()
        .mode(STAGING_DIR_MODE)
        .create(&dir)
        .map_err(storage_error)?;

    Ok(dir)
}

/// 按 manifest 逐文件下载并校验。暂存目录按同步根的同构布局写入，
/// 后面 `plan` / `apply` 拿它顶替同步根即可，语义完全不变。
fn fill_staging<T: DavTransport>(
    client: &WebdavClient<T>,
    manifest: &Manifest,
    staging_root: &Path,
) -> Result<(), TransferError> {
    for file in &manifest.files {
        // 先按 manifest 声明的体积挡一次：真正 GET 回来才知道实际大小。
        if file.size > MAX_FILE_BYTES {
            return Err(TransferError::TooLarge(format!(
                "远端文件 {} 声明 {} 字节，超过 {MAX_FILE_BYTES} 字节上限",
                file.path, file.size
            )));
        }
        let target = scope::resolve_within(staging_root, Path::new(&file.path))
            .map_err(|_| TransferError::OutOfBounds(file.path.clone()))?;

        let (bytes, _etag) = client
            .get(&file.path)
            .map_err(TransferError::Network)?
            .ok_or_else(|| TransferError::Missing(file.path.clone()))?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(TransferError::TooLarge(format!(
                "远端文件 {} 有 {} 字节，超过 {MAX_FILE_BYTES} 字节上限",
                file.path,
                bytes.len()
            )));
        }
        if bytes.len() as u64 != file.size {
            return Err(TransferError::SizeMismatch {
                path: file.path.clone(),
                expected: file.size,
                actual: bytes.len() as u64,
            });
        }
        let actual = snapshot::sha256_hex(&bytes);
        if actual != file.sha256 {
            return Err(TransferError::HashMismatch {
                path: file.path.clone(),
                expected: file.sha256.clone(),
                actual,
            });
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(storage_error)?;
        }
        config::write_private_file(&target, &bytes).map_err(|error| {
            TransferError::Storage(format!("写入暂存文件 {} 失败：{error}", file.path))
        })?;
    }

    Ok(())
}

/// manifest 原样落盘：云端写什么就存什么，不重新序列化，免得指纹与字节都对不上。
fn write_staged_manifest(staging_root: &Path, bytes: &[u8]) -> Result<(), TransferError> {
    let target = staging_root.join(MANIFEST_FILE_NAME);
    config::write_private_file(&target, bytes).map_err(|error| {
        TransferError::Storage(format!("写入暂存 manifest 失败：{error}"))
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

fn storage_error(error: io::Error) -> TransferError {
    TransferError::Storage(format!("无法写入本机暂存目录：{error}"))
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{
        fingerprint, ManifestFile, Requirements, SourceInfo, FORMAT_VERSION, PROFILE_ID,
    };
    use crate::webdav::{DavError, DavRequest, DavResponse};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU32, Ordering};

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

    struct FakeState {
        requests: RefCell<Vec<DavRequest>>,
        responses: RefCell<VecDeque<Result<DavResponse, DavError>>>,
    }

    /// 假 transport：记录请求，按顺序吐出预置响应。
    #[derive(Clone)]
    struct FakeTransport {
        state: Rc<FakeState>,
    }

    impl FakeTransport {
        fn new(responses: Vec<Result<DavResponse, DavError>>) -> Self {
            Self {
                state: Rc::new(FakeState {
                    requests: RefCell::new(Vec::new()),
                    responses: RefCell::new(responses.into()),
                }),
            }
        }

        fn requests(&self) -> Vec<DavRequest> {
            self.state.requests.borrow().clone()
        }
    }

    impl DavTransport for FakeTransport {
        fn send(&self, request: DavRequest) -> Result<DavResponse, DavError> {
            self.state.requests.borrow_mut().push(request);
            match self.state.responses.borrow_mut().pop_front() {
                Some(response) => response,
                None => Err(DavError::Other("测试没有预置响应".to_string())),
            }
        }
    }

    fn response_with_body(body: &[u8]) -> Result<DavResponse, DavError> {
        Ok(DavResponse {
            status: 200,
            headers: Vec::new(),
            body: body.to_vec(),
        })
    }

    fn response(status: u16) -> Result<DavResponse, DavError> {
        Ok(DavResponse {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        })
    }

    fn fake_client(
        responses: Vec<Result<DavResponse, DavError>>,
    ) -> (WebdavClient<FakeTransport>, FakeTransport) {
        let transport = FakeTransport::new(responses);
        let client = WebdavClient::new(transport.clone(), BASE);
        (client, transport)
    }

    fn manifest_file(path: &str, content: &str) -> ManifestFile {
        ManifestFile {
            path: path.to_string(),
            sha256: snapshot::sha256_hex(content.as_bytes()),
            size: content.len() as u64,
            kind: "text".to_string(),
        }
    }

    fn manifest_with(files: Vec<ManifestFile>) -> Manifest {
        Manifest {
            format_version: FORMAT_VERSION,
            profile_id: PROFILE_ID.to_string(),
            created_at: "2026-11-20T05:30:00Z".to_string(),
            fingerprint: fingerprint(FORMAT_VERSION, &files),
            source: SourceInfo {
                app: "0.0.1".to_string(),
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
            required_secrets: Vec::new(),
            required_packages: Vec::new(),
            plaintext_secrets: false,
        }
    }

    fn manifest_bytes(manifest: &Manifest) -> Vec<u8> {
        serde_json::to_vec_pretty(manifest).expect("序列化 manifest")
    }

    fn staging_entries(app_data: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(app_data.join(STAGING_DIR_NAME))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn download_stages_files_and_records_state() {
        let temp = TempDir::new("download");
        let app_data = temp.path().join("data");
        let settings = "{\"theme\": \"dark\"}";
        let skill = "# 技能\n";
        let manifest = manifest_with(vec![
            manifest_file("snapshot/settings.json", settings),
            manifest_file("snapshot/skills/antd/SKILL.md", skill),
        ]);
        let manifest_bytes = manifest_bytes(&manifest);
        let (client, transport) = fake_client(vec![
            response_with_body(&manifest_bytes),
            response_with_body(settings.as_bytes()),
            response_with_body(skill.as_bytes()),
        ]);

        let outcome = download(&client, &app_data).expect("下载应成功");

        // 暂存目录落在应用数据目录下，布局与同步根同构。
        let relative = outcome
            .staging_root
            .strip_prefix(app_data.join(STAGING_DIR_NAME))
            .expect("暂存目录应在 staging 之下")
            .to_path_buf();
        assert_eq!(relative.components().count(), 1, "暂存目录是 staging 的直接子目录");
        assert_eq!(
            fs::read_to_string(outcome.staging_root.join("snapshot/settings.json")).unwrap(),
            settings
        );
        assert_eq!(
            fs::read_to_string(outcome.staging_root.join("snapshot/skills/antd/SKILL.md")).unwrap(),
            skill
        );
        // manifest 原样落盘，不是重新序列化的结果。
        assert_eq!(
            fs::read(outcome.staging_root.join(MANIFEST_FILE_NAME)).unwrap(),
            manifest_bytes
        );

        assert_eq!(outcome.manifest, manifest);
        assert_eq!(outcome.previous_seen_at, None);

        let state = load_state(&app_data).expect("读状态").expect("状态应存在");
        assert_eq!(state.manifest, manifest);
        assert_eq!(state.seen_at.len(), 20);
        assert!(state.seen_at.ends_with('Z'), "{}", state.seen_at);

        // 请求顺序：先 manifest，再按 manifest 里的顺序逐个文件。
        let paths: Vec<String> = transport
            .requests()
            .into_iter()
            .map(|request| request.url)
            .collect();
        assert_eq!(
            paths,
            vec![
                format!("{BASE}/PiAgentSync/manifest.json"),
                format!("{BASE}/PiAgentSync/snapshot/settings.json"),
                format!("{BASE}/PiAgentSync/snapshot/skills/antd/SKILL.md"),
            ]
        );

        // 暂存目录只有本用户可进入。
        #[cfg(unix)]
        {
            let mode = fs::metadata(app_data.join(STAGING_DIR_NAME))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, STAGING_DIR_MODE);
            let file_mode = fs::metadata(outcome.staging_root.join("snapshot/settings.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(file_mode & 0o777, 0o600, "暂存文件应只有本用户可读");
        }
    }

    #[test]
    fn download_reports_the_previous_sync_time() {
        let temp = TempDir::new("previous");
        let app_data = temp.path().join("data");
        fs::create_dir_all(&app_data).unwrap();
        let previous = manifest_with(vec![manifest_file("snapshot/settings.json", "{}")]);
        let state = RemoteState {
            seen_at: "2026-11-01T08:00:00Z".to_string(),
            manifest: previous,
        };
        fs::write(
            app_data.join(REMOTE_STATE_FILE_NAME),
            serde_json::to_string_pretty(&state).unwrap(),
        )
        .unwrap();

        let manifest = manifest_with(vec![manifest_file("snapshot/settings.json", "{ \"a\": 1 }")]);
        let (client, _) = fake_client(vec![
            response_with_body(&manifest_bytes(&manifest)),
            response_with_body(b"{ \"a\": 1 }"),
        ]);

        let outcome = download(&client, &app_data).expect("下载应成功");

        assert_eq!(
            outcome.previous_seen_at.as_deref(),
            Some("2026-11-01T08:00:00Z")
        );
        // 本次观察覆盖旧记录。
        assert_eq!(load_state(&app_data).unwrap().unwrap().manifest, manifest);
    }

    #[test]
    fn hash_mismatch_is_rejected_and_staging_is_removed() {
        let temp = TempDir::new("hash");
        let app_data = temp.path().join("data");
        let manifest = manifest_with(vec![manifest_file("snapshot/settings.json", "{\"a\":1}")]);
        let (client, _) = fake_client(vec![
            response_with_body(&manifest_bytes(&manifest)),
            // 内容被改过：字节数一致，哈希不同。
            response_with_body(b"{\"a\":2}"),
        ]);

        let error = download(&client, &app_data).expect_err("哈希不符应失败");

        match error {
            TransferError::HashMismatch { path, .. } => {
                assert_eq!(path, "snapshot/settings.json")
            }
            other => panic!("期望 HashMismatch，得到 {other:?}"),
        }
        // 本次暂存目录被整份删除，远端状态没有建立。
        assert_eq!(staging_entries(&app_data), Vec::<String>::new());
        assert!(!app_data.join(REMOTE_STATE_FILE_NAME).exists());
    }

    #[test]
    fn size_mismatch_is_rejected_before_hashing() {
        let temp = TempDir::new("size");
        let app_data = temp.path().join("data");
        let mut file = manifest_file("snapshot/settings.json", "{\"a\":1}");
        file.size += 1;
        let manifest = manifest_with(vec![file]);
        let (client, _) = fake_client(vec![
            response_with_body(&manifest_bytes(&manifest)),
            response_with_body(b"{\"a\":1}"),
        ]);

        let error = download(&client, &app_data).expect_err("字节数不符应失败");

        match error {
            TransferError::SizeMismatch {
                path,
                expected,
                actual,
            } => {
                assert_eq!(path, "snapshot/settings.json");
                assert_eq!(expected, 8);
                assert_eq!(actual, 7);
            }
            other => panic!("期望 SizeMismatch，得到 {other:?}"),
        }
        assert_eq!(staging_entries(&app_data), Vec::<String>::new());
    }

    #[test]
    fn out_of_bounds_paths_are_rejected_without_downloading() {
        let temp = TempDir::new("escape");
        let app_data = temp.path().join("data");

        for path in [
            "snapshot/../../evil.txt",
            "/etc/passwd",
            "./snapshot/settings.json",
        ] {
            let manifest = manifest_with(vec![ManifestFile {
                path: path.to_string(),
                sha256: snapshot::sha256_hex(b"x"),
                size: 1,
                kind: "text".to_string(),
            }]);
            let (client, transport) =
                fake_client(vec![response_with_body(&manifest_bytes(&manifest))]);

            let error = download(&client, &app_data).expect_err("越界路径应整次拒绝");

            match error {
                TransferError::OutOfBounds(rejected) => assert_eq!(rejected, path),
                other => panic!("期望 OutOfBounds，得到 {other:?}"),
            }
            // 只有 manifest 那一次 GET：越界路径不会被发出去。
            assert_eq!(transport.requests().len(), 1, "路径 {path}");
            assert_eq!(staging_entries(&app_data), Vec::<String>::new());
        }
    }

    #[test]
    fn missing_manifest_means_not_found() {
        let temp = TempDir::new("missing");
        let app_data = temp.path().join("data");
        let (client, _) = fake_client(vec![response(404)]);

        assert_eq!(download(&client, &app_data), Err(TransferError::NotFound));
        // 连暂存目录都不建。
        assert!(!app_data.join(STAGING_DIR_NAME).exists());
    }

    #[test]
    fn oversized_manifest_is_rejected() {
        let temp = TempDir::new("big-manifest");
        let app_data = temp.path().join("data");
        let body = vec![b' '; MAX_MANIFEST_BYTES + 1];
        let (client, _) = fake_client(vec![response_with_body(&body)]);

        match download(&client, &app_data) {
            Err(TransferError::TooLarge(detail)) => {
                assert!(detail.contains("manifest"), "{detail}")
            }
            other => panic!("期望 TooLarge，得到 {other:?}"),
        }
        assert!(!app_data.join(STAGING_DIR_NAME).exists());
    }

    #[test]
    fn oversized_file_is_rejected_without_downloading() {
        let temp = TempDir::new("big-file");
        let app_data = temp.path().join("data");
        let manifest = manifest_with(vec![ManifestFile {
            path: "snapshot/huge.bin".to_string(),
            sha256: snapshot::sha256_hex(b"x"),
            size: MAX_FILE_BYTES + 1,
            kind: "binary".to_string(),
        }]);
        let (client, transport) = fake_client(vec![response_with_body(&manifest_bytes(&manifest))]);

        match download(&client, &app_data) {
            Err(TransferError::TooLarge(detail)) => {
                assert!(detail.contains("snapshot/huge.bin"), "{detail}")
            }
            other => panic!("期望 TooLarge，得到 {other:?}"),
        }
        // 声明的体积就超限：不应该真的把文件拉下来。
        assert_eq!(transport.requests().len(), 1);
        assert_eq!(staging_entries(&app_data), Vec::<String>::new());
    }

    #[test]
    fn unsupported_format_version_is_corrupt() {
        let temp = TempDir::new("version");
        let app_data = temp.path().join("data");
        let mut manifest = manifest_with(Vec::new());
        manifest.format_version = FORMAT_VERSION + 1;
        let (client, _) = fake_client(vec![response_with_body(&manifest_bytes(&manifest))]);

        match download(&client, &app_data) {
            Err(TransferError::Corrupt(detail)) => {
                assert!(detail.contains("formatVersion"), "{detail}")
            }
            other => panic!("期望 Corrupt，得到 {other:?}"),
        }
        // 不是合法 JSON / 缺字段同样按损坏处理。
        let (broken_client, _) = fake_client(vec![response_with_body(b"{ not json")]);
        assert!(matches!(
            download(&broken_client, &app_data),
            Err(TransferError::Corrupt(detail)) if detail.contains("合法 JSON")
        ));
    }

    #[test]
    fn missing_listed_file_is_reported() {
        let temp = TempDir::new("listed-missing");
        let app_data = temp.path().join("data");
        let manifest = manifest_with(vec![manifest_file("snapshot/settings.json", "{}")]);
        let (client, _) = fake_client(vec![
            response_with_body(&manifest_bytes(&manifest)),
            response(404),
        ]);

        match download(&client, &app_data) {
            Err(TransferError::Missing(path)) => assert_eq!(path, "snapshot/settings.json"),
            other => panic!("期望 Missing，得到 {other:?}"),
        }
        assert_eq!(staging_entries(&app_data), Vec::<String>::new());
    }

    #[test]
    fn success_prunes_older_staging_dirs() {
        let temp = TempDir::new("prune");
        let app_data = temp.path().join("data");
        let stale = app_data.join(STAGING_DIR_NAME).join("2026-11-01T08-00-00");
        fs::create_dir_all(stale.join("snapshot")).unwrap();
        fs::write(stale.join("snapshot/settings.json"), "stale").unwrap();

        let manifest = manifest_with(vec![manifest_file("snapshot/settings.json", "{}")]);
        let (client, _) = fake_client(vec![
            response_with_body(&manifest_bytes(&manifest)),
            response_with_body(b"{}"),
        ]);

        let outcome = download(&client, &app_data).expect("下载应成功");

        assert_eq!(
            staging_entries(&app_data),
            vec![outcome
                .staging_root
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()]
        );
        assert!(!stale.exists());
    }

    #[test]
    fn corrupt_state_file_blocks_download_without_staging() {
        let temp = TempDir::new("corrupt-state");
        let app_data = temp.path().join("data");
        fs::create_dir_all(&app_data).unwrap();
        fs::write(app_data.join(REMOTE_STATE_FILE_NAME), "{ not json").unwrap();

        let manifest = manifest_with(Vec::new());
        let (client, _) = fake_client(vec![response_with_body(&manifest_bytes(&manifest))]);

        match download(&client, &app_data) {
            Err(TransferError::Storage(detail)) => {
                assert!(detail.contains(REMOTE_STATE_FILE_NAME), "{detail}")
            }
            other => panic!("期望 Storage，得到 {other:?}"),
        }
        // 状态文件原样保留，暂存目录没有建立。
        assert_eq!(
            fs::read_to_string(app_data.join(REMOTE_STATE_FILE_NAME)).unwrap(),
            "{ not json"
        );
        assert!(!app_data.join(STAGING_DIR_NAME).exists());
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
                stdout: "node:22.23.0\npi:1.5.2\n".to_string(),
                stderr: String::new(),
            }
        }
    }

    #[test]
    fn staging_root_drives_the_existing_plan_and_apply() {
        let temp = TempDir::new("plan-apply");
        let app_data = temp.path().join("data");
        let agent_dir = temp.path().join("agent");
        fs::create_dir_all(&agent_dir).unwrap();

        let settings = "{\"theme\":\"dark\"}";
        let manifest = manifest_with(vec![manifest_file("snapshot/settings.json", settings)]);
        let (client, _) = fake_client(vec![
            response_with_body(&manifest_bytes(&manifest)),
            response_with_body(settings.as_bytes()),
        ]);

        let outcome = download(&client, &app_data).expect("下载应成功");

        // 暂存目录顶替同步根：只读校验与计划一行都不用改。
        let report = crate::plan::plan_snapshot(&agent_dir, &outcome.staging_root, &FakeRunner)
            .expect("计划应成功");
        assert_eq!(report.manifest_info.fingerprint, outcome.manifest.fingerprint);
        assert_eq!(report.verification.checked, 1);
        assert_eq!(report.plan.added.len(), 1);
        assert_eq!(report.plan.added[0].path, "settings.json");
        assert!(!agent_dir.join("settings.json").exists(), "计划不写 live");

        // 应用也从暂存目录读内容，备份仍落在应用数据目录。
        let applied = crate::apply::apply(
            &agent_dir,
            &outcome.staging_root,
            &outcome.manifest.fingerprint,
            &[],
            &app_data,
            &FakeRunner,
        )
        .expect("应用应成功");
        assert_eq!(applied.written, 1);
        assert_eq!(
            fs::read_to_string(agent_dir.join("settings.json")).unwrap(),
            settings
        );
        assert!(applied.backup_dir.starts_with(app_data.join("backups")));
    }
}

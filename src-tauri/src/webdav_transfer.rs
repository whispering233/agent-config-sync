//! 快照在 WebDAV 与本机之间的搬运。
//!
//! 规则来源：`docs/design/40-webdav.md` 第 5、6、7 节。这里只做搬运与校验，不决定快照内容：
//! 下行（[`download`]）读远端 manifest，逐文件下载到 `<应用数据目录>/staging/<时间戳>/`，
//! 逐个核对字节数与 sha256，全部通过后才写 manifest 与远端状态；上行（[`upload`]）先用
//! `snapshot::export` 把 live 导出到本机工作区（脱敏、扫描、manifest、指纹完全复用），
//! 再按远端 manifest 只补变化的文件，最后条件写 manifest。计划、备份与落地仍走
//! `plan` / `apply`，本模块不碰 live。
//!
//! 远端 manifest 是不可信输入：路径必须落在暂存目录内、文件有体积上限、内容必须与哈希一致。
//! 任一失败都删掉本次暂存目录，远端状态保持原样。

use crate::config;
use crate::platform;
use crate::scope;
use crate::snapshot::{self, ExportFailure, Manifest, FORMAT_VERSION, MANIFEST_FILE_NAME};
use crate::webdav::{DavError, DavTransport, WebdavClient};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 暂存目录名：位于应用数据目录下，与 `backups/` 同级，不进云盘也不进快照。
pub const STAGING_DIR_NAME: &str = "staging";
/// 上传用的临时工作区：只活在一次上传期间，成功或失败都整份删掉。
///
/// 不能放进 `staging/`：那里保留着「下载下来的、可能正要被应用」的快照，混在一起会被误当成本机型快照。
pub const UPLOAD_WORKSPACE_DIR_NAME: &str = "upload-workspace";
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

/// 一次上传的结局。冲突不是错误：用户还没决定用哪一边，远端一个字节都没动。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadOutcome {
    Uploaded {
        fingerprint: String,
        /// 真正 PUT 上去的文件数。
        uploaded: usize,
        /// 与远端已一致、跳过的文件数。
        skipped: usize,
        /// 本次上传后记录的远端状态时间。
        seen_at: String,
    },
    Conflict {
        remote_fingerprint: String,
        /// 下面三个清单描述**远端相对上次记录**的变化（新增 / 修改 / 删除的路径）。
        /// 本机没有记录时三个清单都是空的：没有基准，算不出差异。
        added: Vec<String>,
        modified: Vec<String>,
        removed: Vec<String>,
        /// 远端清单存在但无法解析：只能覆盖，没法先下载。
        remote_corrupt: bool,
    },
}

/// 传输失败（上行与下行共用）。网络类错误保留 [`DavError`]，command 层统一翻译成一句话。
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
    /// 导出当前 live 失败：宿主目录不可用、明文密钥未确认等。
    ///
    /// 保留原结构是为了让前端能把 `secrets_unconfirmed` 的确认流程接上去。
    Export(ExportFailure),
    /// 提交 manifest 时条件请求未通过：远端在本次上传期间被改写过。
    RemoteChanged,
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
            Self::Export(failure) => write!(formatter, "{}", failure.message),
            Self::RemoteChanged => write!(
                formatter,
                "远端 manifest 在本次上传期间被改写过，条件写未通过；请重新上传，让两边先对齐"
            ),
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
/// 返回本次记录的时间，上传成功时直接报给前端，省一次读取。
pub fn save_state(app_data: &Path, manifest: &Manifest) -> Result<String, String> {
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

    Ok(state.seen_at)
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
        .and_then(|()| {
            save_state(app_data, &manifest)
                .map(|_| ())
                .map_err(TransferError::Storage)
        });
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

/// 把当前 live 导出并上传到 WebDAV。
///
/// 顺序固定：导出到本机工作区 → 读远端 manifest → 冲突判定 → 建目录 → 传变化文件 →
/// 条件写 manifest → 记录远端状态。manifest 之前的任何失败都不更新远端状态：远端旧
/// manifest 仍指向旧快照，不会出现「半套快照被应用」。冲突且未确认时一个字节都不写远端。
pub fn upload<T: DavTransport>(
    client: &WebdavClient<T>,
    app_data: &Path,
    agent_dir: &Path,
    allow_plaintext: bool,
    overwrite: bool,
) -> Result<UploadOutcome, TransferError> {
    let workspace = prepare_upload_workspace(app_data)?;
    let input = UploadInput {
        app_data,
        agent_dir,
        workspace: &workspace,
    };
    let outcome = upload_into(client, &input, allow_plaintext, overwrite);
    // 工作区只是本次搬运的中间产物：内容在 live 与远端都还在，留着只会占地方。
    let _ = fs::remove_dir_all(app_data.join(UPLOAD_WORKSPACE_DIR_NAME));
    outcome
}

/// 上传的固定输入。打包起来是为了让核心函数保持在五个入参以内。
struct UploadInput<'a> {
    app_data: &'a Path,
    agent_dir: &'a Path,
    workspace: &'a Path,
}

/// 远端 manifest 与它的 ETag。ETag 用来防止提交时盖掉别人刚推的版本。
struct RemoteManifest {
    etag: Option<String>,
    manifest: Manifest,
}

fn upload_into<T: DavTransport>(
    client: &WebdavClient<T>,
    input: &UploadInput<'_>,
    allow_plaintext: bool,
    overwrite: bool,
) -> Result<UploadOutcome, TransferError> {
    // 1. 导出：脱敏、明文扫描、manifest、指纹全部复用快照流水线，不另写一套。
    snapshot::export(input.agent_dir, input.workspace, allow_plaintext)
        .map_err(TransferError::Export)?;
    let manifest = read_workspace_manifest(input.workspace)?;

    // 2. 读远端 manifest：不存在表示首次上传，可解析则拿它判冲突、做条件写。
    let (remote, corrupt_etag) = match read_remote_manifest(client)? {
        RemoteManifestState::Missing => (None, None),
        RemoteManifestState::Parsed(remote) => (Some(remote), None),
        // 清单坏了就没有可比的基准：没确认覆盖就停下来，别把远端越修越坏。
        RemoteManifestState::Corrupt { .. } if !overwrite => {
            return Ok(UploadOutcome::Conflict {
                remote_fingerprint: String::new(),
                added: Vec::new(),
                modified: Vec::new(),
                removed: Vec::new(),
                remote_corrupt: true,
            });
        }
        RemoteManifestState::Corrupt { etag } => (None, etag),
    };
    // 状态文件损坏时先停：它记着「上次看到的远端」，忽略它就等于放行任何覆盖。
    let recorded = load_state(input.app_data).map_err(TransferError::Storage)?;

    // 3. 冲突判定：只认 fingerprint（40-webdav §7）。
    if let Some(remote) = &remote {
        let changed = match &recorded {
            // 没有记录（换机器、换地址、删过状态文件）按保守冲突处理。
            None => true,
            Some(state) => state.manifest.fingerprint != remote.manifest.fingerprint,
        };
        if changed && !overwrite {
            let (added, modified, removed) = match &recorded {
                Some(state) => diff_manifests(&remote.manifest, &state.manifest),
                // 没有基准就算不出差异，清单留空，由前端文案解释。
                None => (Vec::new(), Vec::new(), Vec::new()),
            };
            return Ok(UploadOutcome::Conflict {
                remote_fingerprint: remote.manifest.fingerprint.clone(),
                added,
                modified,
                removed,
                remote_corrupt: false,
            });
        }
    }

    // 4. 建目录：工作根幂等确保，再按文件的父目录从浅到深逐个建。
    client.mkcol("").map_err(TransferError::Network)?;
    for dir in parent_dirs(&manifest) {
        client.mkcol(&dir).map_err(TransferError::Network)?;
    }

    // 5. 传文件：与远端 manifest 逐项比 path + sha256，只 PUT 变化的。
    let known = remote
        .as_ref()
        .map(|remote| hash_index(&remote.manifest))
        .unwrap_or_default();
    let mut uploaded = 0;
    let mut skipped = 0;
    for file in &manifest.files {
        if known.get(&file.path) == Some(&file.sha256) {
            skipped += 1;
            continue;
        }
        let bytes = fs::read(input.workspace.join(&file.path)).map_err(|error| {
            TransferError::Storage(format!("读取待上传文件 {} 失败：{error}", file.path))
        })?;
        client
            .put(&file.path, &bytes, &[])
            .map_err(TransferError::Network)?;
        uploaded += 1;
    }

    // 6. 提交 manifest。条件头缺一不可：无条件写就会盖掉别人刚推的版本。
    let body = fs::read(input.workspace.join(MANIFEST_FILE_NAME))
        .map_err(|error| TransferError::Storage(format!("读取待上传 manifest 失败：{error}")))?;
    client
        .put(
            MANIFEST_FILE_NAME,
            &body,
            &commit_headers(remote.as_ref(), corrupt_etag.as_deref()),
        )
        .map_err(|error| match error {
            DavError::PreconditionFailed => TransferError::RemoteChanged,
            other => TransferError::Network(other),
        })?;

    // 7. manifest 落盘即代表本次上传完整：到这里才敢更新本机记录。
    let seen_at = save_state(input.app_data, &manifest).map_err(TransferError::Storage)?;

    Ok(UploadOutcome::Uploaded {
        fingerprint: manifest.fingerprint,
        uploaded,
        skipped,
        seen_at,
    })
}

/// 读回刚导出的 manifest：上传的路径清单以它为准，不再从目录重新拼一遍。
fn read_workspace_manifest(workspace: &Path) -> Result<Manifest, TransferError> {
    let bytes = fs::read(workspace.join(MANIFEST_FILE_NAME))
        .map_err(|error| TransferError::Storage(format!("读取临时 manifest 失败：{error}")))?;
    parse_manifest(&bytes)
}

/// 读远端 manifest。404 → 首次上传；其余仍按不可信输入解析。
/// 远端清单的三种形态：不存在、可解析、存在但坏了（仍带着它的 ETag）。
enum RemoteManifestState {
    Missing,
    Parsed(RemoteManifest),
    Corrupt { etag: Option<String> },
}

fn read_remote_manifest<T: DavTransport>(
    client: &WebdavClient<T>,
) -> Result<RemoteManifestState, TransferError> {
    let Some((bytes, etag)) = client
        .get(MANIFEST_FILE_NAME)
        .map_err(TransferError::Network)?
    else {
        return Ok(RemoteManifestState::Missing);
    };
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(TransferError::TooLarge(format!(
            "远端 manifest 有 {} 字节，超过 {MAX_MANIFEST_BYTES} 字节上限",
            bytes.len()
        )));
    }

    match parse_manifest(&bytes) {
        Ok(manifest) => Ok(RemoteManifestState::Parsed(RemoteManifest {
            etag,
            manifest,
        })),
        // 清单坏了但 ETag 还在：覆盖时仍能靠条件请求确认“坏的就是我读到的那一版”。
        Err(_) => Ok(RemoteManifestState::Corrupt { etag }),
    }
}

/// 提交 manifest 的条件头。
/// - 远端清单损坏：ETag 若在则仍然条件写，避免盖掉别人刚修好的版本
/// - 远端原本有清单：用它读到的 ETag（服务端不给 ETag 时只能无条件写）
/// - 远端没有清单：`If-None-Match: *`，防止两台机器同时“首次上传”
fn commit_headers(
    remote: Option<&RemoteManifest>,
    corrupt_etag: Option<&str>,
) -> Vec<(String, String)> {
    if let Some(etag) = corrupt_etag {
        return vec![("If-Match".to_string(), etag.to_string())];
    }
    match remote {
        Some(remote) => match &remote.etag {
            Some(etag) => vec![("If-Match".to_string(), etag.clone())],
            // 服务端没给 ETag 就没有可比对的版本，只能无条件写。
            None => Vec::new(),
        },
        None => vec![("If-None-Match".to_string(), "*".to_string())],
    }
}

/// path → sha256，用于「只传变化文件」的比对。
fn hash_index(manifest: &Manifest) -> BTreeMap<String, String> {
    manifest
        .files
        .iter()
        .map(|file| (file.path.clone(), file.sha256.clone()))
        .collect()
}

/// 远端相对上次记录的变化：新增 / 修改 / 删除的路径。
///
/// 方向是「远端 vs 记录」，因为这正是用户要判断的那件事：云端被别人改了什么。
fn diff_manifests(
    remote: &Manifest,
    recorded: &Manifest,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let recorded_index = hash_index(recorded);
    let remote_index = hash_index(remote);

    let mut added = Vec::new();
    let mut modified = Vec::new();
    for (path, sha256) in &remote_index {
        match recorded_index.get(path) {
            None => added.push(path.clone()),
            Some(previous) if previous != sha256 => modified.push(path.clone()),
            Some(_) => {}
        }
    }
    let removed = recorded_index
        .keys()
        .filter(|path| !remote_index.contains_key(*path))
        .cloned()
        .collect();

    (added, modified, removed)
}

/// manifest 里出现过的全部父目录：去重后按层数从浅到深排列。
fn parent_dirs(manifest: &Manifest) -> Vec<String> {
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for file in &manifest.files {
        let mut current = Path::new(&file.path).parent();
        while let Some(dir) = current {
            let text = dir.to_string_lossy().replace('\\', "/");
            if text.is_empty() {
                break;
            }
            dirs.insert(text);
            current = dir.parent();
        }
    }

    let mut ordered: Vec<String> = dirs.into_iter().collect();
    // 父目录必须先于子目录建出来；同层按名字排序，请求顺序可复现。
    ordered.sort_by(|left, right| {
        left.matches('/')
            .count()
            .cmp(&right.matches('/').count())
            .then_with(|| left.cmp(right))
    });
    ordered
}

/// 建本次上传的临时工作区。上次进程崩溃留下的半成品没有复用价值，整棵树先清掉。
fn prepare_upload_workspace(app_data: &Path) -> Result<PathBuf, TransferError> {
    let root = app_data.join(UPLOAD_WORKSPACE_DIR_NAME);
    if root.exists() {
        fs::remove_dir_all(&root).map_err(storage_error)?;
    }
    fs::create_dir_all(&root).map_err(storage_error)?;
    // create_dir_all 按 umask 建，补一次显式权限：工作区里可能有明文密钥（Windows 上不设）。
    platform::set_mode(&root, STAGING_DIR_MODE).map_err(storage_error)?;

    let dir = root.join(snapshot::format_timestamp(now_seconds()).replace(':', "-"));
    platform::create_dir_all_mode(&dir, STAGING_DIR_MODE).map_err(storage_error)?;

    Ok(dir)
}

/// 解析远端 manifest。版本单独先看：结构不认识时也要报「版本不支持」而不是「文件损坏」。
fn parse_manifest(bytes: &[u8]) -> Result<Manifest, TransferError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| TransferError::Corrupt(format!("远端 manifest 不是合法 JSON：{error}")))?;

    match value
        .get("formatVersion")
        .and_then(serde_json::Value::as_u64)
    {
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

    serde_json::from_value(value)
        .map_err(|error| TransferError::Corrupt(format!("远端 manifest 字段不完整：{error}")))
}

/// 建本次暂存目录。命名与备份目录同一套：UTC 时间戳，同一秒内加序号。
fn create_staging_dir(app_data: &Path) -> Result<PathBuf, TransferError> {
    let root = app_data.join(STAGING_DIR_NAME);
    fs::create_dir_all(&root).map_err(storage_error)?;
    // create_dir_all 按 umask 建，补一次显式权限（Windows 上不设）。
    platform::set_mode(&root, STAGING_DIR_MODE).map_err(storage_error)?;

    let base = snapshot::format_timestamp(now_seconds()).replace(':', "-");
    let mut name = base.clone();
    let mut suffix = 1;
    while root.join(&name).exists() {
        name = format!("{base}-{suffix}");
        suffix += 1;
    }

    let dir = root.join(&name);
    platform::create_dir_all_mode(&dir, STAGING_DIR_MODE).map_err(storage_error)?;

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
    config::write_private_file(&target, bytes)
        .map_err(|error| TransferError::Storage(format!("写入暂存 manifest 失败：{error}")))
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
        assert_eq!(
            relative.components().count(),
            1,
            "暂存目录是 staging 的直接子目录"
        );
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
            use std::os::unix::fs::PermissionsExt;
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

        let manifest = manifest_with(vec![manifest_file(
            "snapshot/settings.json",
            "{ \"a\": 1 }",
        )]);
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
        assert_eq!(
            report.manifest_info.fingerprint,
            outcome.manifest.fingerprint
        );
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

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().expect("文件必须有父目录")).unwrap();
        fs::write(path, content).unwrap();
    }

    /// 真实 live 夹具：上传要真的跑一遍导出流水线，不能拿假 manifest 顶替。
    fn fixture_agent(root: &Path) {
        write_file(&root.join("settings.json"), "{\"theme\":\"dark\"}");
        write_file(&root.join("skills/antd/SKILL.md"), "# 技能\n");
        write_file(&root.join("prompts/review.md"), "# 评审\n");
    }

    fn write_state(app_data: &Path, manifest: &Manifest, seen_at: &str) {
        fs::create_dir_all(app_data).unwrap();
        let state = RemoteState {
            seen_at: seen_at.to_string(),
            manifest: manifest.clone(),
        };
        fs::write(
            app_data.join(REMOTE_STATE_FILE_NAME),
            serde_json::to_string_pretty(&state).unwrap(),
        )
        .unwrap();
    }

    fn with_etag(bytes: &[u8], etag: &str) -> Result<DavResponse, DavError> {
        Ok(DavResponse {
            status: 200,
            headers: vec![("ETag".to_string(), etag.to_string())],
            body: bytes.to_vec(),
        })
    }

    /// 首次上传的响应序列：404 后给足 201；多出来的响应不会被消费，不影响断言。
    fn first_upload_responses() -> Vec<Result<DavResponse, DavError>> {
        let mut responses = vec![response(404)];
        responses.extend((0..24).map(|_| response(201)));
        responses
    }

    /// 请求压成 `方法 相对路径`，方便整体断言顺序。
    fn calls(requests: &[DavRequest]) -> Vec<String> {
        let base = format!("{BASE}/");
        requests
            .iter()
            .map(|request| {
                let path = request
                    .url
                    .strip_prefix(&base)
                    .unwrap_or(&request.url)
                    .to_string();
                format!("{} {path}", request.method)
            })
            .collect()
    }

    fn header(request: &DavRequest, name: &str) -> Option<String> {
        request
            .headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    #[test]
    fn upload_first_time_creates_dirs_and_commits_manifest_last() {
        let temp = TempDir::new("upload-first");
        let app_data = temp.path().join("data");
        let agent = temp.path().join("agent");
        fixture_agent(&agent);
        let (client, transport) = fake_client(first_upload_responses());

        let outcome = upload(&client, &app_data, &agent, false, false).expect("首次上传应成功");

        let fingerprint = match &outcome {
            UploadOutcome::Uploaded {
                fingerprint,
                uploaded,
                skipped,
                seen_at,
            } => {
                assert_eq!(*uploaded, 3);
                assert_eq!(*skipped, 0);
                assert_eq!(fingerprint.len(), 12);
                assert_eq!(seen_at.len(), 20);
                fingerprint.clone()
            }
            other => panic!("期望 Uploaded，得到 {other:?}"),
        };

        // 顺序：读远端 → 建根 → 建父目录（浅到深）→ 传文件 → 最后写 manifest。
        assert_eq!(
            calls(&transport.requests()),
            vec![
                "GET PiAgentSync/manifest.json",
                "MKCOL PiAgentSync",
                "MKCOL PiAgentSync/snapshot",
                "MKCOL PiAgentSync/snapshot/prompts",
                "MKCOL PiAgentSync/snapshot/skills",
                "MKCOL PiAgentSync/snapshot/skills/antd",
                "PUT PiAgentSync/snapshot/prompts/review.md",
                "PUT PiAgentSync/snapshot/settings.json",
                "PUT PiAgentSync/snapshot/skills/antd/SKILL.md",
                "PUT PiAgentSync/manifest.json",
            ]
        );

        let requests = transport.requests();
        let commit = requests.last().expect("最后一次请求是提交 manifest");
        assert_eq!(header(commit, "If-None-Match").as_deref(), Some("*"));
        assert_eq!(header(commit, "If-Match"), None);
        let committed: Manifest = serde_json::from_slice(commit.body.as_deref().unwrap())
            .expect("提交的 manifest 应能解析");
        assert_eq!(committed.fingerprint, fingerprint);

        let settings = requests
            .iter()
            .find(|request| request.url.ends_with("snapshot/settings.json"))
            .expect("应 PUT settings.json");
        assert_eq!(settings.body.as_deref(), Some(&b"{\"theme\":\"dark\"}"[..]));

        // 远端状态记的是本次上传的 manifest；上传工作区不留在盘上。
        let state = load_state(&app_data).expect("读状态").expect("状态应存在");
        assert_eq!(state.manifest.fingerprint, fingerprint);
        assert!(!app_data.join(UPLOAD_WORKSPACE_DIR_NAME).exists());
        assert!(
            !app_data.join(STAGING_DIR_NAME).exists(),
            "上传不碰下载暂存目录"
        );
    }

    #[test]
    fn upload_stops_when_the_remote_manifest_is_corrupt() {
        let temp = TempDir::new("upload-corrupt-stop");
        let app_data = temp.path().join("data");
        let agent = temp.path().join("agent");
        fixture_agent(&agent);
        let corrupt = Ok(DavResponse {
            status: 200,
            headers: vec![("ETag".to_string(), "\"abc\"".to_string())],
            body: b"{ not json".to_vec(),
        });
        let (client, transport) = fake_client(vec![corrupt]);

        let outcome =
            upload(&client, &app_data, &agent, false, false).expect("应返回冲突而不是报错");

        match outcome {
            UploadOutcome::Conflict {
                remote_corrupt,
                added,
                modified,
                removed,
                ..
            } => {
                assert!(remote_corrupt);
                assert!(added.is_empty() && modified.is_empty() && removed.is_empty());
            }
            other => panic!("期望 Conflict，得到 {other:?}"),
        }
        // 只读了 manifest：其余内容一字节都没写。
        assert_eq!(calls(&transport.requests()).len(), 1);
        assert!(load_state(&app_data).expect("读状态").is_none());
    }

    #[test]
    fn upload_overwrites_a_corrupt_manifest_only_when_confirmed() {
        let temp = TempDir::new("upload-corrupt-overwrite");
        let app_data = temp.path().join("data");
        let agent = temp.path().join("agent");
        fixture_agent(&agent);
        let mut responses = vec![Ok(DavResponse {
            status: 200,
            headers: vec![("ETag".to_string(), "\"abc\"".to_string())],
            body: b"{ not json".to_vec(),
        })];
        responses.extend((0..24).map(|_| response(201)));
        let (client, transport) = fake_client(responses);

        let outcome = upload(&client, &app_data, &agent, false, true).expect("确认覆盖后应成功");

        match outcome {
            // 清单坏了就没法增量：全部重传。
            UploadOutcome::Uploaded {
                uploaded, skipped, ..
            } => {
                assert_eq!(uploaded, 3);
                assert_eq!(skipped, 0);
            }
            other => panic!("期望 Uploaded，得到 {other:?}"),
        }

        let requests = transport.requests();
        let commit = requests.last().expect("最后一次请求是提交 manifest");
        assert_eq!(header(commit, "If-Match").as_deref(), Some("\"abc\""));
        assert_eq!(header(commit, "If-None-Match"), None);
        assert!(load_state(&app_data).expect("读状态").is_some());
    }

    /// 真机互通检查：需要一个真实 WebDAV 服务器（本地 `webdav-server` 或坚果云均可）。
    /// 手动跑：`DAV_URL=… DAV_USER=… DAV_PASS=… cargo test interop -- --ignored --nocapture`
    /// 它验证的是假 transport 覆盖不到的部分：Basic 认证、真实 XML、中文路径编码、条件写入。
    /// 注意：会覆盖该地址下 `PiAgentSync/` 的内容，只对测试账号跑。
    #[test]
    #[ignore = "需要真实 WebDAV 服务器与凭据（DAV_URL / DAV_USER / DAV_PASS）"]
    fn interop_roundtrip_against_a_real_server() {
        let url = std::env::var("DAV_URL").expect("需要 DAV_URL");
        let user = std::env::var("DAV_USER").expect("需要 DAV_USER");
        let pass = std::env::var("DAV_PASS").expect("需要 DAV_PASS");

        let transport = crate::webdav::ReqwestTransport::new(&user, &pass).expect("构造 transport");
        let client = WebdavClient::new(transport, &url);

        let temp = TempDir::new("interop");
        let app_data = temp.path().join("data");
        let agent = temp.path().join("agent");
        fixture_agent(&agent);
        // 中文文件名：真实服务器上最容易出问题的一类路径。
        fs::write(agent.join("prompts/提案.md"), "中文文件名").expect("写中文文件");

        let probe = crate::webdav::probe(&client).expect("探测应通过");
        assert!(!probe.leftover_write_test_file, "测试服务器应允许 DELETE");

        // 云端可能留着上一轮的快照，而本机目录是新的：按设计会报保守冲突。
        // 测试要可重跑，所以确认覆盖一次，而不是把“上次运行的残留”当成失败。
        let fingerprint =
            match upload(&client, &app_data, &agent, false, false).expect("上传应成功") {
                UploadOutcome::Uploaded {
                    fingerprint,
                    uploaded,
                    skipped,
                    ..
                } => {
                    assert!(uploaded + skipped > 0);
                    fingerprint
                }
                UploadOutcome::Conflict { .. } => {
                    match upload(&client, &app_data, &agent, false, true).expect("确认覆盖后应成功")
                    {
                        UploadOutcome::Uploaded {
                            fingerprint,
                            uploaded,
                            skipped,
                            ..
                        } => {
                            assert!(uploaded + skipped > 0);
                            fingerprint
                        }
                        other => panic!("期望 Uploaded，得到 {other:?}"),
                    }
                }
            };

        // 同一份内容再传一次：不应该冲突，也不该重传任何文件。
        let again = upload(&client, &app_data, &agent, false, false).expect("二次上传应成功");
        match again {
            UploadOutcome::Uploaded {
                uploaded, skipped, ..
            } => {
                assert_eq!(uploaded, 0, "内容没变就不该再 PUT 文件");
                assert!(skipped > 0);
            }
            other => panic!("期望 Uploaded，得到 {other:?}"),
        }

        // 换一个应用数据目录当作新机器：下载、校验、指纹一致。
        let other_machine = temp.path().join("data2");
        let downloaded = download(&client, &other_machine).expect("下载应成功");
        assert_eq!(downloaded.manifest.fingerprint, fingerprint);
        let staged = downloaded.staging_root.join("snapshot/prompts/提案.md");
        assert_eq!(
            fs::read_to_string(staged).expect("读暂存文件"),
            "中文文件名"
        );
    }

    /// 条件写诊断：看服务端到底给不给 ETag、认不认 If-Match。
    /// 只在一个临时文件上试，不碰 manifest，跑完删掉。
    #[test]
    #[ignore = "需要真实 WebDAV 服务器与凭据（DAV_URL / DAV_USER / DAV_PASS）"]
    fn interop_probes_conditional_write() {
        let url = std::env::var("DAV_URL").expect("需要 DAV_URL");
        let user = std::env::var("DAV_USER").expect("需要 DAV_USER");
        let pass = std::env::var("DAV_PASS").expect("需要 DAV_PASS");

        let transport = crate::webdav::ReqwestTransport::new(&user, &pass).expect("构造 transport");
        let client = WebdavClient::new(transport, &url);

        client.mkcol("").expect("工作根应可创建或已存在");
        client
            .put("etag-probe.txt", b"one", &[])
            .expect("写探针文件");

        let (body, etag) = client
            .get("etag-probe.txt")
            .expect("读探针文件")
            .expect("探针文件应存在");
        assert_eq!(body, b"one");
        println!("GET 响应的 ETag: {etag:?}");

        let wrong = vec![("If-Match".to_string(), "\"definitely-wrong\"".to_string())];
        match client.put("etag-probe.txt", b"two", &wrong) {
            Err(DavError::PreconditionFailed) => {
                println!("If-Match 生效：错误 ETag 被拒（412）");
            }
            Ok(()) => {
                println!("⚠ 服务端忽略 If-Match：错误 ETag 仍然写入成功");
            }
            Err(other) => {
                println!("⚠ 条件写入返回了预期之外的结果：{other}");
            }
        }

        client.delete("etag-probe.txt").expect("清理探针文件");
    }

    #[test]
    fn upload_puts_only_files_that_differ_from_the_remote_manifest() {
        let temp = TempDir::new("upload-incremental");
        let app_data = temp.path().join("data");
        let agent = temp.path().join("agent");
        fixture_agent(&agent);

        // 先完整上传一次，拿到远端真实存在的 manifest（状态里存的就是它）。
        let (first_client, _) = fake_client(first_upload_responses());
        upload(&first_client, &app_data, &agent, false, false).expect("首次上传应成功");
        let remote = load_state(&app_data).unwrap().unwrap().manifest;

        write_file(&agent.join("settings.json"), "{\"theme\":\"light\"}");
        let mut responses = vec![with_etag(&manifest_bytes(&remote), "\"v1\"")];
        responses.extend((0..24).map(|_| response(201)));
        let (client, transport) = fake_client(responses);

        let outcome = upload(&client, &app_data, &agent, false, false).expect("增量上传应成功");

        match &outcome {
            UploadOutcome::Uploaded {
                uploaded, skipped, ..
            } => {
                assert_eq!(*uploaded, 1, "只有 settings.json 变了");
                assert_eq!(*skipped, 2, "没变的文件不该重传");
            }
            other => panic!("期望 Uploaded，得到 {other:?}"),
        }

        let requests = transport.requests();
        assert_eq!(
            calls(&requests),
            vec![
                "GET PiAgentSync/manifest.json",
                "MKCOL PiAgentSync",
                "MKCOL PiAgentSync/snapshot",
                "MKCOL PiAgentSync/snapshot/prompts",
                "MKCOL PiAgentSync/snapshot/skills",
                "MKCOL PiAgentSync/snapshot/skills/antd",
                "PUT PiAgentSync/snapshot/settings.json",
                "PUT PiAgentSync/manifest.json",
            ]
        );
        // 远端原本有清单：提交时要带它的 ETag。
        assert_eq!(
            header(requests.last().unwrap(), "If-Match").as_deref(),
            Some("\"v1\"")
        );

        let state = load_state(&app_data).unwrap().unwrap();
        assert_ne!(state.manifest.fingerprint, remote.fingerprint);
    }

    #[test]
    fn upload_without_record_conflicts_and_writes_nothing() {
        let temp = TempDir::new("upload-no-record");
        let app_data = temp.path().join("data");
        let agent = temp.path().join("agent");
        fixture_agent(&agent);
        let remote = manifest_with(vec![manifest_file(
            "snapshot/settings.json",
            "{\"theme\":\"dark\"}",
        )]);
        let (client, transport) = fake_client(vec![with_etag(&manifest_bytes(&remote), "\"v1\"")]);

        let outcome = upload(&client, &app_data, &agent, false, false).expect("冲突不是错误");

        // 没有基准就算不出差异：三个清单给空，由前端文案解释。
        assert_eq!(
            outcome,
            UploadOutcome::Conflict {
                remote_fingerprint: remote.fingerprint.clone(),
                added: Vec::new(),
                modified: Vec::new(),
                removed: Vec::new(),
                remote_corrupt: false,
            }
        );
        assert_eq!(
            transport.requests().len(),
            1,
            "冲突且未确认时不得写入任何远端内容"
        );
        assert!(!app_data.join(REMOTE_STATE_FILE_NAME).exists());
        assert!(!app_data.join(UPLOAD_WORKSPACE_DIR_NAME).exists());
    }

    #[test]
    fn upload_conflicts_when_the_recorded_fingerprint_differs() {
        let temp = TempDir::new("upload-conflict");
        let app_data = temp.path().join("data");
        let agent = temp.path().join("agent");
        fixture_agent(&agent);
        let recorded = manifest_with(vec![
            manifest_file("snapshot/settings.json", "{\"theme\":\"dark\"}"),
            manifest_file("snapshot/gone.md", "旧\n"),
        ]);
        let remote = manifest_with(vec![
            manifest_file("snapshot/settings.json", "{\"theme\":\"light\"}"),
            manifest_file("snapshot/new.md", "新\n"),
        ]);
        write_state(&app_data, &recorded, "2026-11-01T08:00:00Z");
        let (client, transport) = fake_client(vec![with_etag(&manifest_bytes(&remote), "\"v2\"")]);

        let outcome = upload(&client, &app_data, &agent, false, false).expect("冲突不是错误");

        assert_eq!(
            outcome,
            UploadOutcome::Conflict {
                remote_fingerprint: remote.fingerprint.clone(),
                added: vec!["snapshot/new.md".to_string()],
                modified: vec!["snapshot/settings.json".to_string()],
                removed: vec!["snapshot/gone.md".to_string()],
                remote_corrupt: false,
            }
        );
        assert_eq!(transport.requests().len(), 1, "冲突不建目录也不传文件");
        // 状态仍是上次记录：记录只能在成功提交 manifest 之后更新。
        let state = load_state(&app_data).unwrap().unwrap();
        assert_eq!(state.seen_at, "2026-11-01T08:00:00Z");
        assert_eq!(state.manifest, recorded);
    }

    #[test]
    fn upload_overwrite_replaces_the_remote_snapshot() {
        let temp = TempDir::new("upload-overwrite");
        let app_data = temp.path().join("data");
        let agent = temp.path().join("agent");
        fixture_agent(&agent);
        let recorded = manifest_with(vec![manifest_file("snapshot/gone.md", "旧\n")]);
        let remote = manifest_with(vec![manifest_file("snapshot/new.md", "新\n")]);
        write_state(&app_data, &recorded, "2026-11-01T08:00:00Z");
        let mut responses = vec![with_etag(&manifest_bytes(&remote), "\"v7\"")];
        responses.extend((0..24).map(|_| response(201)));
        let (client, transport) = fake_client(responses);

        let outcome = upload(&client, &app_data, &agent, false, true).expect("覆盖上传应成功");

        let fingerprint = match &outcome {
            UploadOutcome::Uploaded {
                fingerprint,
                uploaded,
                ..
            } => {
                assert_eq!(*uploaded, 3, "覆盖也要逐个补文件");
                fingerprint.clone()
            }
            other => panic!("期望 Uploaded，得到 {other:?}"),
        };
        // 覆盖写仍以远端 ETag 为条件：期间别人又改过就还是 412。
        assert_eq!(
            header(transport.requests().last().unwrap(), "If-Match").as_deref(),
            Some("\"v7\"")
        );
        let state = load_state(&app_data).unwrap().unwrap();
        assert_eq!(state.manifest.fingerprint, fingerprint);
        assert_ne!(fingerprint, remote.fingerprint);
    }

    #[test]
    fn upload_reports_remote_changed_on_412_and_keeps_the_state() {
        let temp = TempDir::new("upload-412");
        let app_data = temp.path().join("data");
        let agent = temp.path().join("agent");
        fixture_agent(&agent);
        let (first_client, _) = fake_client(first_upload_responses());
        upload(&first_client, &app_data, &agent, false, false).expect("首次上传应成功");
        let before = load_state(&app_data).unwrap().unwrap();

        // 无损内容变：文件全部跳过，直接提交 manifest，服务端回 412。
        // 根 MKCOL + 4 个父目录 MKCOL 之后才是提交。
        let mut responses = vec![with_etag(&manifest_bytes(&before.manifest), "\"v9\"")];
        responses.extend((0..5).map(|_| response(201)));
        responses.push(response(412));
        let (client, transport) = fake_client(responses);

        let error = upload(&client, &app_data, &agent, false, false).expect_err("412 应失败");

        assert_eq!(error, TransferError::RemoteChanged);
        assert_eq!(
            calls(&transport.requests()).last().unwrap(),
            "PUT PiAgentSync/manifest.json"
        );
        // 失败停在原地：记录与远端状态都不变。
        let after = load_state(&app_data).unwrap().unwrap();
        assert_eq!(after.seen_at, before.seen_at);
        assert_eq!(after.manifest, before.manifest);
        assert!(!app_data.join(UPLOAD_WORKSPACE_DIR_NAME).exists());
    }

    #[test]
    fn parent_dirs_are_deduplicated_and_ordered_shallow_first() {
        let manifest = manifest_with(vec![
            manifest_file("snapshot/skills/antd/SKILL.md", "a"),
            manifest_file("snapshot/skills/other/SKILL.md", "b"),
            manifest_file("snapshot/settings.json", "c"),
            manifest_file("manifest.json", "d"),
        ]);

        assert_eq!(
            parent_dirs(&manifest),
            vec![
                "snapshot".to_string(),
                "snapshot/skills".to_string(),
                "snapshot/skills/antd".to_string(),
                "snapshot/skills/other".to_string(),
            ]
        );
    }

    #[test]
    fn upload_stops_before_the_network_when_export_fails() {
        let temp = TempDir::new("upload-export-fail");
        let app_data = temp.path().join("data");
        let agent = temp.path().join("agent");

        // 宿主目录不存在：导出预检就失败，一个请求都不该发出去。
        let (client, transport) = fake_client(Vec::new());
        let error = upload(&client, &app_data, &agent, false, false).expect_err("导出失败应停下");
        match error {
            TransferError::Export(failure) => assert_eq!(failure.code, "agent_dir_missing"),
            other => panic!("期望 Export，得到 {other:?}"),
        }
        assert!(transport.requests().is_empty(), "导出失败不写远端");
        assert!(!app_data.join(REMOTE_STATE_FILE_NAME).exists());
        assert!(!app_data.join(UPLOAD_WORKSPACE_DIR_NAME).exists());
    }

    #[test]
    fn upload_blocks_on_plaintext_secrets_before_the_network() {
        let temp = TempDir::new("upload-plaintext");
        let app_data = temp.path().join("data");
        let agent = temp.path().join("agent");
        fixture_agent(&agent);
        // 范围外的普通脚本里出现已知前缀：不改写，只阻断，等用户确认。
        write_file(
            &agent.join("extensions/demo/index.ts"),
            "const apiKey = \"sk-Zx7Qv2Lm9Rt4Bc8Wn3Yp6Kd1\";\n",
        );

        let (client, transport) = fake_client(Vec::new());
        let error =
            upload(&client, &app_data, &agent, false, false).expect_err("明文密钥未确认时不能上传");
        match error {
            TransferError::Export(failure) => {
                assert_eq!(failure.code, "secrets_unconfirmed");
                assert!(!failure.findings.is_empty(), "要带上待确认条目");
            }
            other => panic!("期望 Export，得到 {other:?}"),
        }
        assert!(transport.requests().is_empty());

        // 用户确认后同一个入口能继续：复用导出的 allow_plaintext 语义。
        let (confirming_client, _) = fake_client(first_upload_responses());
        let outcome =
            upload(&confirming_client, &app_data, &agent, true, false).expect("确认后应上传成功");
        assert!(matches!(outcome, UploadOutcome::Uploaded { .. }));
    }
}

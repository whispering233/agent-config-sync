//! 包存储：列举、取、原子放入、删除、保留清理只有一套调用面，本地文件夹与 WebDAV 各一个实现。
//!
//! 规则来源：`docs/design/10-snapshot-sync.md` 第 2 节「存储目录布局」「历史与保留」，
//! `docs/design/40-webdav.md` 第 4、5 节。包名解析与 `.tmp-` 约定复用 [`crate::package`]：
//! 这一层不重判包名，也不认识 manifest 与快照内容。

use crate::package::{self, TEMP_NAME_PREFIX};
use crate::platform;
use crate::webdav::{DavError, DavTransport, RenameOutcome, WebdavClient};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 存储根的权限：包里有明文凭据，只允许本用户读写。
const STORE_DIR_MODE: u32 = 0o700;

/// 一条可解析的包。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreEntry {
    pub name: String,
    pub size: u64,
    /// 包名里的 UTC 秒。
    pub created_at: u64,
}

/// 一次列举。`unrecognized` 只有名字：这些条目调用方只展示，不参与「最新」判定，
/// 也不参与保留清理。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreListing {
    pub packages: Vec<StoreEntry>,
    pub unrecognized: Vec<String>,
}

/// 保留清理的结果。单个包删不掉只进 `failed`，不阻塞其它包，也不上抛。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneOutcome {
    pub pruned: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// 存储失败。`code` 取自 `docs/design/10-snapshot-sync.md` 第 9 节的失败码表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError {
    pub code: &'static str,
    pub message: String,
}

impl StoreError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// 本地目录读不了或写不了：沿用失败码表里「目标不可写」这一条。
    fn storage(message: impl Into<String>) -> Self {
        Self::new("no_write_permission", message)
    }

    fn move_failed(message: impl Into<String>) -> Self {
        Self::new("remote_move_failed", message)
    }

    fn prune_failed(message: impl Into<String>) -> Self {
        Self::new("remote_prune_failed", message)
    }

    /// 协议层失败落到失败码表的三条远端错误上：认证与配额能分辨，其余算不可达。
    fn from_dav(error: DavError) -> Self {
        let message = error.to_string();
        let code = match error {
            DavError::AuthFailed => "remote_auth_failed",
            DavError::Quota => "remote_quota",
            _ => "remote_unreachable",
        };

        Self::new(code, message)
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StoreError {}

/// 包存储。两种传输同构，差异只在实现里；五个动作全部阻塞，调用方按自己的线程调。
pub trait PackageStore {
    fn list(&self) -> Result<StoreListing, StoreError>;
    /// 包不存在返回 `Ok(None)`：它不是错误。
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, StoreError>;
    /// 原子定案：先写临时名，再改名到正式名。失败时清掉自己写的临时名。
    fn put(&self, name: &str, bytes: &[u8]) -> Result<(), StoreError>;
    fn delete(&self, name: &str) -> Result<(), StoreError>;
    fn prune(&self, keep: usize, current: &str) -> Result<PruneOutcome, StoreError>;
}

/// 保留清理的判定，逐条对应 `10-snapshot-sync.md` 第 2 节「历史与保留」：
/// 只考虑可解析的包、按名字倒序留前 `keep` 个、只删名字严格小于 `current` 的
/// （这一条同时保证本次刚上传的那份永远不被删）。
fn prune_candidates(names: &[&str], keep: usize, current: &str) -> Vec<String> {
    let mut ordered: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| package::parse_name(name).is_some())
        .collect();
    ordered.sort_unstable_by(|left, right| right.cmp(left));

    ordered
        .into_iter()
        .skip(keep)
        .filter(|name| *name < current)
        .map(str::to_string)
        .collect()
}

/// 逐项删除：一个包删不掉只进 `failed`，其它包照删。
fn prune_listing(
    listing: &StoreListing,
    keep: usize,
    current: &str,
    mut remove: impl FnMut(&str) -> Result<(), String>,
) -> PruneOutcome {
    let names: Vec<&str> = listing
        .packages
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    let mut outcome = PruneOutcome::default();
    for name in prune_candidates(&names, keep, current) {
        match remove(&name) {
            Ok(()) => outcome.pruned.push(name),
            Err(message) => outcome.failed.push((name, message)),
        }
    }
    outcome
}

/// 列举分类：只有严格符合包名的条目算包；`.tmp-*`、旧布局残留、云盘冲突副本与
/// 外来文件一律只记名字。
fn classify(name: &str, size: u64, listing: &mut StoreListing) {
    match package::parse_name(name) {
        Some(parsed) => listing.packages.push(StoreEntry {
            name: name.to_string(),
            size,
            created_at: parsed.unix_seconds,
        }),
        None => listing.unrecognized.push(name.to_string()),
    }
}

/// 临时名 `.tmp-<纳秒戳>-<fp8>`：fp8 借正式名里那一段，人工核对时对得上。
// 纳秒戳只在同一进程内保证唯一；`.tmp-*` 本来就是一次性名字，被覆盖不影响正确性。
fn temp_name(name: &str, nanos: u128) -> String {
    match package::parse_name(name) {
        Some(parsed) => format!("{TEMP_NAME_PREFIX}{nanos}-{}", parsed.fingerprint8),
        None => format!("{TEMP_NAME_PREFIX}{nanos}"),
    }
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
}

/// 名字必须是存储根的直接子项：带分隔符的名字会把读写引到根之外。
fn child_path(root: &Path, name: &str) -> Result<PathBuf, StoreError> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err(StoreError::storage(format!("不是存储根下的文件名：{name}")));
    }

    Ok(root.join(name))
}

fn storage_error(path: &Path, error: io::Error) -> StoreError {
    StoreError::storage(format!("无法读写存储目录 {}：{error}", path.display()))
}

/// 本地文件夹模式：存储根就是用户选的同步文件夹。
pub struct LocalStore {
    root: PathBuf,
}

impl LocalStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// 存储根。按路径读单个包（例如只读 manifest）时要用；包本身仍经由 trait 读写。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 根不存在时按 `0700` 建；已存在就不动权限，那是用户的目录。
    fn ensure_root(&self) -> Result<(), StoreError> {
        platform::create_dir_all_mode(&self.root, STORE_DIR_MODE)
            .map_err(|error| storage_error(&self.root, error))
    }
}

impl PackageStore for LocalStore {
    fn list(&self) -> Result<StoreListing, StoreError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(StoreListing::default())
            }
            Err(error) => return Err(storage_error(&self.root, error)),
        };

        let mut listing = StoreListing::default();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // 读不到元数据按 0 字节记：体积只用于展示，不该让整次列举失败。
            let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            classify(&name, size, &mut listing);
        }

        Ok(listing)
    }

    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let path = child_path(&self.root, name)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(storage_error(&path, error)),
        }
    }

    fn put(&self, name: &str, bytes: &[u8]) -> Result<(), StoreError> {
        let dest = child_path(&self.root, name)?;
        self.ensure_root()?;
        let temp = child_path(&self.root, &temp_name(name, now_nanos()))?;

        // 先写临时名再同盘改名：正式名下出现的包一定是完整的。
        if let Err(error) = fs::write(&temp, bytes) {
            let _ = fs::remove_file(&temp);
            return Err(storage_error(&temp, error));
        }
        if let Err(error) = fs::rename(&temp, &dest) {
            let _ = fs::remove_file(&temp);
            return Err(storage_error(&dest, error));
        }

        Ok(())
    }

    fn delete(&self, name: &str) -> Result<(), StoreError> {
        let path = child_path(&self.root, name)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            // 已经不在了就等于删掉了：清理重复删同一份时不该报错。
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(storage_error(&path, error)),
        }
    }

    fn prune(&self, keep: usize, current: &str) -> Result<PruneOutcome, StoreError> {
        let listing = self.list()?;

        Ok(prune_listing(&listing, keep, current, |name| {
            fs::remove_file(self.root.join(name)).map_err(|error| error.to_string())
        }))
    }
}

/// WebDAV 模式：工作根 `PiAgentSync/` 就是存储根。
pub struct WebdavStore<'a, T: DavTransport> {
    client: &'a WebdavClient<T>,
}

impl<'a, T: DavTransport> WebdavStore<'a, T> {
    pub fn new(client: &'a WebdavClient<T>) -> Self {
        Self { client }
    }

    /// 工作根不存在时建出来。`propfind` 对不存在的集合回 207 + 空 body，所以 `None`
    /// 才是「不存在」，空列表是「存在但空」。
    fn ensure_root(&self) -> Result<(), StoreError> {
        if self
            .client
            .propfind("")
            .map_err(StoreError::from_dav)?
            .is_none()
        {
            self.client.mkcol("").map_err(StoreError::from_dav)?;
        }

        Ok(())
    }
}

impl<T: DavTransport> PackageStore for WebdavStore<'_, T> {
    fn list(&self) -> Result<StoreListing, StoreError> {
        let mut listing = StoreListing::default();
        let entries = self.client.propfind("").map_err(StoreError::from_dav)?;
        for entry in entries.unwrap_or_default() {
            classify(&entry.path, entry.content_length.unwrap_or(0), &mut listing);
        }

        Ok(listing)
    }

    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self
            .client
            .get(name)
            .map_err(StoreError::from_dav)?
            .map(|(body, _)| body))
    }

    fn put(&self, name: &str, bytes: &[u8]) -> Result<(), StoreError> {
        self.ensure_root()?;
        let temp = temp_name(name, now_nanos());

        if let Err(error) = self.client.put(&temp, bytes, &[]) {
            let _ = self.client.delete(&temp);
            return Err(StoreError::from_dav(error));
        }

        match self.client.rename(&temp, name, false) {
            Ok(RenameOutcome::Renamed) => Ok(()),
            Ok(RenameOutcome::TargetExists) => {
                let _ = self.client.delete(&temp);
                Err(StoreError::move_failed(format!(
                    "远端已有同名包，无法定案：{name}"
                )))
            }
            Err(error) => {
                // 清临时名是 best-effort：清不掉只留残留，不改报错内容。
                let _ = self.client.delete(&temp);
                Err(StoreError::move_failed(format!(
                    "无法把临时包改名为正式名：{error}"
                )))
            }
        }
    }

    fn delete(&self, name: &str) -> Result<(), StoreError> {
        self.client.delete(name).map_err(StoreError::from_dav)
    }

    fn prune(&self, keep: usize, current: &str) -> Result<PruneOutcome, StoreError> {
        // 列不出来就没法清理：这是一种「整次清理失败」，由调用方单独上报，上传已经算成功。
        let listing = self
            .list()
            .map_err(|error| StoreError::prune_failed(error.message))?;

        Ok(prune_listing(&listing, keep, current, |name| {
            self.client.delete(name).map_err(|error| error.to_string())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webdav::{DavRequest, DavResponse};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU32, Ordering};

    const BASE: &str = "https://dav.example.com/dav";
    const ROOT_URL: &str = "https://dav.example.com/dav/PiAgentSync";

    /// 临时目录夹具：Drop 时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-store-{}-{}-{}",
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

    fn response(status: u16) -> Result<DavResponse, DavError> {
        Ok(DavResponse {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        })
    }

    fn response_with(status: u16, body: &str) -> Result<DavResponse, DavError> {
        Ok(DavResponse {
            status,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        })
    }

    fn header(request: &DavRequest, name: &str) -> Option<String> {
        request
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    fn methods(requests: &[DavRequest]) -> Vec<&str> {
        requests
            .iter()
            .map(|request| request.method.as_str())
            .collect()
    }

    /// 工作根存在但只有它自己一条。
    const ROOT_ONLY_MULTISTATUS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:"><d:response>
    <d:href>/dav/PiAgentSync/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
</d:response></d:multistatus>"#;

    const LISTING_MULTISTATUS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/PiAgentSync/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/PiAgentSync/20261122-105230-deadbeef.zip</d:href>
    <d:propstat><d:prop><d:resourcetype/><d:getcontentlength>4096</d:getcontentlength></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/PiAgentSync/20261121-091210-7f3e9c01.zip</d:href>
    <d:propstat><d:prop><d:resourcetype/><d:getcontentlength>2048</d:getcontentlength></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/PiAgentSync/.tmp-1795344750123456789-deadbeef</d:href>
    <d:propstat><d:prop><d:resourcetype/><d:getcontentlength>10</d:getcontentlength></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/PiAgentSync/manifest.json</d:href>
    <d:propstat><d:prop><d:resourcetype/><d:getcontentlength>12</d:getcontentlength></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/PiAgentSync/snapshot/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/PiAgentSync/20261122-105230-deadbeef%20(%E5%86%B2%E7%AA%81).zip</d:href>
    <d:propstat><d:prop><d:resourcetype/><d:getcontentlength>4096</d:getcontentlength></d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn local_put_get_list_delete_round_trip() {
        let temp = TempDir::new("round-trip");
        let root = temp.path().join("store");
        let store = LocalStore::new(root.clone());
        let name = package::build_name(1_795_344_750, "deadbeef");

        // 根还不存在：列举是空的，取不到就是 None，删不存在的包不报错。
        assert_eq!(store.list().unwrap(), StoreListing::default());
        assert_eq!(store.get(&name), Ok(None));
        assert_eq!(store.delete(&name), Ok(()));

        store.put(&name, b"package-bytes").unwrap();
        assert_eq!(store.get(&name).unwrap(), Some(b"package-bytes".to_vec()));
        assert_eq!(
            store.list().unwrap().packages,
            vec![StoreEntry {
                name: name.clone(),
                size: 13,
                created_at: 1_795_344_750,
            }]
        );

        // 临时名不该留在存储根里。
        let leftovers: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(TEMP_NAME_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "残留：{leftovers:?}");

        store.delete(&name).unwrap();
        assert_eq!(store.get(&name), Ok(None));
        assert!(store.list().unwrap().packages.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn local_root_is_created_with_0700() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new("mode");
        let root = temp.path().join("store");
        let store = LocalStore::new(root.clone());

        store
            .put(&package::build_name(1_795_344_750, "deadbeef"), b"x")
            .unwrap();

        let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn webdav_put_creates_root_then_renames_into_place() {
        let name = "20261122-105230-deadbeef.zip";
        let transport = FakeTransport::new(vec![
            response(404),
            response(201),
            response(201),
            response(201),
        ]);
        let client = WebdavClient::new(transport.clone(), BASE);
        let store = WebdavStore::new(&client);

        store.put(name, b"package-bytes").unwrap();

        let requests = transport.requests();
        assert_eq!(methods(&requests), vec!["PROPFIND", "MKCOL", "PUT", "MOVE"]);
        assert_eq!(requests[1].url, ROOT_URL);

        let temp = requests[2]
            .url
            .strip_prefix(&format!("{ROOT_URL}/"))
            .unwrap();
        assert!(temp.starts_with(TEMP_NAME_PREFIX), "临时名：{temp}");
        assert_eq!(requests[2].body.as_deref(), Some(&b"package-bytes"[..]));

        assert_eq!(
            requests[3].url,
            format!("{ROOT_URL}/{}", temp),
            "MOVE 的源必须是刚才的临时名"
        );
        assert_eq!(
            header(&requests[3], "Destination").as_deref(),
            Some(format!("{ROOT_URL}/{name}").as_str())
        );
        assert_eq!(header(&requests[3], "Overwrite").as_deref(), Some("F"));
    }

    #[test]
    fn webdav_move_destination_is_encoded_each_segment() {
        let transport = FakeTransport::new(vec![
            response(404),
            response(201),
            response(201),
            response(201),
        ]);
        let client = WebdavClient::new(transport.clone(), BASE);
        let store = WebdavStore::new(&client);

        store.put("快照 包.zip", b"x").unwrap();

        let requests = transport.requests();
        assert_eq!(
            header(&requests[3], "Destination").as_deref(),
            Some("https://dav.example.com/dav/PiAgentSync/%E5%BF%AB%E7%85%A7%20%E5%8C%85.zip")
        );
    }

    #[test]
    fn webdav_move_failure_clears_temp_name_and_reports_move_failed() {
        for move_status in [409, 412, 500] {
            let transport = FakeTransport::new(vec![
                response_with(207, ROOT_ONLY_MULTISTATUS),
                response(201),
                response(move_status),
                response(204),
            ]);
            let client = WebdavClient::new(transport.clone(), BASE);
            let store = WebdavStore::new(&client);

            let error = store
                .put("20261122-105230-deadbeef.zip", b"x")
                .expect_err("MOVE 失败必须报错");
            assert_eq!(error.code, "remote_move_failed", "状态 {move_status}");

            let requests = transport.requests();
            assert_eq!(
                methods(&requests),
                vec!["PROPFIND", "PUT", "MOVE", "DELETE"],
                "状态 {move_status}"
            );
            assert_eq!(
                requests[3].url, requests[2].url,
                "清掉的必须是刚才写的临时名"
            );
        }
    }

    #[test]
    fn webdav_put_failure_clears_temp_name() {
        let transport = FakeTransport::new(vec![
            response_with(207, ROOT_ONLY_MULTISTATUS),
            response(507),
            response(204),
        ]);
        let client = WebdavClient::new(transport.clone(), BASE);
        let store = WebdavStore::new(&client);

        let error = store
            .put("20261122-105230-deadbeef.zip", b"x")
            .expect_err("PUT 失败必须报错");

        assert_eq!(error.code, "remote_quota");
        let requests = transport.requests();
        assert_eq!(methods(&requests), vec!["PROPFIND", "PUT", "DELETE"]);
        assert_eq!(requests[2].url, requests[1].url);
    }

    #[test]
    fn webdav_list_splits_packages_from_unrecognized_names() {
        let transport = FakeTransport::new(vec![response_with(207, LISTING_MULTISTATUS)]);
        let client = WebdavClient::new(transport.clone(), BASE);
        let store = WebdavStore::new(&client);

        let listing = store.list().unwrap();

        let names: Vec<&str> = listing
            .packages
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "20261121-091210-7f3e9c01.zip",
                "20261122-105230-deadbeef.zip"
            ]
        );
        assert_eq!(listing.packages[1].size, 4096);
        assert_eq!(
            listing.packages[1].created_at,
            package::parse_name("20261122-105230-deadbeef.zip")
                .unwrap()
                .unix_seconds
        );
        assert_eq!(
            listing.unrecognized,
            vec![
                ".tmp-1795344750123456789-deadbeef",
                "20261122-105230-deadbeef (冲突).zip",
                "manifest.json",
                "snapshot",
            ]
        );
    }

    #[test]
    fn webdav_list_treats_missing_root_as_empty() {
        let transport = FakeTransport::new(vec![response(404)]);
        let client = WebdavClient::new(transport, BASE);
        let store = WebdavStore::new(&client);

        assert_eq!(store.list().unwrap(), StoreListing::default());
    }

    #[test]
    fn webdav_prune_failure_to_list_is_reported_as_prune_failed() {
        let transport = FakeTransport::new(vec![response(401)]);
        let client = WebdavClient::new(transport, BASE);
        let store = WebdavStore::new(&client);

        let error = store
            .prune(20, "20261122-105230-deadbeef.zip")
            .expect_err("列举失败即清理失败");

        assert_eq!(error.code, "remote_prune_failed");
    }

    fn named(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn prune_candidates_keeps_newest_and_only_names_below_current() {
        let older = package::build_name(1_700_000_000, "deadbeef");
        let middle = package::build_name(1_700_003_600, "deadbeef");
        let newest = package::build_name(1_700_007_200, "deadbeef");
        let names = [older.as_str(), middle.as_str(), newest.as_str()];

        assert_eq!(
            prune_candidates(&names, 2, &newest),
            named(&[older.as_str()])
        );
        // current 比 newest 小：名字 ≥ current 的一份都不能删。
        assert_eq!(
            prune_candidates(&names, 0, &middle),
            named(&[older.as_str()])
        );
        // 保留数够了就一个都不删。
        assert!(prune_candidates(&names, 20, &newest).is_empty());
    }

    #[test]
    fn prune_candidates_never_touches_unparseable_names() {
        let older = package::build_name(1_700_000_000, "deadbeef");
        let newest = package::build_name(1_700_003_600, "deadbeef");
        let names = [
            older.as_str(),
            newest.as_str(),
            "manifest.json",
            ".tmp-1795344750123456789-deadbeef",
            "20261122-105230-deadbeef (冲突).zip",
        ];

        assert_eq!(
            prune_candidates(&names, 1, &newest),
            named(&[older.as_str()])
        );
    }

    #[test]
    fn prune_listing_records_failures_without_blocking_others() {
        let older = package::build_name(1_700_000_000, "deadbeef");
        let middle = package::build_name(1_700_003_600, "deadbeef");
        let newest = package::build_name(1_700_007_200, "deadbeef");
        let listing = StoreListing {
            packages: vec![
                StoreEntry {
                    name: newest.clone(),
                    size: 1,
                    created_at: 1_700_007_200,
                },
                StoreEntry {
                    name: middle.clone(),
                    size: 1,
                    created_at: 1_700_003_600,
                },
                StoreEntry {
                    name: older.clone(),
                    size: 1,
                    created_at: 1_700_000_000,
                },
            ],
            unrecognized: named(&["manifest.json"]),
        };

        let mut attempted = Vec::new();
        let outcome = prune_listing(&listing, 1, &newest, |name| {
            attempted.push(name.to_string());
            if name == middle {
                Err("远端拒绝删除".to_string())
            } else {
                Ok(())
            }
        });

        assert_eq!(attempted, vec![middle.clone(), older.clone()]);
        assert_eq!(outcome.pruned, vec![older]);
        assert_eq!(outcome.failed, vec![(middle, "远端拒绝删除".to_string())]);
    }

    #[test]
    fn local_prune_keeps_newest_twenty_and_leaves_foreign_files() {
        let temp = TempDir::new("prune");
        let root = temp.path().join("store");
        let store = LocalStore::new(root.clone());
        for index in 0..25u64 {
            store
                .put(
                    &package::build_name(1_700_000_000 + index * 3_600, "deadbeef"),
                    b"x",
                )
                .unwrap();
        }
        fs::write(root.join("notes.txt"), b"keep me").unwrap();
        assert_eq!(store.list().unwrap().unrecognized, named(&["notes.txt"]));

        let newest = package::build_name(1_795_344_750, "deadbeef");
        let outcome = store.prune(20, &newest).unwrap();

        let listing = store.list().unwrap();
        assert_eq!(listing.packages.len(), 20);
        assert_eq!(outcome.pruned.len(), 5);
        assert!(outcome.failed.is_empty());
        assert!(root.join("notes.txt").exists(), "外来文件永不被清理");
    }
}

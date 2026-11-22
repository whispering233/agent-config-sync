//! WebDAV 上传：把当前 live 导出成一个快照包放到远端，并按保留份数清理。
//!
//! 规则来源：`docs/design/40-webdav.md` 第 5、5.1、7 节。传输只有一套：手动上传与自动上行
//! 都走 [`upload_to_webdav`]，不另写链路。导出与落地语义见 `10-snapshot-sync.md` 第 5 节。

use crate::credentials;
use crate::package;
use crate::remote_state::{self, RemoteState};
use crate::snapshot::{self, SourceVersions, KEEP_PACKAGES};
use crate::store::{PackageStore, PruneOutcome, WebdavStore};
use crate::webdav::{ReqwestTransport, WebdavClient};
use serde::Serialize;
use std::path::Path;
use tauri::Manager;

/// 一次上传的结果，按包语义描述。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadView {
    /// 上传的包名；内容未变时指上次上传的那份（本机记录里查不到时为空）。
    pub package: String,
    pub unchanged: bool,
    /// 本次清理掉的旧包名。
    pub pruned: Vec<String>,
    /// 清理失败的份：不阻塞上传成功，单独上报。
    pub prune_failed: Vec<PruneFailure>,
    /// 导出时的根告警（根是链接、外部根断链等）；上传成功后仍需让用户看见。
    pub root_warnings: Vec<String>,
}

/// 清理失败的一份包。整个清理动作失败（列不出来）时 `name` 为空。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PruneFailure {
    pub name: String,
    pub reason: String,
}

/// 上传失败。码取自 `10-snapshot-sync.md` 第 9 节的失败码表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadFailure {
    pub code: &'static str,
    pub message: String,
}

impl UploadFailure {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// 一次上传的完整结论：给 UI 的视图，加上本机记录里之前没见过的包数。
///
/// 后者只给自动上行用：远端有本机未见的新包时照常上传，但状态要显示 `diverged`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadOutcome {
    pub view: UploadView,
    /// 上传后云端有、本机记录里没有的**他机**包数。
    pub unseen: usize,
}

impl UploadView {
    /// 本地文件夹模式的「上传」没有远端可言：导出结论与保留清理结果。
    pub fn from_export(report: snapshot::ExportReport, prune: PruneOutcome) -> Self {
        Self {
            package: report.package_name,
            unchanged: report.unchanged,
            pruned: prune.pruned,
            prune_failed: prune_failures(prune.failed),
            root_warnings: report.root_warnings,
        }
    }
}

/// 清理失败的份过 IPC 前换成带名字的结构：整个清理动作失败时 `name` 为空。
fn prune_failures(failed: Vec<(String, String)>) -> Vec<PruneFailure> {
    failed
        .into_iter()
        .map(|(name, reason)| PruneFailure { name, reason })
        .collect()
}

/// WebDAV 上传全流程：HTTPS 门槛 → 导出成包 → 内容未变则跳过 → 原子放入 → 重新列举 →
/// 保留清理 → 记录状态。
///
/// 调用方负责拿操作锁：自动上行用 `try_lock_operation`（拿不到就跳过本次），命令层阻塞等待。
pub fn upload_to_webdav(
    store: &dyn PackageStore,
    endpoint: &str,
    app_data: &Path,
    agent_dir: &Path,
    home_dir: Option<&Path>,
    versions: SourceVersions,
    include_auth: bool,
) -> Result<UploadOutcome, UploadFailure> {
    // 门槛放在最前：非 HTTPS 的远端不只可能被读走凭据，还可能被塞进任意快照包，
    // 而落地语义是「快照为准」，所以一个请求都不发。
    if !credentials::is_secure_url(endpoint) {
        return Err(UploadFailure::new(
            "remote_insecure",
            "WebDAV 地址必须是 HTTPS（本机回环地址例外），否则拒绝上传",
        ));
    }

    let state = remote_state::load(app_data);
    let baseline = state.last_uploaded_fingerprint.as_deref();
    let stored = snapshot::pack_and_store(
        store,
        baseline,
        agent_dir,
        home_dir,
        app_data,
        None,
        versions,
        include_auth,
    )
    .map_err(|failure| UploadFailure::new(failure.code, failure.message))?;

    let Some(stored) = stored else {
        // 内容没变：不写包、不碰远端。但远端集合可能已被他机改过（另一台机器推了新包），
        // 不列一次就永远记不到它，`unseen` 恒为 0，托盘会一直报 success。
        let Some(packages) = listed_package_names(store) else {
            // 列不出来就保持原记录：状态缺项比把一次上传判失败轻。
            return Ok(UploadOutcome {
                unseen: 0,
                view: UploadView {
                    package: uploaded_name(&state, baseline.unwrap_or_default()),
                    unchanged: true,
                    pruned: Vec::new(),
                    prune_failed: Vec::new(),
                    root_warnings: Vec::new(),
                },
            });
        };
        let unseen = packages
            .iter()
            .filter(|name| !state.packages.contains(name))
            .count();
        let next = RemoteState {
            seen_at: snapshot::format_timestamp(snapshot::now_seconds()),
            // 集合原样留着：把它刷成「远端现有」就等于把这次发现的差异记成见过了，
            // `unseen` 下次归零、`diverged` 只亮一个周期，用户再也看不到提醒。
            packages: state.packages.clone(),
            // 这次没上传：上次上传的指纹原样留着，下次比对仍以它为准。
            last_uploaded_fingerprint: state.last_uploaded_fingerprint.clone(),
        };
        remote_state::save(app_data, &next).map_err(|error| {
            UploadFailure::new(
                "no_write_permission",
                format!("内容未变，但本机无法记录远端状态：{error}"),
            )
        })?;

        return Ok(UploadOutcome {
            unseen,
            view: UploadView {
                package: uploaded_name(&next, baseline.unwrap_or_default()),
                unchanged: true,
                pruned: Vec::new(),
                prune_failed: Vec::new(),
                root_warnings: Vec::new(),
            },
        });
    };

    let fingerprint = stored.manifest.fingerprint.clone();
    let (pruned, prune_failed) = match store.prune(KEEP_PACKAGES, &stored.name) {
        Ok(outcome) => (outcome.pruned, prune_failures(outcome.failed)),
        // 包已经在远端了：清理失败只单独上报，不改判成上传失败。
        Err(error) => (
            Vec::new(),
            vec![PruneFailure {
                name: String::new(),
                reason: error.message,
            }],
        ),
    };

    // 记录 prune 之后的真实集合：再列一次（prune 内部虽重新列举，但不外传结果），
    // 才能把并发新增的他机包算进去；漏记会让下次判定把别人的包当成「新包」。
    let packages = match store.list() {
        Ok(listing) => listing
            .packages
            .into_iter()
            .map(|entry| entry.name)
            .collect(),
        // 列举失败极少见，按「本机已知 + 本次包」兑底：状态缺项比丢包轻。
        Err(_) => {
            let mut names = state.packages.clone();
            names.push(stored.name.clone());
            names
        }
    };
    let unseen = packages
        .iter()
        .filter(|name| **name != stored.name && !state.packages.contains(name))
        .count();

    let next = RemoteState {
        seen_at: snapshot::format_timestamp(snapshot::now_seconds()),
        packages,
        last_uploaded_fingerprint: Some(fingerprint),
    };
    remote_state::save(app_data, &next).map_err(|error| {
        UploadFailure::new(
            "no_write_permission",
            format!("快照包已上传，但本机无法记录远端状态：{error}"),
        )
    })?;

    Ok(UploadOutcome {
        unseen,
        view: UploadView {
            package: stored.name,
            unchanged: false,
            pruned,
            prune_failed,
            root_warnings: stored.root_warnings,
        },
    })
}

/// 远端当前的可解析包名。列举失败返回 `None`：调用方保持原记录，不改判成上传失败。
fn listed_package_names(store: &dyn PackageStore) -> Option<Vec<String>> {
    let listing = store.list().ok()?;

    Some(
        listing
            .packages
            .into_iter()
            .map(|entry| entry.name)
            .collect(),
    )
}

/// 上次上传的那份包的展示名：本机记录里指纹前 8 位对得上的最新包名。
fn uploaded_name(state: &RemoteState, fingerprint: &str) -> String {
    let head = fingerprint.get(..8).unwrap_or(fingerprint);
    state
        .packages
        .iter()
        .filter(|name| package::parse_name(name).is_some_and(|parsed| parsed.fingerprint8 == head))
        .max()
        .cloned()
        .unwrap_or_default()
}

/// 从已保存的凭据建 WebDAV 存储，再跑一次上传。手动上传与自动上行共用它。
pub fn upload_from_app(
    app: &tauri::AppHandle,
    include_auth: bool,
) -> Result<UploadOutcome, UploadFailure> {
    let app_data = app.path().app_data_dir().map_err(|error| {
        UploadFailure::new(
            "app_data_unavailable",
            format!("无法定位应用数据目录：{error}"),
        )
    })?;
    let credentials = credentials::load(&app_data)
        .map_err(|error| UploadFailure::new("remote_not_configured", error.to_string()))?
        .ok_or_else(|| {
            UploadFailure::new(
                "remote_not_configured",
                "还没有保存 WebDAV 凭据：请先在设置页填写地址、用户名与密码",
            )
        })?;
    let transport = ReqwestTransport::new(&credentials.username, &credentials.password)
        .map_err(|error| UploadFailure::new("remote_unreachable", error.to_string()))?;
    let client = WebdavClient::new(transport, &credentials.url);
    let store = WebdavStore::new(&client);
    let context = snapshot::export_context(app)
        .map_err(|failure| UploadFailure::new(failure.code, failure.message))?;

    upload_to_webdav(
        &store,
        &credentials.url,
        &context.app_data,
        &context.agent_dir,
        context.home_dir.as_deref(),
        context.versions,
        include_auth,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::TEMP_NAME_PREFIX;
    use crate::webdav::{DavError, DavRequest, DavResponse, DavTransport};
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU32, Ordering};

    const BASE: &str = "https://dav.example.com/dav";
    const REMOTE_ROOT: &str = "https://dav.example.com/dav/PiAgentSync";

    /// 临时目录夹具：Drop 时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-upload-{}-{}-{}",
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

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("创建父目录");
        }
        fs::write(path, content).expect("写入夹具文件");
    }

    fn fixture_agent(root: &Path) {
        write_file(
            &root.join("settings.json"),
            "{\"packages\": [\"npm:pi-web-access\"]}",
        );
        write_file(&root.join("AGENTS.md"), "# 指令\n");
        write_file(
            &root.join("auth.json"),
            "{\"openai\": {\"key\": \"sk-secret\"}}",
        );
    }

    struct RemoteStateServer {
        requests: RefCell<Vec<DavRequest>>,
        /// 工作根下的文件：名字 → 内容。够用就行，不模拟子目录。
        files: RefCell<BTreeMap<String, Vec<u8>>>,
        root_exists: Cell<bool>,
        /// `MOVE` 一律按这个状态码失败；0 表示正常。
        move_status: Cell<u16>,
        /// 这些名字的 `DELETE` 返回 403。
        undeletable: RefCell<BTreeSet<String>>,
    }

    /// 极小的假 WebDAV 服务端：只实现包存储用到的五个动词，按真实语义回状态码。
    #[derive(Clone)]
    struct FakeDav {
        state: Rc<RemoteStateServer>,
    }

    impl FakeDav {
        fn new() -> Self {
            Self {
                state: Rc::new(RemoteStateServer {
                    requests: RefCell::new(Vec::new()),
                    files: RefCell::new(BTreeMap::new()),
                    root_exists: Cell::new(false),
                    move_status: Cell::new(0),
                    undeletable: RefCell::new(BTreeSet::new()),
                }),
            }
        }

        fn requests(&self) -> Vec<DavRequest> {
            self.state.requests.borrow().clone()
        }

        fn methods(&self) -> Vec<String> {
            self.requests()
                .into_iter()
                .map(|request| request.method)
                .collect()
        }

        fn seed(&self, name: &str, body: &[u8]) {
            self.state.root_exists.set(true);
            self.state
                .files
                .borrow_mut()
                .insert(name.to_string(), body.to_vec());
        }

        fn file_names(&self) -> Vec<String> {
            self.state.files.borrow().keys().cloned().collect()
        }

        fn package_names(&self) -> Vec<String> {
            self.file_names()
                .into_iter()
                .filter(|name| package::parse_name(name).is_some())
                .collect()
        }
    }

    /// 工作根下的相对名字；工作根自身为空串。
    fn rel_of(url: &str) -> String {
        url.split_once("/PiAgentSync")
            .map(|(_, rest)| rest.trim_matches('/').to_string())
            .unwrap_or_default()
    }

    fn resp(status: u16, body: Vec<u8>) -> Result<DavResponse, DavError> {
        Ok(DavResponse {
            status,
            headers: Vec::new(),
            body,
        })
    }

    impl DavTransport for FakeDav {
        fn send(&self, request: DavRequest) -> Result<DavResponse, DavError> {
            self.state.requests.borrow_mut().push(request.clone());
            let rel = rel_of(&request.url);

            match request.method.as_str() {
                "PROPFIND" => {
                    if !self.state.root_exists.get() {
                        return resp(404, Vec::new());
                    }
                    let mut body = String::from(
                        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
                         <d:multistatus xmlns:d=\"DAV:\">\
                         <d:response><d:href>/dav/PiAgentSync/</d:href>\
                         <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype>\
                         </d:prop></d:propstat></d:response>",
                    );
                    for (name, bytes) in self.state.files.borrow().iter() {
                        body.push_str(&format!(
                            "<d:response><d:href>/dav/PiAgentSync/{name}</d:href>\
                             <d:propstat><d:prop><d:resourcetype/>\
                             <d:getcontentlength>{}</d:getcontentlength>\
                             </d:prop></d:propstat></d:response>",
                            bytes.len()
                        ));
                    }
                    body.push_str("</d:multistatus>");
                    resp(207, body.into_bytes())
                }
                "MKCOL" => {
                    self.state.root_exists.set(true);
                    resp(201, Vec::new())
                }
                "PUT" => {
                    self.state
                        .files
                        .borrow_mut()
                        .insert(rel, request.body.unwrap_or_default());
                    resp(201, Vec::new())
                }
                "MOVE" => {
                    let status = self.state.move_status.get();
                    if status != 0 {
                        return resp(status, Vec::new());
                    }
                    let destination = request
                        .headers
                        .iter()
                        .find(|(name, _)| name == "Destination")
                        .map(|(_, value)| rel_of(value))
                        .unwrap_or_default();
                    let bytes = self.state.files.borrow_mut().remove(&rel);
                    match bytes {
                        // 包名只增不改：目标已存在时回 409，与真机一致。
                        Some(_) if self.state.files.borrow().contains_key(&destination) => {
                            resp(409, Vec::new())
                        }
                        Some(bytes) => {
                            self.state.files.borrow_mut().insert(destination, bytes);
                            resp(201, Vec::new())
                        }
                        None => resp(404, Vec::new()),
                    }
                }
                "DELETE" => {
                    if self.state.undeletable.borrow().contains(&rel) {
                        return resp(403, Vec::new());
                    }
                    self.state.files.borrow_mut().remove(&rel);
                    resp(204, Vec::new())
                }
                other => Err(DavError::Other(format!("测试未实现的方法：{other}"))),
            }
        }
    }

    /// 一套夹具：live 目录、应用数据目录与远端。
    struct Fixture {
        temp: TempDir,
        remote: FakeDav,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let temp = TempDir::new(label);
            let remote = FakeDav::new();
            fixture_agent(&temp.path().join("agent"));
            Self { temp, remote }
        }

        fn agent(&self) -> PathBuf {
            self.temp.path().join("agent")
        }

        fn app_data(&self) -> PathBuf {
            self.temp.path().join("app-data")
        }

        /// 单根家目录：存在但没有 `~/.agents/skills`，导出只声明 agent 根。
        fn home(&self) -> PathBuf {
            let home = self.temp.path().join("home");
            fs::create_dir_all(&home).expect("创建 home");
            home
        }

        fn upload(&self, endpoint: &str) -> Result<UploadOutcome, UploadFailure> {
            let client = WebdavClient::new(self.remote.clone(), BASE);
            let store = WebdavStore::new(&client);
            upload_to_webdav(
                &store,
                endpoint,
                &self.app_data(),
                &self.agent(),
                Some(&self.home()),
                SourceVersions::default(),
                true,
            )
        }

        fn state(&self) -> RemoteState {
            remote_state::load(&self.app_data())
        }
    }

    fn fingerprint8_of(name: &str) -> String {
        package::parse_name(name)
            .expect("包名必须可解析")
            .fingerprint8
    }

    #[test]
    fn insecure_endpoint_sends_no_request() {
        let fixture = Fixture::new("insecure");

        let failure = fixture
            .upload("http://dav.example.com/dav")
            .expect_err("非 HTTPS 必须拒绝");

        assert_eq!(failure.code, "remote_insecure");
        assert!(
            fixture.remote.requests().is_empty(),
            "门槛在最前：一个请求都不该发出"
        );
        assert!(!fixture.app_data().join("remote-state.json").exists());
    }

    #[test]
    fn loopback_http_endpoints_pass_the_gate() {
        for (index, endpoint) in ["http://127.0.0.1:8080/dav", "http://localhost/dav"]
            .into_iter()
            .enumerate()
        {
            let fixture = Fixture::new(&format!("loopback-{index}"));

            let outcome = fixture.upload(endpoint).expect("回环地址应放行");

            assert!(!outcome.view.unchanged, "{endpoint}");
            assert!(!fixture.remote.requests().is_empty(), "{endpoint}");
        }
    }

    #[test]
    fn first_upload_creates_root_puts_moves_and_records_state() {
        let fixture = Fixture::new("first");

        let outcome = fixture.upload(BASE).expect("首次上传应成功");

        assert_eq!(
            fixture.remote.methods(),
            vec!["PROPFIND", "MKCOL", "PUT", "MOVE", "PROPFIND", "PROPFIND"]
        );
        let requests = fixture.remote.requests();
        let temp = rel_of(&requests[2].url);
        assert!(temp.starts_with(TEMP_NAME_PREFIX), "临时名：{temp}");
        let destination = requests[3]
            .headers
            .iter()
            .find(|(name, _)| name == "Destination")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        assert_eq!(
            destination,
            format!("{REMOTE_ROOT}/{}", outcome.view.package)
        );
        assert!(!fixture.remote.methods().contains(&"DELETE".to_string()));

        assert!(!outcome.view.unchanged);
        assert!(outcome.view.pruned.is_empty());
        assert!(outcome.view.prune_failed.is_empty());
        assert_eq!(outcome.unseen, 0);
        assert_eq!(
            fixture.remote.file_names(),
            vec![outcome.view.package.clone()]
        );

        let state = fixture.state();
        assert_eq!(state.packages, vec![outcome.view.package.clone()]);
        let fingerprint = state.last_uploaded_fingerprint.expect("应记录本次指纹");
        assert_eq!(fingerprint.len(), 12);
        assert_eq!(fingerprint8_of(&outcome.view.package), fingerprint[..8]);
        assert_eq!(state.seen_at.len(), 20, "RFC 3339 秒级：{}", state.seen_at);
        assert!(state.seen_at.ends_with('Z'));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(fixture.app_data().join("remote-state.json"))
                .expect("状态文件")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn move_failure_reports_move_failed_and_deletes_the_temp_name() {
        let fixture = Fixture::new("move-failure");
        fixture.remote.state.move_status.set(500);

        let failure = fixture.upload(BASE).expect_err("MOVE 失败必须停住");

        assert_eq!(failure.code, "remote_move_failed");
        assert_eq!(
            fixture.remote.methods(),
            vec!["PROPFIND", "MKCOL", "PUT", "MOVE", "DELETE"]
        );
        let requests = fixture.remote.requests();
        assert_eq!(
            requests[4].url, requests[2].url,
            "清掉的必须是刚才写的临时名"
        );
        assert!(
            !fixture.app_data().join("remote-state.json").exists(),
            "失败不记状态"
        );
        assert!(fixture.remote.package_names().is_empty());
    }

    #[test]
    fn unchanged_content_only_lists_the_remote_and_keeps_the_upload_fingerprint() {
        let fixture = Fixture::new("unchanged");
        fixture.upload(BASE).expect("首次上传应成功");
        let before = fixture.state();

        // 换一个空远端：这次只要发出写请求就会留下痕迹。
        let second_remote = FakeDav::new();
        let client = WebdavClient::new(second_remote.clone(), BASE);
        let store = WebdavStore::new(&client);
        let outcome = upload_to_webdav(
            &store,
            BASE,
            &fixture.app_data(),
            &fixture.agent(),
            Some(&fixture.home()),
            SourceVersions::default(),
            true,
        )
        .expect("内容未变不是错误");

        assert!(outcome.view.unchanged);
        assert_eq!(
            second_remote.methods(),
            vec!["PROPFIND"],
            "内容未变只列一次远端"
        );
        assert_eq!(outcome.unseen, 0, "远端已空：没有本机未见的新包");
        let after = fixture.state();
        assert_eq!(
            after.packages, before.packages,
            "只推进时间：集合只由用户主动动作推进"
        );
        assert_eq!(
            after.last_uploaded_fingerprint, before.last_uploaded_fingerprint,
            "只跳过上传：上次上传的指纹原样留着"
        );
        assert_eq!(after.seen_at.len(), 20, "RFC 3339 秒级：{}", after.seen_at);
    }

    #[test]
    fn unchanged_content_still_counts_packages_another_machine_pushed() {
        let fixture = Fixture::new("unchanged-diverged");
        let first = fixture.upload(BASE).expect("首次上传应成功");
        let fingerprint = fixture.state().last_uploaded_fingerprint;
        assert!(fingerprint.is_some(), "首次上传应记下指纹");

        // 他机推了一份新包，而本机记录还停在上一次上传时的样子。
        let pushed = old_package(30);
        fixture.remote.seed(&pushed, b"other-machine");
        remote_state::save(
            &fixture.app_data(),
            &RemoteState {
                seen_at: "2026-11-20T00:00:00Z".to_string(),
                packages: vec![first.view.package.clone()],
                last_uploaded_fingerprint: fingerprint.clone(),
            },
        )
        .expect("写初始状态");

        let outcome = fixture.upload(BASE).expect("内容未变不是错误");

        assert!(outcome.view.unchanged);
        assert_eq!(outcome.unseen, 1, "另一台机器的包要算进来");
        let state = fixture.state();
        assert_eq!(
            state.packages,
            vec![first.view.package.clone()],
            "集合不动：后台把它记成见过就再也报不出 diverged"
        );
        assert_eq!(
            state.last_uploaded_fingerprint, fingerprint,
            "只跳过上传：上次上传的指纹原样留着"
        );
        assert_ne!(state.seen_at, "2026-11-20T00:00:00Z");
        assert_eq!(state.seen_at.len(), 20, "RFC 3339 秒级：{}", state.seen_at);
    }

    fn old_package(index: u64) -> String {
        package::build_name(1_700_000_000 + index * 3_600, "deadbeef")
    }

    /// 24 份旧包 + 本次 1 份 = 25：保留 20，删掉最旧的 5 份。
    fn seed_old_packages(fixture: &Fixture) {
        for index in 0..24u64 {
            fixture.remote.seed(&old_package(index), b"old-package");
        }
        fixture.remote.seed("notes.txt", b"keep me");
    }

    #[test]
    fn prune_keeps_twenty_and_leaves_unrecognized_files_alone() {
        let fixture = Fixture::new("prune");
        seed_old_packages(&fixture);

        let outcome = fixture.upload(BASE).expect("上传应成功");

        let oldest: Vec<String> = (0..5u64).map(old_package).collect();
        let mut pruned = outcome.view.pruned.clone();
        pruned.sort();
        assert_eq!(pruned, oldest, "删的是最旧的 5 份");
        assert!(outcome.view.prune_failed.is_empty());

        assert_eq!(fixture.remote.package_names().len(), 20);
        assert!(
            fixture
                .remote
                .file_names()
                .contains(&"notes.txt".to_string()),
            "无法识别的文件永不被清理"
        );
        // 记录的是 prune 之后的集合：被删掉的 5 份不在里面。
        let state = fixture.state();
        assert_eq!(state.packages.len(), 20);
        for name in &oldest {
            assert!(!state.packages.contains(name), "{name}");
        }
        assert!(state.packages.contains(&outcome.view.package));
    }

    #[test]
    fn prune_failure_reports_the_item_without_failing_the_upload() {
        let fixture = Fixture::new("prune-failure");
        seed_old_packages(&fixture);
        let stuck = old_package(0);
        fixture
            .remote
            .state
            .undeletable
            .borrow_mut()
            .insert(stuck.clone());

        let outcome = fixture.upload(BASE).expect("清理失败不阻塞上传");

        assert_eq!(outcome.view.pruned.len(), 4);
        assert_eq!(outcome.view.prune_failed.len(), 1);
        assert_eq!(outcome.view.prune_failed[0].name, stuck);
        assert!(!outcome.view.prune_failed[0].reason.is_empty());
        assert!(fixture.remote.file_names().contains(&stuck));
        // 删不掉的那份仍在远端，因此也要留在记录里。
        assert!(fixture.state().packages.contains(&stuck));
    }

    #[test]
    fn unseen_counts_remote_packages_the_state_never_saw() {
        let fixture = Fixture::new("diverged");
        seed_old_packages(&fixture);
        // 本机记录只认得 23 份：最新的那份旧包是他机刚推的，保留清理不会碰它。
        let known: Vec<String> = (0..23u64).map(old_package).collect();
        remote_state::save(
            &fixture.app_data(),
            &RemoteState {
                seen_at: "2026-11-20T00:00:00Z".to_string(),
                packages: known,
                last_uploaded_fingerprint: None,
            },
        )
        .expect("写初始状态");

        let outcome = fixture.upload(BASE).expect("云端有新包也照常上传");

        assert!(!outcome.view.unchanged);
        assert_eq!(outcome.unseen, 1);
        assert!(fixture.remote.file_names().contains(&old_package(23)));
    }
}

//! WebDAV 客户端（无 UI、无 command）。
//!
//! 规则来源：`docs/design/40-webdav.md` 第 4 节。这里只做协议：建目录、列目录、读写文件、
//! 删除；不决定快照内容，也不做冲突判定。
//!
//! 请求与响应先落成 [`DavRequest`] / [`DavResponse`]，真实实现 [`ReqwestTransport`] 负责
//! 交给 HTTP 库。协议形状（方法、路径编码、`Depth`、条件头、body）因此能用假 transport
//! 精确断言，不必起服务端，也就不引入 HTTP mock 依赖。
//!
//! 凭据只用于构造一次 Basic 头：不进错误信息，也不进 [`fmt::Debug`]。

use std::fmt;
use std::time::Duration;

/// 工作根的固定目录名。与设计文档的远端布局一致，不开放配置。
pub const REMOTE_ROOT: &str = "PiAgentSync";
/// 控制请求（`PROPFIND` / `MKCOL` / `DELETE`）的超时。
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// 单文件传输（`PUT` / `GET`）的超时。
pub const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);

/// 传输失败。404 不在这里：不存在由各方法返回 `None` 表达。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DavError {
    /// 401/403：凭据无效或权限不足。
    AuthFailed,
    /// 507，或 403 且服务商在响应体里提示配额。
    Quota,
    /// 网络、超时、其它 5xx。带上底层原因，便于用户自查 DNS/TLS/代理。
    Unreachable(String),
    /// 412：条件请求（`If-Match` / `If-None-Match`）不成立。
    ///
    /// 单独一类是因为它不代表网络失败，而是远端在本次操作期间被改写过；
    /// 上传据此报 `remote_changed` 并停下，不重试。
    PreconditionFailed,
    /// 其余状态与协议层错误。
    Other(String),
}

impl fmt::Display for DavError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthFailed => write!(formatter, "认证失败（凭据无效或权限不足）"),
            Self::Quota => write!(formatter, "远端空间或流量不足"),
            Self::PreconditionFailed => {
                write!(formatter, "远端在本次操作期间被改写（条件请求未通过）")
            }
            Self::Unreachable(detail) | Self::Other(detail) => write!(formatter, "{detail}"),
        }
    }
}

impl std::error::Error for DavError {}

/// 一次待发送的请求。`url` 是拼好的绝对地址，transport 不再加工。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// `None` 表示没有 body（`GET` / `DELETE` / `MKCOL`）。
    pub body: Option<Vec<u8>>,
    pub timeout: Duration,
}

/// 一次收到的响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// HTTP 发送能力。生产用 [`ReqwestTransport`]，单测用假实现。
pub trait DavTransport {
    fn send(&self, request: DavRequest) -> Result<DavResponse, DavError>;
}

/// 用 `reqwest` 的阻塞客户端发请求。
pub struct ReqwestTransport {
    client: reqwest::blocking::Client,
    /// 构造时算好的 `Authorization` 值：凭据不留在结构体里，也不会被逐请求重算。
    auth: String,
}

impl fmt::Debug for ReqwestTransport {
    /// 手写 Debug：`auth` 是凭据的等价物，绝不打印。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReqwestTransport")
            .finish_non_exhaustive()
    }
}

impl ReqwestTransport {
    pub fn new(username: &str, password: &str) -> Result<Self, DavError> {
        // 默认 feature 已含 rustls + 平台根证书 + 系统代理，无需再改 TLS 配置。
        let client = reqwest::blocking::Client::builder()
            .build()
            .map_err(network_error)?;

        Ok(Self {
            client,
            auth: basic_auth_header(username, password),
        })
    }
}

impl DavTransport for ReqwestTransport {
    fn send(&self, request: DavRequest) -> Result<DavResponse, DavError> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|_| DavError::Other(format!("无法识别的 HTTP 方法：{}", request.method)))?;

        let mut builder = self
            .client
            .request(method, &request.url)
            // 认证头统一在这里加：调用方拿不到凭据。
            .header(reqwest::header::AUTHORIZATION, self.auth.as_str());

        for (name, value) in &request.headers {
            let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| DavError::Other(format!("无法识别的请求头名：{name}")))?;
            // 值不进错误信息：头的种类由调用方决定，无法预判是否敏感。
            let header_value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| DavError::Other(format!("请求头 {name} 的值不合法")))?;
            builder = builder.header(header_name, header_value);
        }

        if let Some(body) = request.body {
            builder = builder.body(body);
        }

        let response = builder
            .timeout(request.timeout)
            .send()
            .map_err(network_error)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body = response.bytes().map_err(network_error)?.to_vec();

        Ok(DavResponse {
            status,
            headers,
            body,
        })
    }
}

/// 超时、DNS、TLS、连接被拒对上层都是「不可达」；带上底层文本便于用户自查。
fn network_error(error: reqwest::Error) -> DavError {
    let kind = if error.is_timeout() {
        "请求超时"
    } else if error.is_connect() {
        "连接失败"
    } else {
        "网络错误"
    };

    DavError::Unreachable(format!("{kind}：{error}"))
}

/// `Authorization` 头。Basic 认证是 base64 的唯一用途，不为此引 crate。
fn basic_auth_header(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64_encode(format!("{username}:{password}").as_bytes())
    )
}

fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0] as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (first << 16) | (second << 8) | third;

        output.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        output.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }

    output
}

/// `MOVE` 的结局。目标已存在单独一类：它不是网络失败，调用方要能分辨。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameOutcome {
    Renamed,
    TargetExists,
}

/// 一条目录项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavEntry {
    /// 相对工作根的路径：已解码、无前导斜杠、无尾斜杠；空串代表工作根自身。
    ///
    /// 一律相对工作根，与 manifest 里的路径同一套坐标系。
    pub path: String,
    pub is_collection: bool,
    pub content_length: Option<u64>,
}

/// 五动词客户端。`T` 只是发送能力，路径拼接与响应解释都在这里。
pub struct WebdavClient<T: DavTransport> {
    transport: T,
    /// `<地址>/PiAgentSync`，不带尾斜杠。
    root: String,
    control_timeout: Duration,
    transfer_timeout: Duration,
}

impl<T: DavTransport> WebdavClient<T> {
    /// 地址语义是「服务地址」，不是「云盘里的某个文件夹」，所以工作根固定拼在后面。
    pub fn new(transport: T, base_url: &str) -> Self {
        let root = format!("{}/{}", base_url.trim_end_matches('/'), REMOTE_ROOT);

        Self {
            root,
            transport,
            control_timeout: CONTROL_TIMEOUT,
            transfer_timeout: TRANSFER_TIMEOUT,
        }
    }

    /// 覆盖超时：生产用常量，单测要毫秒级。
    // 只在测试里存在：生产路径直接用常量，不保留配置入口。
    #[cfg(test)]
    pub fn with_timeouts(mut self, control: Duration, transfer: Duration) -> Self {
        self.control_timeout = control;
        self.transfer_timeout = transfer;
        self
    }

    /// 工作根的绝对地址，用于日志与探测文案。
    pub fn root(&self) -> &str {
        &self.root
    }

    /// 列目录。条目只含直接子项；被列的那个集合自身那一项会被过滤掉。
    ///
    /// 有的服务端对不存在的集合回 207 + 空 body，所以原始条目为 0 才判「不存在」；
    /// 存在的空目录会返回 `Some(vec![])`。
    pub fn propfind(&self, rel: &str) -> Result<Option<Vec<DavEntry>>, DavError> {
        // 剥掉的前缀固定是工作根：条目路径与 manifest 用同一套坐标系，调用方不必再做换算。
        let base = url_path(&self.root).to_string();
        let self_path = percent_decode(rel.trim_matches('/'));
        let response = self.transport.send(DavRequest {
            method: "PROPFIND".to_string(),
            url: self.url(rel),
            headers: vec![
                ("Depth".to_string(), "1".to_string()),
                (
                    "Content-Type".to_string(),
                    "application/xml; charset=utf-8".to_string(),
                ),
            ],
            body: Some(PROPFIND_BODY.as_bytes().to_vec()),
            timeout: self.control_timeout,
        })?;

        match response.status {
            200 | 207 => {
                let parsed = parse_propfind(&String::from_utf8_lossy(&response.body), &base);
                let present = !parsed.is_empty();
                let entries = parsed
                    .into_iter()
                    .filter(|entry| !entry.path.is_empty() && entry.path != self_path)
                    .collect::<Vec<_>>();
                if present {
                    Ok(Some(entries))
                } else {
                    Ok(None)
                }
            }
            404 => Ok(None),
            status => Err(error_for(status, &response.body)),
        }
    }

    /// 建目录。返回 `true` 表示这次建出来了，`false` 表示已存在。
    ///
    /// 不带尾斜杠：有的服务端对带尾斜杠的 `MKCOL` 回 405 却没创建。
    pub fn mkcol(&self, rel: &str) -> Result<bool, DavError> {
        let response = self.transport.send(DavRequest {
            method: "MKCOL".to_string(),
            url: self.url(rel),
            headers: Vec::new(),
            body: None,
            timeout: self.control_timeout,
        })?;

        match response.status {
            200 | 201 | 204 => Ok(true),
            // 405/301/302 = 目录已存在（服务端差异见设计文档第 4 节）。
            301 | 302 | 405 => Ok(false),
            status => Err(error_for(status, &response.body)),
        }
    }

    /// 上传文件。条件头（`If-Match` / `If-None-Match`）由调用方原样传入。
    pub fn put(
        &self,
        rel: &str,
        body: &[u8],
        headers: &[(String, String)],
    ) -> Result<(), DavError> {
        let response = self.transport.send(DavRequest {
            method: "PUT".to_string(),
            url: self.url(rel),
            headers: headers.to_vec(),
            body: Some(body.to_vec()),
            timeout: self.transfer_timeout,
        })?;

        if (200..300).contains(&response.status) {
            Ok(())
        } else {
            Err(error_for(response.status, &response.body))
        }
    }

    /// `MOVE`：一次请求完成的原子改名，`PUT` 定案靠它。
    ///
    /// `Destination` 必须是绝对 URL，路径编码与其它动词一致。定案用 `Overwrite: F`：
    /// 包名只增不改，目标已存在（409/412）说明另有机器刚推了同名包，因此单独返回
    /// [`RenameOutcome::TargetExists`]，不混进网络失败。
    pub fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<RenameOutcome, DavError> {
        let response = self.transport.send(DavRequest {
            method: "MOVE".to_string(),
            url: self.url(from),
            headers: vec![
                ("Destination".to_string(), self.url(to)),
                (
                    "Overwrite".to_string(),
                    if overwrite { "T" } else { "F" }.to_string(),
                ),
            ],
            body: None,
            timeout: self.control_timeout,
        })?;

        match response.status {
            201 | 204 => Ok(RenameOutcome::Renamed),
            409 | 412 => Ok(RenameOutcome::TargetExists),
            status => Err(error_for(status, &response.body)),
        }
    }

    /// 下载文件：404 → `Ok(None)`；成功时连响应头的 ETag 一起返回。
    pub fn get(&self, rel: &str) -> Result<Option<(Vec<u8>, Option<String>)>, DavError> {
        let response = self.transport.send(DavRequest {
            method: "GET".to_string(),
            url: self.url(rel),
            headers: Vec::new(),
            body: None,
            timeout: self.transfer_timeout,
        })?;

        match response.status {
            200..=299 => Ok(Some((
                response.body,
                header_value(&response.headers, "etag"),
            ))),
            404 => Ok(None),
            status => Err(error_for(status, &response.body)),
        }
    }

    /// 删除文件。404 视为已删除：清探针时重复删除不该报错。
    pub fn delete(&self, rel: &str) -> Result<(), DavError> {
        let response = self.transport.send(DavRequest {
            method: "DELETE".to_string(),
            url: self.url(rel),
            headers: Vec::new(),
            body: None,
            timeout: self.control_timeout,
        })?;

        match response.status {
            200..=299 | 404 => Ok(()),
            status => Err(error_for(status, &response.body)),
        }
    }

    /// 相对路径 → 绝对地址；相对路径以已解码形式传入，逐段编码。
    fn url(&self, rel: &str) -> String {
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            self.root.clone()
        } else {
            format!("{}/{}", self.root, encode_path(rel))
        }
    }
}

/// 写权限测试用的临时文件名。落在工作根下，探测结束即删。
const WRITE_TEST_FILE: &str = ".tmp-writetest";

/// 一次连接探测的结论：地址、凭据、读权限、写权限是否都可用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// 本次探测新建了工作根目录。
    pub created: bool,
    /// 写权限测试文件没能删掉：只读账号常见，但读写本身已经验证，所以不算失败。
    pub leftover_write_test_file: bool,
}

/// 探测地址、凭据与写权限：工作根存在（必要时创建）→ 写测试文件 → 删测试文件。
///
/// 只读不写的账号在删除这一步失败，仍返回成功并标记残留：
/// 用户需要知道「能连、但删不掉东西」，而不是被一句认证失败挡住。
pub fn probe<T: DavTransport>(client: &WebdavClient<T>) -> Result<ProbeOutcome, DavError> {
    let created = match client.propfind("")? {
        Some(_) => false,
        None => {
            let created = client.mkcol("")?;
            // 有的服务端对 MKCOL 回 2xx 却没建出来：不在这里拦住，后面的 PUT 会以 404 失败，
            // 用户看到的就是一个对不上原因的错误。
            if client.propfind("")?.is_none() {
                return Err(DavError::Other(format!(
                    "工作目录不存在且创建失败：{}",
                    client.root()
                )));
            }
            created
        }
    };

    client.put(WRITE_TEST_FILE, b"ok", &[])?;

    Ok(ProbeOutcome {
        created,
        leftover_write_test_file: client.delete(WRITE_TEST_FILE).is_err(),
    })
}

/// `PROPFIND` 只说清楚要哪几项，免得服务端回一大堆用不上的属性。
const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/></d:prop></d:propfind>"#;

/// 状态码 → 错误。404 与具体方法的成功分支不在这里处理。
fn error_for(status: u16, body: &[u8]) -> DavError {
    if status == 507 || (status == 403 && looks_like_quota(body)) {
        return DavError::Quota;
    }

    match status {
        401 | 403 => DavError::AuthFailed,
        412 => DavError::PreconditionFailed,
        500..=599 => DavError::Unreachable(format!("服务端返回 HTTP {status}")),
        _ => DavError::Other(format!("服务端返回 HTTP {status}")),
    }
}

/// 配额提示是服务商自定义文案，只能按关键词猜。
fn looks_like_quota(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body).to_lowercase();
    ["quota", "insufficient", "配额", "空间", "流量", "容量"]
        .iter()
        .any(|needle| text.contains(needle))
}

/// 响应头按名字取值，大小写不敏感。
fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

/// 解析 207 Multi-Status。
///
/// `base_path` 是工作根在 URL 里的 path 段（如 `/dav/PiAgentSync`），用来把 href 还原成
/// 相对路径。只认 href、resourcetype、getcontentlength；命名空间前缀可有可无。
pub fn parse_propfind(xml: &str, base_path: &str) -> Vec<DavEntry> {
    let base = percent_decode(base_path.trim_end_matches('/'));

    let mut entries: Vec<DavEntry> = element_blocks(xml, "response")
        .into_iter()
        .filter_map(|block| parse_response(block, &base))
        .collect();

    entries.sort_by(|left, right| left.path.cmp(&right.path));
    entries
}

fn parse_response(block: &str, base: &str) -> Option<DavEntry> {
    let href = element_text(block, "href")?;

    let is_collection = element_blocks(block, "resourcetype")
        .first()
        .map(|inner| has_element(inner, "collection"))
        .unwrap_or(false);
    let content_length =
        element_text(block, "getcontentlength").and_then(|text| text.trim().parse::<u64>().ok());

    Some(DavEntry {
        path: relative_path(&href, base),
        is_collection,
        content_length,
    })
}

/// href → 相对工作根的路径：丢掉查询串与片段，绝对 URL 只取 path，再解码、剥前缀。
fn relative_path(href: &str, base: &str) -> String {
    let href = href.trim();
    let href = href.split(['?', '#']).next().unwrap_or_default();
    let decoded = percent_decode(href_path(href));
    let decoded = decoded.trim_matches('/');
    let base = base.trim_matches('/');

    let relative = if decoded.is_empty() {
        ""
    } else if base.is_empty() {
        decoded
    } else if decoded == base {
        ""
    } else {
        decoded
            .strip_prefix(base)
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(decoded)
    };

    relative.trim_end_matches('/').to_string()
}

/// 绝对 URL 取 path 段，其余原样返回（href 也可能是绝对路径或相对路径）。
fn href_path(href: &str) -> &str {
    if href.contains("://") {
        url_path(href)
    } else {
        href
    }
}

/// 取绝对 URL 的 path 段；没有 path 时返回 `/`。
fn url_path(url: &str) -> &str {
    match url.split_once("://") {
        Some((_, rest)) => rest.find('/').map(|index| &rest[index..]).unwrap_or("/"),
        // 地址必须带 scheme（凭据校验时已限制 http/https），这里只当兜底。
        None => url,
    }
}

/// 逐段百分号编码。
///
/// 约定调用方传已解码的路径；但已经带 `%XX` 的段按「已编码」原样保留，避免 `%20` 变
/// `%2520`。代价是字面量里真含 `%XX` 的段不会被编码，路径里出现这种段属于用户自找。
pub fn encode_path(rel: &str) -> String {
    rel.split('/')
        .filter(|segment| !segment.is_empty())
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_segment(segment: &str) -> String {
    if has_percent_escape(segment) {
        return segment.to_string();
    }

    let mut output = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                output.push(byte as char)
            }
            _ => output.push_str(&format!("%{byte:02X}")),
        }
    }

    output
}

fn has_percent_escape(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    bytes.windows(3).any(|window| {
        window[0] == b'%' && window[1].is_ascii_hexdigit() && window[2].is_ascii_hexdigit()
    })
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = hex_value(bytes[index + 1]);
            let low = hex_value(bytes[index + 2]);
            if let (Some(high), Some(low)) = (high, low) {
                output.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }

    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 一个 XML 标签：本地名（去掉命名空间前缀）、是否闭合、是否自闭合、在原文里的位置。
struct Tag {
    local: String,
    closing: bool,
    self_closing: bool,
    start: usize,
    end: usize,
}

/// 扫出所有普通标签，跳过 `<?xml?>`、注释与 CDATA。
///
/// 够用的窄解析：DAV 响应是机器生成的，属性值里不会出现 `>`，不值得为它引 XML 依赖。
fn tags(text: &str) -> Vec<Tag> {
    let mut found = Vec::new();
    let mut cursor = 0;

    while let Some(offset) = text[cursor..].find('<') {
        let start = cursor + offset;
        let rest = &text[start..];

        if rest.starts_with("<?") || rest.starts_with("<!--") || rest.starts_with("<![CDATA[") {
            let terminator = if rest.starts_with("<?") {
                "?>"
            } else if rest.starts_with("<!--") {
                "-->"
            } else {
                "]]>"
            };
            match rest.find(terminator) {
                Some(index) => {
                    cursor = start + index + terminator.len();
                    continue;
                }
                // 没有结束标记：后面的内容都不可信。
                None => break,
            }
        }

        let Some(gt) = rest.find('>') else {
            break;
        };
        let end = start + gt + 1;
        let inner = &rest[1..gt];

        if !inner.is_empty() {
            let closing = inner.starts_with('/');
            let body = inner.trim_start_matches('/').trim_end();
            let self_closing = body.ends_with('/');
            let name = body
                .trim_end_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or_default();

            if !name.is_empty() {
                found.push(Tag {
                    local: local_name(name),
                    closing,
                    self_closing,
                    start,
                    end,
                });
            }
        }

        cursor = end;
    }

    found
}

/// `D:href` / `ns0:href` / `href` 都视作 `href`。
fn local_name(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_ascii_lowercase()
}

/// 取所有 `<name>…</name>` 的内文（同名不嵌套）。
fn element_blocks<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let tags = tags(text);
    let mut blocks = Vec::new();
    let mut index = 0;

    while index < tags.len() {
        let tag = &tags[index];
        if tag.closing || tag.local != name {
            index += 1;
            continue;
        }
        if tag.self_closing {
            blocks.push("");
            index += 1;
            continue;
        }

        // 找同名闭合标签；响应被截断时放弃这一段，不 panic。
        match tags[index + 1..]
            .iter()
            .position(|next| next.closing && next.local == name)
        {
            Some(offset) => {
                let close = &tags[index + 1 + offset];
                blocks.push(&text[tag.end..close.start]);
                index += 1 + offset + 1;
            }
            None => break,
        }
    }

    blocks
}

fn element_text(text: &str, name: &str) -> Option<String> {
    element_blocks(text, name)
        .first()
        .map(|inner| unescape(inner.trim()))
        .filter(|value| !value.is_empty())
}

fn has_element(text: &str, name: &str) -> bool {
    tags(text)
        .iter()
        .any(|tag| !tag.closing && tag.local == name)
}

/// XML 实体反转义。单趟扫描，避免 `&amp;lt;` 被解两次。
fn unescape(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(index) = rest.find('&') {
        output.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        match entity(after) {
            Some((replacement, consumed)) => {
                output.push(replacement);
                rest = &after[consumed..];
            }
            None => {
                output.push('&');
                rest = after;
            }
        }
    }
    output.push_str(rest);

    output
}

fn entity(text: &str) -> Option<(char, usize)> {
    for (name, character) in [
        ("amp;", '&'),
        ("lt;", '<'),
        ("gt;", '>'),
        ("quot;", '"'),
        ("#39;", '\''),
        ("apos;", '\''),
    ] {
        if text.starts_with(name) {
            return Some((character, name.len()));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    /// 工作根路径，单测里反复出现。
    const BASE: &str = "https://dav.example.com/dav";

    struct State {
        requests: RefCell<Vec<DavRequest>>,
        responses: RefCell<VecDeque<Result<DavResponse, DavError>>>,
    }

    /// 假 transport：记录收到的请求，按顺序吐出预置响应。
    #[derive(Clone)]
    struct FakeTransport {
        state: Rc<State>,
    }

    impl FakeTransport {
        fn new(responses: Vec<Result<DavResponse, DavError>>) -> Self {
            Self {
                state: Rc::new(State {
                    requests: RefCell::new(Vec::new()),
                    responses: RefCell::new(responses.into()),
                }),
            }
        }

        fn last(&self) -> DavRequest {
            self.state
                .requests
                .borrow()
                .last()
                .cloned()
                .expect("没有捕获到请求")
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

    fn response(status: u16) -> DavResponse {
        DavResponse {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn response_with(status: u16, headers: Vec<(String, String)>, body: &[u8]) -> DavResponse {
        DavResponse {
            status,
            headers,
            body: body.to_vec(),
        }
    }

    fn client(
        responses: Vec<Result<DavResponse, DavError>>,
    ) -> (WebdavClient<FakeTransport>, FakeTransport) {
        let transport = FakeTransport::new(responses);
        let client = WebdavClient::new(transport.clone(), BASE);
        (client, transport)
    }

    fn header(request: &DavRequest, name: &str) -> Option<String> {
        request
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    const EMPTY_MULTISTATUS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:"></d:multistatus>"#;

    /// 存在但没有子项的工作根：服务端仍会把集合自身列出来。
    const ROOT_ONLY_MULTISTATUS: &str = r#"<d:multistatus xmlns:d="DAV:"><d:response>
    <d:href>/dav/PiAgentSync/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
</d:response></d:multistatus>"#;

    const MIXED_MULTISTATUS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>https://dav.example.com/dav/PiAgentSync/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/PiAgentSync/snapshot/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/PiAgentSync/snapshot/manifest.json</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype/>
        <d:getcontentlength>2048</d:getcontentlength>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <D:response>
    <D:href>/dav/PiAgentSync/snapshot/%E9%85%8D%E7%BD%AE%20%E6%96%87%E4%BB%B6.json</D:href>
    <D:propstat>
      <D:prop><ns0:resourcetype xmlns:ns0="DAV:"/><D:getcontentlength>10</D:getcontentlength></D:prop>
    </D:propstat>
  </D:response>
  <response>
    <href>/dav/PiAgentSync/with&amp;entity&#39;s.txt?token=1#frag</href>
    <propstat><prop><resourcetype/></prop></propstat>
  </response>
</d:multistatus>"#;

    #[test]
    fn basic_auth_header_is_standard_base64() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(basic_auth_header("user", "pass"), "Basic dXNlcjpwYXNz");
    }

    #[test]
    fn transport_debug_never_leaks_credentials() {
        let transport = ReqwestTransport::new("user", "s3cret-value").expect("构造 transport");

        let text = format!("{transport:?}");

        assert!(!text.contains("s3cret-value"));
        assert!(!text.contains("Basic"));
        assert!(text.contains("ReqwestTransport"));
    }

    #[test]
    fn encode_path_encodes_utf8_and_keeps_existing_escapes() {
        assert_eq!(
            encode_path("snapshot/配置 文件.json"),
            "snapshot/%E9%85%8D%E7%BD%AE%20%E6%96%87%E4%BB%B6.json"
        );
        // 已编码的段不重复编码。
        assert_eq!(
            encode_path("snapshot/%E9%85%8D%E7%BD%AE.json"),
            "snapshot/%E9%85%8D%E7%BD%AE.json"
        );
        assert_eq!(encode_path("/a//b/"), "a/b");
        assert_eq!(encode_path("manifest.json"), "manifest.json");
    }

    #[test]
    fn propfind_empty_multistatus_means_absent() {
        let (client, transport) = client(vec![Ok(response_with(
            207,
            Vec::new(),
            EMPTY_MULTISTATUS.as_bytes(),
        ))]);

        assert_eq!(client.propfind("").unwrap(), None);

        let request = transport.last();
        assert_eq!(request.method, "PROPFIND");
        assert_eq!(request.url, "https://dav.example.com/dav/PiAgentSync");
        assert_eq!(header(&request, "Depth").as_deref(), Some("1"));
        assert_eq!(request.timeout, CONTROL_TIMEOUT);
        assert!(String::from_utf8_lossy(request.body.as_deref().unwrap()).contains("propfind"));
    }

    #[test]
    fn parse_propfind_handles_any_prefix_and_decodes_hrefs() {
        let entries = parse_propfind(MIXED_MULTISTATUS, "/dav/PiAgentSync");

        assert_eq!(
            entries,
            vec![
                DavEntry {
                    path: String::new(),
                    is_collection: true,
                    content_length: None,
                },
                DavEntry {
                    path: "snapshot".to_string(),
                    is_collection: true,
                    content_length: None,
                },
                DavEntry {
                    path: "snapshot/manifest.json".to_string(),
                    is_collection: false,
                    content_length: Some(2048),
                },
                DavEntry {
                    path: "snapshot/配置 文件.json".to_string(),
                    is_collection: false,
                    content_length: Some(10),
                },
                DavEntry {
                    path: "with&entity's.txt".to_string(),
                    is_collection: false,
                    content_length: None,
                },
            ]
        );
    }

    #[test]
    fn parse_propfind_empty_multistatus_and_missing_props() {
        assert!(parse_propfind(EMPTY_MULTISTATUS, "/dav/PiAgentSync").is_empty());

        let no_length = r#"<d:multistatus xmlns:d="DAV:"><d:response>
            <d:href>/dav/PiAgentSync/a.txt</d:href>
            <d:propstat><d:prop><d:resourcetype/></d:prop></d:propstat>
        </d:response></d:multistatus>"#;

        let entries = parse_propfind(no_length, "/dav/PiAgentSync");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "a.txt");
        assert!(!entries[0].is_collection);
        assert_eq!(entries[0].content_length, None);
    }

    #[test]
    fn propfind_filters_self_entry() {
        let (client, _) = client(vec![Ok(response_with(
            207,
            Vec::new(),
            MIXED_MULTISTATUS.as_bytes(),
        ))]);

        let entries = client.propfind("").unwrap().unwrap();
        let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();

        assert_eq!(
            paths,
            vec![
                "snapshot",
                "snapshot/manifest.json",
                "snapshot/配置 文件.json",
                "with&entity's.txt"
            ]
        );
        assert!(!paths.contains(&""));
    }

    #[test]
    fn propfind_paths_stay_relative_to_work_root() {
        let (client, _) = client(vec![Ok(response_with(
            207,
            Vec::new(),
            MIXED_MULTISTATUS.as_bytes(),
        ))]);

        let entries = client.propfind("snapshot").unwrap().unwrap();
        let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();

        // 被列集合自身（`snapshot`）与工作根自身（空串）都不算子项。
        assert_eq!(
            paths,
            vec![
                "snapshot/manifest.json",
                "snapshot/配置 文件.json",
                "with&entity's.txt"
            ]
        );
    }

    #[test]
    fn propfind_404_and_empty_collection() {
        let (missing, _) = client(vec![Ok(response(404))]);
        assert_eq!(missing.propfind("nope").unwrap(), None);

        let self_only = r#"<d:multistatus xmlns:d="DAV:"><d:response>
            <d:href>/dav/PiAgentSync/empty/</d:href>
            <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
        </d:response></d:multistatus>"#;
        let (client, _) = client(vec![Ok(response_with(
            207,
            Vec::new(),
            self_only.as_bytes(),
        ))]);

        assert_eq!(client.propfind("empty").unwrap(), Some(Vec::new()));
    }

    #[test]
    fn mkcol_reports_created_or_existing() {
        let (created, created_transport) = client(vec![Ok(response(201))]);
        assert!(created.mkcol("snapshot").unwrap());
        assert_eq!(
            created_transport.last().url,
            "https://dav.example.com/dav/PiAgentSync/snapshot"
        );

        for status in [301, 302, 405] {
            let (existing, _) = client(vec![Ok(response(status))]);
            assert!(!existing.mkcol("snapshot").unwrap(), "状态 {status}");
        }

        // 调用方多写一个尾斜杠也不该发出去：有的服务端回 405 却没创建。
        let (stripped, transport) = client(vec![Ok(response(201))]);
        stripped.mkcol("snapshot/").unwrap();
        assert_eq!(
            transport.last().url,
            "https://dav.example.com/dav/PiAgentSync/snapshot"
        );
    }

    #[test]
    fn mkcol_maps_errors() {
        let (client, _) = client(vec![Ok(response(403))]);

        assert_eq!(client.mkcol("snapshot"), Err(DavError::AuthFailed));
    }

    #[test]
    fn put_passes_body_and_condition_headers_through() {
        let (client, transport) = client(vec![Ok(response(201))]);
        let headers = vec![("If-Match".to_string(), "\"v1\"".to_string())];

        client
            .put("snapshot/manifest.json", b"{}", &headers)
            .unwrap();

        let request = transport.last();
        assert_eq!(request.method, "PUT");
        assert_eq!(
            request.url,
            "https://dav.example.com/dav/PiAgentSync/snapshot/manifest.json"
        );
        assert_eq!(request.body.as_deref(), Some(&b"{}"[..]));
        assert_eq!(header(&request, "If-Match").as_deref(), Some("\"v1\""));
        assert_eq!(request.timeout, TRANSFER_TIMEOUT);
    }

    #[test]
    fn put_reports_conditional_failure_as_precondition_failed() {
        let (client, _) = client(vec![Ok(response(412))]);

        assert_eq!(
            client.put("snapshot/manifest.json", b"{}", &[]),
            Err(DavError::PreconditionFailed)
        );
    }

    #[test]
    fn rename_sends_absolute_encoded_destination_with_overwrite() {
        let (client, transport) = client(vec![Ok(response(201))]);

        let outcome = client
            .rename(".tmp-1-deadbeef", "快照 包.zip", false)
            .unwrap();

        assert_eq!(outcome, RenameOutcome::Renamed);
        let request = transport.last();
        assert_eq!(request.method, "MOVE");
        assert_eq!(
            request.url,
            "https://dav.example.com/dav/PiAgentSync/.tmp-1-deadbeef"
        );
        assert_eq!(
            header(&request, "Destination").as_deref(),
            Some("https://dav.example.com/dav/PiAgentSync/%E5%BF%AB%E7%85%A7%20%E5%8C%85.zip")
        );
        assert_eq!(header(&request, "Overwrite").as_deref(), Some("F"));
        assert_eq!(request.timeout, CONTROL_TIMEOUT);
    }

    #[test]
    fn rename_accepts_204_and_overwrite() {
        let (client, transport) = client(vec![Ok(response(204))]);

        let outcome = client.rename("a", "b", true).unwrap();

        assert_eq!(outcome, RenameOutcome::Renamed);
        assert_eq!(header(&transport.last(), "Overwrite").as_deref(), Some("T"));
    }

    #[test]
    fn rename_reports_existing_target_separately_from_network_failures() {
        for status in [409, 412] {
            let (client, _) = client(vec![Ok(response(status))]);

            assert_eq!(
                client.rename("a", "b", false),
                Ok(RenameOutcome::TargetExists),
                "状态 {status}"
            );
        }

        let (client, _) = client(vec![Ok(response(500))]);
        assert_eq!(
            client.rename("a", "b", false),
            Err(DavError::Unreachable("服务端返回 HTTP 500".to_string()))
        );
    }

    #[test]
    fn get_returns_body_and_etag() {
        let (client, transport) = client(vec![Ok(response_with(
            200,
            vec![("ETag".to_string(), "\"v1\"".to_string())],
            b"manifest",
        ))]);

        let (body, etag) = client.get("manifest.json").unwrap().unwrap();

        assert_eq!(body, b"manifest");
        assert_eq!(etag.as_deref(), Some("\"v1\""));
        assert_eq!(transport.last().method, "GET");
        assert_eq!(transport.last().timeout, TRANSFER_TIMEOUT);
    }

    #[test]
    fn get_accepts_lowercase_etag_header() {
        let (client, _) = client(vec![Ok(response_with(
            200,
            vec![("etag".to_string(), "W/\"abc\"".to_string())],
            b"",
        ))]);

        let (_, etag) = client.get("manifest.json").unwrap().unwrap();

        assert_eq!(etag.as_deref(), Some("W/\"abc\""));
    }

    #[test]
    fn get_missing_file_is_none() {
        let (client, _) = client(vec![Ok(response(404))]);

        assert_eq!(client.get("manifest.json"), Ok(None));
    }

    #[test]
    fn delete_treats_404_as_done() {
        let (removed, transport) = client(vec![Ok(response(204)), Ok(response(404))]);

        assert_eq!(removed.delete("probe.txt"), Ok(()));
        assert_eq!(removed.delete("probe.txt"), Ok(()));
        assert_eq!(transport.requests().len(), 2);
        assert_eq!(transport.last().method, "DELETE");
    }

    #[test]
    fn error_mapping_covers_known_statuses() {
        let cases = [
            (401, "", DavError::AuthFailed),
            (403, "Forbidden", DavError::AuthFailed),
            (507, "", DavError::Quota),
            (403, "Insufficient Storage", DavError::Quota),
            (403, "空间不足", DavError::Quota),
            (
                500,
                "",
                DavError::Unreachable("服务端返回 HTTP 500".to_string()),
            ),
            (418, "", DavError::Other("服务端返回 HTTP 418".to_string())),
        ];

        for (status, body, expected) in cases {
            let (client, _) = client(vec![Ok(response_with(status, Vec::new(), body.as_bytes()))]);
            assert_eq!(client.get("manifest.json"), Err(expected), "状态 {status}");
        }
    }

    #[test]
    fn transport_failure_is_passed_through() {
        let (client, _) = client(vec![Err(DavError::Unreachable(
            "请求超时：operation timed out".to_string(),
        ))]);

        match client.get("manifest.json") {
            Err(DavError::Unreachable(detail)) => assert!(detail.contains("请求超时")),
            other => panic!("期望 Unreachable，得到 {other:?}"),
        }
    }

    #[test]
    fn urls_join_base_with_and_without_trailing_slash() {
        for base in [
            "https://dav.example.com/dav",
            "https://dav.example.com/dav/",
        ] {
            let transport = FakeTransport::new(vec![Ok(response(207))]);
            let client = WebdavClient::new(transport.clone(), base);

            assert_eq!(
                client.root(),
                "https://dav.example.com/dav/PiAgentSync",
                "base {base}"
            );

            let _ = client.propfind("snapshot");
            assert_eq!(
                transport.last().url,
                "https://dav.example.com/dav/PiAgentSync/snapshot"
            );
        }
    }

    #[test]
    fn urls_are_percent_encoded_per_segment() {
        let transport = FakeTransport::new(vec![Ok(response(201))]);
        let client = WebdavClient::new(transport.clone(), BASE);

        client.put("snapshot/配置 文件.json", b"x", &[]).unwrap();

        assert_eq!(
            transport.last().url,
            "https://dav.example.com/dav/PiAgentSync/snapshot/%E9%85%8D%E7%BD%AE%20%E6%96%87%E4%BB%B6.json"
        );
    }

    fn probe_responses_with_delete(delete: u16) -> Vec<Result<DavResponse, DavError>> {
        vec![
            Ok(response_with(207, Vec::new(), MIXED_MULTISTATUS.as_bytes())),
            Ok(response(201)),
            Ok(response(delete)),
        ]
    }

    #[test]
    fn probe_creates_missing_work_root() {
        let (client, transport) = client(vec![
            Ok(response(404)),
            Ok(response(201)),
            Ok(response_with(
                207,
                Vec::new(),
                ROOT_ONLY_MULTISTATUS.as_bytes(),
            )),
            Ok(response(201)),
            Ok(response(204)),
        ]);

        let outcome = probe(&client).expect("探测应成功");

        assert_eq!(
            outcome,
            ProbeOutcome {
                created: true,
                leftover_write_test_file: false,
            }
        );

        let requests = transport.requests();
        let methods: Vec<&str> = requests
            .iter()
            .map(|request| request.method.as_str())
            .collect();
        assert_eq!(
            methods,
            vec!["PROPFIND", "MKCOL", "PROPFIND", "PUT", "DELETE"]
        );
        assert_eq!(requests[1].url, "https://dav.example.com/dav/PiAgentSync");
        assert_eq!(
            requests[3].url,
            "https://dav.example.com/dav/PiAgentSync/.tmp-writetest"
        );
        assert_eq!(requests[3].body.as_deref(), Some(&b"ok"[..]));
        assert!(requests[3].headers.is_empty(), "写测试文件不该带条件头");
    }

    #[test]
    fn probe_reuses_existing_work_root_without_mkcol() {
        let (client, transport) = client(probe_responses_with_delete(204));

        let outcome = probe(&client).expect("探测应成功");

        assert_eq!(
            outcome,
            ProbeOutcome {
                created: false,
                leftover_write_test_file: false,
            }
        );

        let methods: Vec<String> = transport
            .requests()
            .into_iter()
            .map(|request| request.method)
            .collect();
        assert_eq!(methods, vec!["PROPFIND", "PUT", "DELETE"]);
    }

    #[test]
    fn probe_reports_leftover_write_test_file_when_delete_is_forbidden() {
        let (client, _) = client(probe_responses_with_delete(403));

        let outcome = probe(&client).expect("删除失败不该整体失败");

        assert_eq!(
            outcome,
            ProbeOutcome {
                created: false,
                leftover_write_test_file: true,
            }
        );
    }

    #[test]
    fn probe_maps_401_to_auth_failed() {
        let (client, _) = client(vec![Ok(response(401))]);

        assert_eq!(probe(&client), Err(DavError::AuthFailed));
    }

    #[test]
    fn probe_fails_when_created_root_is_still_missing() {
        let (client, _) = client(vec![
            Ok(response(404)),
            Ok(response(201)),
            Ok(response_with(207, Vec::new(), EMPTY_MULTISTATUS.as_bytes())),
        ]);

        match probe(&client) {
            Err(DavError::Other(detail)) => {
                assert!(detail.contains("工作目录不存在且创建失败"), "{detail}");
            }
            other => panic!("期望 Other，得到 {other:?}"),
        }
    }

    #[test]
    fn probe_does_not_claim_creation_when_mkcol_says_it_exists() {
        let (client, _) = client(vec![
            Ok(response(404)),
            Ok(response(405)),
            Ok(response_with(
                207,
                Vec::new(),
                ROOT_ONLY_MULTISTATUS.as_bytes(),
            )),
            Ok(response(201)),
            Ok(response(204)),
        ]);

        let outcome = probe(&client).expect("探测应成功");

        assert!(!outcome.created);
    }

    #[test]
    fn timeouts_can_be_overridden_for_tests() {
        let transport = FakeTransport::new(vec![Ok(response(207)), Ok(response(404))]);
        let client = WebdavClient::new(transport.clone(), BASE)
            .with_timeouts(Duration::from_millis(5), Duration::from_millis(7));

        let _ = client.propfind("snapshot");
        let _ = client.get("manifest.json");

        let requests = transport.requests();
        assert_eq!(requests[0].timeout, Duration::from_millis(5));
        assert_eq!(requests[1].timeout, Duration::from_millis(7));
    }
}

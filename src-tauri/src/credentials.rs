//! WebDAV 凭据：只存地址、用户名与应用密码。
//!
//! 与同步文件夹分开放在应用数据目录下的 `webdav.json`（0600、原子写），
//! 因为凭据既不属于快照内容，也不能随快照或云盘客户端离开本机。
//! 密码只在本模块内部出现：结构体 `Debug` 与所有错误信息都不得带出明文。

use crate::config;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 应用数据目录下的凭据文件名。
pub const WEBDAV_FILE_NAME: &str = "webdav.json";

/// 序列化时替换密码的占位文本，保证 `Debug` 输出不含明文。
const REDACTED: &str = "<redacted>";

/// WebDAV 凭据。字段名与 `docs/design/40-webdav.md` 一致。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebdavCredentials {
    pub url: String,
    pub username: String,
    pub password: String,
}

impl fmt::Debug for WebdavCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebdavCredentials")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("password", &REDACTED)
            .finish()
    }
}

/// 读写凭据时可能出现的可读错误。所有分支都不含密码内容。
#[derive(Debug)]
pub enum CredentialError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Write {
        path: PathBuf,
        source: io::Error,
    },
    Corrupt {
        path: PathBuf,
        detail: String,
    },
    /// 地址不是 http/https，或取不出主机名。
    InvalidUrl,
    /// 用户名为空。
    MissingUsername,
    /// 首次保存时没有给密码。
    MissingPassword,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredentialError::Read { path, source } => {
                write!(formatter, "无法读取 WebDAV 凭据 {}：{}", path.display(), source)
            }
            CredentialError::Write { path, source } => {
                write!(formatter, "无法保存 WebDAV 凭据 {}：{}", path.display(), source)
            }
            CredentialError::Corrupt { path, detail } => write!(
                formatter,
                "WebDAV 凭据文件格式损坏（{}）：{}；原文件未改动",
                path.display(),
                detail
            ),
            CredentialError::InvalidUrl => {
                write!(formatter, "WebDAV 地址必须以 http:// 或 https:// 开头，并包含主机名")
            }
            CredentialError::MissingUsername => write!(formatter, "WebDAV 用户名不能为空"),
            CredentialError::MissingPassword => {
                write!(
                    formatter,
                    "没有可沿用的 WebDAV 密码，请填写应用密码（更换地址或用户名后需重新填写）"
                )
            }
        }
    }
}

impl Error for CredentialError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            CredentialError::Read { source, .. } | CredentialError::Write { source, .. } => {
                Some(source)
            }
            _ => None,
        }
    }
}

/// 读取凭据。文件不存在时返回 `Ok(None)`；内容非法时报错且不改文件。
pub fn load(dir: &Path) -> Result<Option<WebdavCredentials>, CredentialError> {
    let path = dir.join(WEBDAV_FILE_NAME);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(CredentialError::Read { path, source }),
    };

    let parsed: WebdavCredentials =
        serde_json::from_str(&contents).map_err(|error| CredentialError::Corrupt {
            path,
            detail: error.to_string(),
        })?;

    Ok(Some(parsed))
}

/// 保存凭据：同目录临时文件 + rename 原子替换，文件创建即 0600。
pub fn save(dir: &Path, credentials: &WebdavCredentials) -> Result<(), CredentialError> {
    fs::create_dir_all(dir).map_err(|source| CredentialError::Write {
        path: dir.to_path_buf(),
        source,
    })?;

    let target = dir.join(WEBDAV_FILE_NAME);
    let temp = dir.join(format!("{WEBDAV_FILE_NAME}.tmp"));

    let mut contents =
        serde_json::to_string_pretty(credentials).map_err(|error| CredentialError::Write {
            path: target.clone(),
            source: io::Error::new(io::ErrorKind::InvalidData, error.to_string()),
        })?;
    contents.push('\n');

    config::write_private_file(&temp, contents.as_bytes()).map_err(|source| {
        let _ = fs::remove_file(&temp);
        CredentialError::Write {
            path: temp.clone(),
            source,
        }
    })?;

    if let Err(source) = fs::rename(&temp, &target) {
        let _ = fs::remove_file(&temp);
        return Err(CredentialError::Write {
            path: target,
            source,
        });
    }

    Ok(())
}

/// 保存前的最小校验：地址只接受 http/https，用户名不能为空。
///
/// 只做形状检查，不探测连通性——连通性由单独的探测流程负责。
pub fn validate(url: &str, username: &str) -> Result<(String, String), CredentialError> {
    let url = url.trim();
    let username = username.trim();

    if !is_http_url(url) {
        return Err(CredentialError::InvalidUrl);
    }
    if username.is_empty() {
        return Err(CredentialError::MissingUsername);
    }

    Ok((url.to_string(), username.to_string()))
}

fn is_http_url(url: &str) -> bool {
    let rest = match url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    {
        Some(rest) => rest,
        None => return false,
    };

    // 主机名在第一个 `/`、`?`、`#` 之前；空主机名（如 `https:///dav`）不算可解析。
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    !host.is_empty() && !host.contains(char::is_whitespace)
}

/// 解析一次网络请求要用的凭据：空密码等于沿用已存值，与 [`upsert`] 的保存语义一致。
///
/// 与 `upsert` 分开是因为探测不该落盘：它只借已存密码，不写文件。
/// 端点等价：只忽略尾部斜杠；地址或用户名变了就不能沿用旧密码。
fn same_endpoint(left: &str, right: &str) -> bool {
    left.trim_end_matches('/') == right.trim_end_matches('/')
}

pub fn resolve(
    dir: &Path,
    url: &str,
    username: &str,
    password: Option<&str>,
) -> Result<WebdavCredentials, CredentialError> {
    let (url, username) = validate(url, username)?;

    let password = match password.map(str::trim) {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => match load(dir)? {
            // 只在端点未变时沿用旧密码：否则“换个地址 + 密码留空”会把旧密码发给新主机。
            Some(stored)
                if same_endpoint(&stored.url, &url) && stored.username == username =>
            {
                stored.password
            }
            _ => return Err(CredentialError::MissingPassword),
        },
    };

    Ok(WebdavCredentials {
        url,
        username,
        password,
    })
}

/// 保存或更新凭据：空密码等于保留原值，避免只改地址就把密码清掉。
pub fn upsert(
    dir: &Path,
    url: &str,
    username: &str,
    password: Option<&str>,
) -> Result<WebdavCredentials, CredentialError> {
    let credentials = resolve(dir, url, username, password)?;
    save(dir, &credentials)?;

    Ok(credentials)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT_DIR_ID: AtomicU32 = AtomicU32::new(0);

    /// 临时目录夹具：不引入 tempfile，退出时自行清理。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = NEXT_DIR_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-credentials-{}-{}-{}",
                label,
                std::process::id(),
                unique
            ));
            if path.exists() {
                fs::remove_dir_all(&path).expect("清理残留临时目录");
            }
            fs::create_dir_all(&path).expect("创建临时目录");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn join(&self, child: &str) -> PathBuf {
            self.path.join(child)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn sample() -> WebdavCredentials {
        WebdavCredentials {
            url: "https://dav.jianguoyun.com/dav".to_string(),
            username: "someone@example.com".to_string(),
            password: "app-password-123".to_string(),
        }
    }

    #[test]
    fn load_returns_none_when_file_is_missing() {
        let dir = TempDir::new("missing");
        assert_eq!(load(dir.path()).expect("缺文件应返回 None"), None);
        assert!(!dir.join(WEBDAV_FILE_NAME).exists());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = TempDir::new("round-trip");
        let credentials = sample();
        save(dir.path(), &credentials).expect("保存应成功");

        let loaded = load(dir.path()).expect("读取应成功");
        assert_eq!(loaded, Some(credentials));
    }

    #[test]
    fn file_holds_only_url_username_password() {
        let dir = TempDir::new("keys");
        save(dir.path(), &sample()).expect("保存应成功");

        let text = fs::read_to_string(dir.join(WEBDAV_FILE_NAME)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let keys: std::collections::BTreeSet<&str> = value
            .as_object()
            .expect("凭据应为 JSON 对象")
            .keys()
            .map(String::as_str)
            .collect();
        let expected: std::collections::BTreeSet<&str> =
            ["url", "username", "password"].into_iter().collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn corrupt_file_is_reported_and_left_untouched() {
        let dir = TempDir::new("corrupt");
        let path = dir.join(WEBDAV_FILE_NAME);
        fs::write(&path, "{ not json").unwrap();

        let error = load(dir.path()).expect_err("非法内容应报错");
        assert!(matches!(error, CredentialError::Corrupt { .. }));
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_private_from_the_moment_it_exists() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("private");
        save(dir.path(), &sample()).expect("保存应成功");

        let mode = fs::metadata(dir.join(WEBDAV_FILE_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "凭据文件权限应为 0600");
    }

    #[test]
    fn debug_output_never_contains_the_password() {
        let credentials = sample();
        let rendered = format!("{credentials:?}");
        assert!(rendered.contains("https://dav.jianguoyun.com/dav"));
        assert!(!rendered.contains(&credentials.password), "{rendered}");
    }

    #[test]
    fn validation_rejects_non_http_urls_and_empty_usernames() {
        for url in ["", "dav.jianguoyun.com/dav", "ftp://example.com", "https://"] {
            let error = validate(url, "someone").expect_err("地址应被拒绝");
            assert!(matches!(error, CredentialError::InvalidUrl), "{url}");
        }

        let error = validate("https://dav.jianguoyun.com/dav", "   ").expect_err("用户名应被拒绝");
        assert!(matches!(error, CredentialError::MissingUsername));

        let (url, username) = validate("  https://dav.jianguoyun.com/dav  ", " someone ").unwrap();
        assert_eq!(url, "https://dav.jianguoyun.com/dav");
        assert_eq!(username, "someone");
    }

    #[test]
    fn blank_password_keeps_the_saved_one() {
        let dir = TempDir::new("keep-password");
        save(dir.path(), &sample()).expect("保存应成功");

        for password in [None, Some(""), Some("   ")] {
            let saved = upsert(dir.path(), &sample().url, &sample().username, password)
                .expect("保存应成功");
            assert_eq!(saved.url, sample().url);
            assert_eq!(saved.password, sample().password);
        }
    }

    #[test]
    fn new_password_replaces_the_saved_one() {
        let dir = TempDir::new("replace-password");
        save(dir.path(), &sample()).expect("保存应成功");

        upsert(dir.path(), "https://example.com/dav", "someone", Some("next"))
            .expect("保存应成功");
        assert_eq!(load(dir.path()).unwrap().unwrap().password, "next");
    }

    #[test]
    fn resolve_borrows_the_saved_password_without_writing() {
        let dir = TempDir::new("resolve-keep");
        save(dir.path(), &sample()).expect("保存应成功");

        let resolved = resolve(dir.path(), " https://dav.jianguoyun.com/dav ", " someone@example.com ", Some("  "))
            .expect("应沿用已存密码");

        assert_eq!(resolved.url, "https://dav.jianguoyun.com/dav");
        assert_eq!(resolved.username, "someone@example.com");
        assert_eq!(resolved.password, sample().password);
        // 探测不落盘：文件仍是探测前的内容。
        assert_eq!(load(dir.path()).unwrap(), Some(sample()));
    }

    #[test]
    fn resolve_does_not_reuse_password_for_another_host() {
        let dir = TempDir::new("resolve-host");
        save(dir.path(), &sample()).expect("保存应成功");

        let error = resolve(dir.path(), "https://evil.example/dav", "someone@example.com", None)
            .expect_err("换了主机就不能沿用旧密码");
        assert!(matches!(error, CredentialError::MissingPassword));
    }

    #[test]
    fn resolve_does_not_reuse_password_for_another_username() {
        let dir = TempDir::new("resolve-user");
        save(dir.path(), &sample()).expect("保存应成功");

        let error = resolve(dir.path(), "https://dav.jianguoyun.com/dav", "another@example.com", None)
            .expect_err("换了用户名就不能沿用旧密码");
        assert!(matches!(error, CredentialError::MissingPassword));
    }

    #[test]
    fn resolve_tolerates_trailing_slash_when_reusing_password() {
        let dir = TempDir::new("resolve-slash");
        save(dir.path(), &sample()).expect("保存应成功");

        let resolved = resolve(dir.path(), "https://dav.jianguoyun.com/dav/", "someone@example.com", None)
            .expect("仅尾斜杠不同应沿用旧密码");
        assert_eq!(resolved.password, sample().password);
    }

    #[test]
    fn resolve_prefers_the_given_password() {
        let dir = TempDir::new("resolve-new");
        save(dir.path(), &sample()).expect("保存应成功");

        let resolved = resolve(dir.path(), "https://example.com/dav", "someone", Some(" next "))
            .expect("应使用传入密码");

        assert_eq!(resolved.password, "next");
        assert_eq!(load(dir.path()).unwrap(), Some(sample()));
    }

    #[test]
    fn resolve_without_any_password_is_rejected() {
        let dir = TempDir::new("resolve-missing");

        for password in [None, Some("")] {
            let error = resolve(dir.path(), "https://example.com/dav", "someone", password)
                .expect_err("无密码应报错");
            assert!(matches!(error, CredentialError::MissingPassword));
        }
        assert!(!dir.join(WEBDAV_FILE_NAME).exists());
    }

    #[test]
    fn first_save_without_password_is_rejected() {
        let dir = TempDir::new("first-password");
        let error = upsert(dir.path(), "https://example.com/dav", "someone", None)
            .expect_err("首次保存缺密码应报错");
        assert!(matches!(error, CredentialError::MissingPassword));
        assert!(!dir.join(WEBDAV_FILE_NAME).exists());
    }

    #[test]
    fn invalid_input_never_writes_a_file() {
        let dir = TempDir::new("invalid-input");
        let cases = [
            ("ftp://example.com", "someone", Some("secret")),
            ("https://example.com", "", Some("secret")),
        ];
        for (url, username, password) in cases {
            upsert(dir.path(), url, username, password).expect_err("非法输入应被拒绝");
            assert!(!dir.join(WEBDAV_FILE_NAME).exists());
        }
    }

    #[test]
    fn error_messages_never_contain_the_password() {
        let dir = TempDir::new("error-text");
        fs::write(
            dir.join(WEBDAV_FILE_NAME),
            r#"{"url":"https://example.com/dav","username":"someone","password":"app-password-123""#,
        )
        .unwrap();

        let error = load(dir.path()).expect_err("非法内容应报错");
        let rendered = error.to_string();
        assert!(!rendered.contains("app-password-123"), "{rendered}");
    }
}

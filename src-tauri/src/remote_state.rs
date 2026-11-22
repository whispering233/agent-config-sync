//! 本机记的远端快照包状态：上次成功上传/下载时看到的包名集合与时间。
//!
//! 规则来源：`docs/design/40-webdav.md` 第 7 节。只记包名与指纹，不保存远端内容；
//! 文件位于应用数据目录（与 `webdav.json` 同级），按私有文件写（0600、原子替换）。

use crate::config;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 应用数据目录下的状态文件名。
pub const REMOTE_STATE_FILE_NAME: &str = "remote-state.json";

/// 远端状态。字段名与设计文档第 7 节一致（camelCase）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteState {
    /// RFC 3339 UTC。
    pub seen_at: String,
    /// 上次记录时远端可解析的包名集合。
    pub packages: Vec<String>,
    /// 上一次成功上传的完整指纹；从未上传过（或只有下载记录）时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_uploaded_fingerprint: Option<String>,
}

fn path(app_data: &Path) -> PathBuf {
    app_data.join(REMOTE_STATE_FILE_NAME)
}

/// 读取本机记录。文件缺失或解析不了都按「无记录」处理：换机器、换地址、旧版本遗留时
/// 表现是一次保守的全新上传，不丢数据，也不让坏文件把上传永久卡住。
pub fn load(app_data: &Path) -> RemoteState {
    fs::read(path(app_data))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// 写入本机记录：同目录临时文件 + rename 原子替换，文件创建即 0600。
pub fn save(app_data: &Path, state: &RemoteState) -> io::Result<()> {
    fs::create_dir_all(app_data)?;

    let target = path(app_data);
    let temp = app_data.join(format!("{REMOTE_STATE_FILE_NAME}.tmp"));

    let mut contents = serde_json::to_string_pretty(state)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    contents.push('\n');

    if let Err(error) = config::write_private_file(&temp, contents.as_bytes()) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temp, &target) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT_DIR_ID: AtomicU32 = AtomicU32::new(0);

    /// 临时目录夹具：不引入 tempfile，退出时自行清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = NEXT_DIR_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-remote-state-{}-{}-{}",
                label,
                std::process::id(),
                unique
            ));
            let _ = fs::remove_dir_all(&path);
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

    fn sample() -> RemoteState {
        RemoteState {
            seen_at: "2026-11-22T10:52:30Z".to_string(),
            packages: vec![
                "20261122-105230-deadbeef.zip".to_string(),
                "20261121-091210-7f3e9c01.zip".to_string(),
            ],
            last_uploaded_fingerprint: Some("deadbeefcafe".to_string()),
        }
    }

    #[test]
    fn missing_state_reads_as_no_record() {
        let dir = TempDir::new("missing");
        assert_eq!(load(dir.path()), RemoteState::default());

        // 坏文件同样按无记录处理，不阻断上传。
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(dir.path().join(REMOTE_STATE_FILE_NAME), b"{not json").unwrap();
        assert_eq!(load(dir.path()), RemoteState::default());
    }

    #[test]
    fn state_round_trips_with_documented_field_names() {
        let dir = TempDir::new("round-trip");
        let state = sample();
        save(dir.path(), &state).unwrap();

        assert_eq!(load(dir.path()), state);

        let text = fs::read_to_string(dir.path().join(REMOTE_STATE_FILE_NAME)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["seenAt"], "2026-11-22T10:52:30Z");
        assert_eq!(value["packages"][0], "20261122-105230-deadbeef.zip");
        assert_eq!(value["lastUploadedFingerprint"], "deadbeefcafe");

        // 写完后不留临时文件。
        assert!(!dir
            .path()
            .join(format!("{REMOTE_STATE_FILE_NAME}.tmp"))
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_written_with_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("mode");
        save(dir.path(), &sample()).unwrap();

        let mode = fs::metadata(dir.path().join(REMOTE_STATE_FILE_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn state_without_upload_fingerprint_omits_the_field() {
        let dir = TempDir::new("no-fingerprint");
        let state = RemoteState {
            seen_at: "2026-11-22T10:52:30Z".to_string(),
            packages: vec!["20261122-105230-deadbeef.zip".to_string()],
            last_uploaded_fingerprint: None,
        };
        save(dir.path(), &state).unwrap();

        assert_eq!(load(dir.path()), state);
    }
}

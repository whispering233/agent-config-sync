//! 快照包的打包、解包与包名。
//!
//! 格式与安全闸门来源：`docs/design/10-snapshot-sync.md` 第 2 节。
//! 本模块只处理字节：manifest 由调用方序列化好交给 [`pack`]，解出来的仍是字节，
//! 这里不认识 manifest 结构，也不碰受管范围、计划与网络。

use crate::platform;
use crate::snapshot;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// manifest 在包根，与 `snapshot/` 平级。
pub const MANIFEST_ENTRY: &str = "manifest.json";
/// 快照内容在包内的前缀，与 manifest 里 `files[].path` 的写法一致。
pub const SNAPSHOT_ENTRY_PREFIX: &str = "snapshot/";
/// 上传中的临时名前缀：这个名字下的文件永不当作包。
pub const TEMP_NAME_PREFIX: &str = ".tmp-";

const COPY_BUFFER: usize = 64 * 1024;
/// manifest 是被单独读的唯一条目，它自己也要有体积上限（不可信输入）。
const MANIFEST_MAX_BYTES: u64 = 1024 * 1024;
/// 解包落盘的权限：暂存目录可能是下一次应用的对象，而快照含明文密钥。
/// 不套用包内记录的 mode——那是不可信输入。
const UNPACK_DIR_MODE: u32 = 0o700;
const UNPACK_FILE_MODE: u32 = 0o600;
/// 包名的固定形态 `<yyyymmdd-HHmmss>-<fp8>.zip`，逐位校验时用到它的长度。
const NAME_LEN: usize = 28;

/// 解包闸门的上限。真实受管范围约几十到几百个文件、约 100 KB，这里留了两个数量级余量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub entries: usize,
    pub unpacked_bytes: u64,
    pub package_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: 20_000,
            unpacked_bytes: 256 * 1024 * 1024,
            package_bytes: 64 * 1024 * 1024,
        }
    }
}

/// 解包结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnpackReport {
    /// 写出的文件数，含 `manifest.json`。
    pub files: usize,
    /// 写出的字节数，含 `manifest.json`。
    pub bytes: u64,
}

/// 包操作失败。`code` 取自 `docs/design/10-snapshot-sync.md` 第 9 节的失败码表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageError {
    pub code: &'static str,
    pub message: String,
}

impl PackageError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn corrupt(message: impl Into<String>) -> Self {
        Self::new("package_corrupt", message)
    }

    fn unsafe_entry(message: impl Into<String>) -> Self {
        Self::new("package_unsafe_entry", message)
    }

    fn too_large(message: impl Into<String>) -> Self {
        Self::new("package_too_large", message)
    }

    fn write_failed(message: impl Into<String>) -> Self {
        Self::new("no_write_permission", message)
    }

    fn export_failed(message: impl Into<String>) -> Self {
        Self::new("export_failed", message)
    }
}

impl fmt::Display for PackageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PackageError {}

/// 包名。`stamp` 是 `yyyymmdd-HHmmss`（UTC），`unix_seconds` 是它的回读结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageName {
    pub stamp: String,
    pub fingerprint8: String,
    pub unix_seconds: u64,
}

/// 按 `<UTC yyyymmdd-HHmmss>-<fingerprint 前 8 位小写>.zip` 生成包名。
/// 时间只用 UTC 且到秒：字典序即时间序，"最新 = 名字最大"才成立。
pub fn build_name(unix_seconds: u64, fingerprint: &str) -> String {
    let fingerprint8: String = fingerprint
        .chars()
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    format!(
        "{}-{fingerprint8}.zip",
        snapshot::format_compact_stamp(unix_seconds)
    )
}

/// 解析包名。只认 `^\d{8}-\d{6}-[0-9a-f]{8}\.zip$` 这一个形态，别的文件
/// （`.tmp-*`、云盘冲突副本、用户手放的文件）一律返回 `None`。
pub fn parse_name(name: &str) -> Option<PackageName> {
    if name.len() != NAME_LEN || !name.is_ascii() {
        return None;
    }
    let (head, tail) = name.split_at(16);
    let stamp = head.strip_suffix('-')?;
    let (fingerprint8, suffix) = tail.split_at(8);
    if suffix != ".zip"
        || !fingerprint8
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    Some(PackageName {
        stamp: stamp.to_string(),
        fingerprint8: fingerprint8.to_string(),
        unix_seconds: parse_stamp(stamp)?,
    })
}

/// `yyyymmdd-HHmmss` 回读成 UTC 秒。日期必须与日历换算往返一致：
/// `20261301`、`20260230` 这类会进位成别的日期，靠反向换算掐掉。
fn parse_stamp(stamp: &str) -> Option<u64> {
    if stamp.len() != 15 || stamp.as_bytes()[8] != b'-' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<u64> {
        let text = stamp.get(range)?;
        if !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    };
    let year = number(0..4)?;
    let month = number(4..6)?;
    let day = number(6..8)?;
    let hour = number(9..11)?;
    let minute = number(11..13)?;
    let second = number(13..15)?;
    if !(1..=12).contains(&month) || day == 0 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = snapshot::days_from_civil(year as i64, month, day);
    if snapshot::civil_from_days(days) != (year as i64, month, day) || days < 0 {
        return None;
    }
    Some(days as u64 * 86_400 + hour * 3600 + minute * 60 + second)
}

/// 打包：`manifest.json` 原样写入，`snapshot_dir` 的内容递归带 `snapshot/` 前缀。
///
/// `manifest_json` 由调用方序列化：这里不重新序列化，避免打包与 manifest 内容分家。
/// 条目全是普通文件，空目录不写条目；快照目录里出现链接或特殊文件即报错——导出流程
/// 已经把链接物化，出现链接说明调用方漏了这一步，跟着它会静默带进范围外的内容。
pub fn pack(dest: &Path, manifest_json: &[u8], snapshot_dir: &Path) -> Result<(), PackageError> {
    let result = write_package(dest, manifest_json, snapshot_dir);
    if result.is_err() {
        // 正式名下出现即代表包完整，留着半个包比删掉危险。
        let _ = fs::remove_file(dest);
    }
    result
}

fn write_package(
    dest: &Path,
    manifest_json: &[u8],
    snapshot_dir: &Path,
) -> Result<(), PackageError> {
    let file = File::create(dest).map_err(|error| {
        PackageError::export_failed(format!("无法创建快照包 {}：{error}", dest.display()))
    })?;
    let mut writer = ZipWriter::new(file);
    writer
        .start_file(MANIFEST_ENTRY, file_options(None))
        .map_err(|error| PackageError::export_failed(format!("无法写入 manifest 条目：{error}")))?;
    writer
        .write_all(manifest_json)
        .map_err(|error| PackageError::export_failed(format!("无法写入 manifest 条目：{error}")))?;
    add_tree(&mut writer, snapshot_dir, SNAPSHOT_ENTRY_PREFIX)?;
    writer
        .finish()
        .map_err(|error| PackageError::export_failed(format!("无法完成快照包：{error}")))?;
    Ok(())
}

fn add_tree(writer: &mut ZipWriter<File>, dir: &Path, prefix: &str) -> Result<(), PackageError> {
    let entries = fs::read_dir(dir).map_err(|error| {
        PackageError::export_failed(format!("无法读取快照目录 {}：{error}", dir.display()))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            PackageError::export_failed(format!("无法读取快照目录 {}：{error}", dir.display()))
        })?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            PackageError::export_failed(format!("无法读取 {}：{error}", path.display()))
        })?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(PackageError::export_failed(format!(
                "快照目录里有符号链接：{}",
                path.display()
            )));
        }
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            return Err(PackageError::export_failed(format!(
                "快照目录里有非 UTF-8 文件名：{}",
                path.display()
            )));
        };
        let name = format!("{prefix}{file_name}");
        if file_type.is_dir() {
            add_tree(writer, &path, &format!("{name}/"))?;
        } else if file_type.is_file() {
            let mode = platform::mode_of(&metadata);
            writer
                .start_file(&name, file_options(mode))
                .map_err(|error| {
                    PackageError::export_failed(format!("无法写入条目 {name}：{error}"))
                })?;
            let mut source = File::open(&path).map_err(|error| {
                PackageError::export_failed(format!("无法读取 {}：{error}", path.display()))
            })?;
            io::copy(&mut source, writer).map_err(|error| {
                PackageError::export_failed(format!("无法写入条目 {name}：{error}"))
            })?;
        } else {
            return Err(PackageError::export_failed(format!(
                "快照目录里有非普通文件：{}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn file_options(mode: Option<u32>) -> SimpleFileOptions {
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    match mode {
        Some(mode) => options.unix_permissions(mode),
        None => options,
    }
}

/// 只读包里的 `manifest.json`：列举历史时看指纹、导出前判「内容未变」都只需要它，
/// 为此解整包会把上百个文件写到盘上再删掉。
pub fn read_manifest(archive: &Path) -> Result<Vec<u8>, PackageError> {
    let size = fs::metadata(archive)
        .map_err(|error| {
            PackageError::corrupt(format!("无法读取包文件 {}：{error}", archive.display()))
        })?
        .len();
    if size > Limits::default().package_bytes {
        return Err(PackageError::too_large(format!(
            "包体积 {size} 字节，超过上限 {}",
            Limits::default().package_bytes
        )));
    }
    let file = File::open(archive).map_err(|error| {
        PackageError::corrupt(format!("无法打开包文件 {}：{error}", archive.display()))
    })?;
    let mut zip = ZipArchive::new(file)
        .map_err(|error| PackageError::corrupt(format!("无法解析快照包：{error}")))?;
    let mut entry = zip
        .by_name(MANIFEST_ENTRY)
        .map_err(|_| PackageError::corrupt("包内缺少 manifest.json"))?;
    // manifest 正常只有几 KB；条目声明的体积决定要不要读，读的时候再卡一次硬上限。
    if entry.size() > MANIFEST_MAX_BYTES {
        return Err(PackageError::unsafe_entry(format!(
            "manifest.json 声明 {} 字节，超过 {MANIFEST_MAX_BYTES} 字节上限",
            entry.size()
        )));
    }
    let mut bytes = Vec::new();
    io::Read::take(&mut entry, MANIFEST_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| PackageError::corrupt(format!("manifest.json 读不出来：{error}")))?;
    if bytes.len() as u64 > MANIFEST_MAX_BYTES {
        return Err(PackageError::unsafe_entry(format!(
            "manifest.json 超过 {MANIFEST_MAX_BYTES} 字节上限"
        )));
    }

    Ok(bytes)
}

/// 解包到 `dest`。`dest` 必须是调用方按包名准备的新路径：失败时整棵删掉，不留半套快照。
pub fn unpack(archive: &Path, dest: &Path) -> Result<UnpackReport, PackageError> {
    unpack_with(archive, dest, Limits::default())
}

fn unpack_with(archive: &Path, dest: &Path, limits: Limits) -> Result<UnpackReport, PackageError> {
    // 已存在的路径可能是一份可用的暂存：这里拒绝而不是复用，失败路径才不会删掉别人的目录。
    if dest.symlink_metadata().is_ok() {
        return Err(PackageError::corrupt(format!(
            "暂存目录已存在，拒绝对其解包：{}",
            dest.display()
        )));
    }
    let size = fs::metadata(archive)
        .map_err(|error| {
            PackageError::corrupt(format!("无法读取包文件 {}：{error}", archive.display()))
        })?
        .len();
    if size > limits.package_bytes {
        return Err(PackageError::too_large(format!(
            "包体积 {size} 字节，超过上限 {}",
            limits.package_bytes
        )));
    }
    let file = File::open(archive).map_err(|error| {
        PackageError::corrupt(format!("无法打开包文件 {}：{error}", archive.display()))
    })?;
    let mut zip = ZipArchive::new(file)
        .map_err(|error| PackageError::corrupt(format!("无法解析快照包：{error}")))?;
    if zip.len() > limits.entries {
        return Err(PackageError::unsafe_entry(format!(
            "包内条目数 {} 超过上限 {}",
            zip.len(),
            limits.entries
        )));
    }
    let result = write_entries(&mut zip, dest, limits);
    if result.is_err() {
        let _ = fs::remove_dir_all(dest);
    }
    result
}

/// 逐条目过闸门再写。不使用库的批量解压接口：它们不看条目名，也不做体积累计。
fn write_entries<R: Read + io::Seek>(
    zip: &mut ZipArchive<R>,
    dest: &Path,
    limits: Limits,
) -> Result<UnpackReport, PackageError> {
    // 上层目录（暂存根）可能还不存在：递归建，每一级都按 0700 建，不经过 umask。
    platform::create_dir_all_mode(dest, UNPACK_DIR_MODE).map_err(|error| {
        PackageError::write_failed(format!("无法创建暂存目录 {}：{error}", dest.display()))
    })?;
    let mut report = UnpackReport { files: 0, bytes: 0 };
    let mut has_manifest = false;
    let mut buffer = vec![0u8; COPY_BUFFER];
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|error| {
            PackageError::corrupt(format!("无法读取第 {index} 个条目：{error}"))
        })?;
        let name = entry
            .name()
            .map_err(|error| PackageError::corrupt(format!("条目名不是 UTF-8：{error}")))?
            .into_owned();
        check_entry_kind(&name, entry.unix_mode())?;
        let relative = safe_relative(&name)
            .ok_or_else(|| PackageError::unsafe_entry(format!("条目路径不安全：{name}")))?;
        if name != MANIFEST_ENTRY && !name.starts_with(SNAPSHOT_ENTRY_PREFIX) {
            return Err(PackageError::corrupt(format!("条目不在白名单内：{name}")));
        }
        if entry.is_dir() {
            continue;
        }
        let target = resolve_within(dest, &relative)
            .ok_or_else(|| PackageError::unsafe_entry(format!("条目落点越界：{name}")))?;
        if let Some(parent) = target.parent() {
            platform::create_dir_all_mode(parent, UNPACK_DIR_MODE).map_err(|error| {
                PackageError::write_failed(format!("无法创建目录 {}：{error}", parent.display()))
            })?;
        }
        let mut output = File::create(&target).map_err(|error| {
            PackageError::write_failed(format!("无法写入 {}：{error}", target.display()))
        })?;
        loop {
            let read = entry
                .read(&mut buffer)
                .map_err(|error| PackageError::corrupt(format!("条目 {name} 解压失败：{error}")))?;
            if read == 0 {
                break;
            }
            report.bytes += read as u64;
            if report.bytes > limits.unpacked_bytes {
                return Err(PackageError::unsafe_entry(format!(
                    "解压总体积超过上限 {} 字节",
                    limits.unpacked_bytes
                )));
            }
            output.write_all(&buffer[..read]).map_err(|error| {
                PackageError::write_failed(format!("无法写入 {}：{error}", target.display()))
            })?;
        }
        // `File::create` 按 umask 建（通常 0644），写完补一次显式权限（Windows 上不设）。
        platform::set_mode(&target, UNPACK_FILE_MODE).map_err(|error| {
            PackageError::write_failed(format!("无法设置 {} 的权限：{error}", target.display()))
        })?;
        report.files += 1;
        if name == MANIFEST_ENTRY {
            has_manifest = true;
        }
    }
    if !has_manifest {
        return Err(PackageError::corrupt("包内缺少 manifest.json"));
    }
    Ok(report)
}

/// 条目类型闸门：只放行普通文件与目录。符号链接在 external attributes 里是
/// `S_IFLNK`，设备和 FIFO 同理，都按"非普通文件"拒绝——落盘时就不可能造出链接。
/// `unix_mode` 为 `None` 是 DOS/Windows 造的包，没有类型位可判，按普通文件处理。
fn check_entry_kind(name: &str, unix_mode: Option<u32>) -> Result<(), PackageError> {
    match unix_mode.map(|mode| mode & 0o170000) {
        None | Some(0) | Some(0o040000) | Some(0o100000) => Ok(()),
        Some(_) => Err(PackageError::corrupt(format!(
            "包内出现非普通文件条目：{name}"
        ))),
    }
}

/// 条目名必须能安全地当相对路径用：拒绝绝对路径、`..`、反斜杠、盘符与空段。
/// 目录条目的尾斜杠先去掉再切段，否则最后一段必然为空。
fn safe_relative(name: &str) -> Option<PathBuf> {
    if name.contains(['\\', ':', '\0']) {
        return None;
    }
    let mut relative = PathBuf::new();
    for segment in name.strip_suffix('/').unwrap_or(name).split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return None;
        }
        relative.push(segment);
    }
    Some(relative)
}

/// 落点必须仍在 `dest` 之内。逐段归一化后按组件比较，不用字符串前缀：
/// `dest` 为 `/a/b` 时，`/a/bc` 这种同前缀目录不能被当成子目录。
fn resolve_within(root: &Path, relative: &Path) -> Option<PathBuf> {
    let mut resolved = PathBuf::new();
    for component in root.join(relative).components() {
        match component {
            Component::Normal(_) => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => return None,
            Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
        }
    }
    if !resolved.starts_with(root) {
        return None;
    }
    Some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 临时目录夹具：Drop 时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-package-{}-{}-{}",
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

    /// 造一个只含给定条目的包，用来验证解包闸门。
    fn build_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).expect("创建测试包");
        let mut writer = ZipWriter::new(file);
        for (name, content) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .expect("写入测试条目");
            writer.write_all(content).expect("写入测试内容");
        }
        writer.finish().expect("完成测试包");
    }

    fn write_file(path: &Path, content: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("创建父目录");
        }
        fs::write(path, content).expect("写入夹具文件");
    }

    /// 收集目录下所有文件的相对路径，用来比对解包结果。
    fn relative_files(root: &Path) -> Vec<String> {
        fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) {
            for entry in fs::read_dir(dir).expect("读取目录") {
                let entry = entry.expect("读取目录项");
                let path = entry.path();
                if path.is_dir() {
                    walk(root, &path, found);
                } else {
                    found.push(
                        path.strip_prefix(root)
                            .expect("相对路径")
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        let mut found = Vec::new();
        walk(root, root, &mut found);
        found.sort();
        found
    }

    #[test]
    fn roundtrip_keeps_bytes_and_file_count() {
        let temp = TempDir::new("roundtrip");
        let source = temp.path().join("snapshot");
        let manifest = br#"{"formatVersion":2,"files":[{"path":"snapshot/settings.json"}]}"#;
        let binary: Vec<u8> = (0..=255u8).collect();
        write_file(&source.join("settings.json"), br#"{"theme":"dark"}"#);
        write_file(&source.join("AGENTS.md"), "行尾不补换行".as_bytes());
        write_file(&source.join("skills/binary.bin"), &binary);
        write_file(&source.join("skills/antd/SKILL.md"), b"# antd\n");

        let archive = temp.path().join("20261122-105230-deadbeef.zip");
        pack(&archive, manifest, &source).expect("打包");

        let dest = temp.path().join("out");
        let report = unpack(&archive, &dest).expect("解包");

        assert_eq!(report.files, 5, "4 个快照文件 + manifest");
        assert_eq!(
            fs::read(dest.join("manifest.json")).expect("读 manifest"),
            manifest,
            "manifest 原样写入，不重新序列化"
        );
        assert_eq!(
            relative_files(&dest),
            vec![
                "manifest.json".to_string(),
                "snapshot/AGENTS.md".to_string(),
                "snapshot/settings.json".to_string(),
                "snapshot/skills/antd/SKILL.md".to_string(),
                "snapshot/skills/binary.bin".to_string(),
            ]
        );
        for relative in [
            "snapshot/settings.json",
            "snapshot/AGENTS.md",
            "snapshot/skills/binary.bin",
            "snapshot/skills/antd/SKILL.md",
        ] {
            assert_eq!(
                fs::read(dest.join(relative)).expect("读解包文件"),
                fs::read(source.join(relative.trim_start_matches("snapshot/"))).expect("读源文件"),
                "{relative} 必须逐字节一致"
            );
        }
        // 体积累计要覆盖 manifest 与全部快照文件。
        let expected_bytes = manifest.len() as u64
            + fs::metadata(source.join("settings.json"))
                .expect("stat")
                .len()
            + fs::metadata(source.join("AGENTS.md")).expect("stat").len()
            + fs::metadata(source.join("skills/binary.bin"))
                .expect("stat")
                .len()
            + fs::metadata(source.join("skills/antd/SKILL.md"))
                .expect("stat")
                .len();
        assert_eq!(report.bytes, expected_bytes);
    }

    #[test]
    fn read_manifest_returns_only_that_entry() {
        let temp = TempDir::new("read-manifest");
        let source = temp.path().join("snapshot");
        let manifest = br#"{"formatVersion":2,"files":[]}"#;
        write_file(&source.join("settings.json"), br#"{"theme":"dark"}"#);
        let archive = temp.path().join("20261122-105230-deadbeef.zip");
        pack(&archive, manifest, &source).expect("打包");

        assert_eq!(read_manifest(&archive).expect("读 manifest"), manifest);
    }

    #[test]
    fn read_manifest_rejects_package_without_manifest() {
        let temp = TempDir::new("read-manifest-missing");
        let archive = temp.path().join("package.zip");
        build_zip(&archive, &[("snapshot/a.json", b"x")]);

        let error = read_manifest(&archive).expect_err("缺 manifest 必须拒绝");
        assert_eq!(error.code, "package_corrupt", "{}", error.message);
    }

    #[test]
    fn read_manifest_rejects_non_zip() {
        let temp = TempDir::new("read-manifest-garbage");
        let archive = temp.path().join("package.zip");
        write_file(&archive, b"not a zip at all");

        let error = read_manifest(&archive).expect_err("非 zip 必须拒绝");
        assert_eq!(error.code, "package_corrupt", "{}", error.message);
    }

    #[test]
    fn empty_directories_are_not_packed() {
        let temp = TempDir::new("emptydir");
        let source = temp.path().join("snapshot");
        fs::create_dir_all(source.join("skills/empty")).expect("建空目录");
        write_file(&source.join("settings.json"), b"{}");
        let archive = temp.path().join("package.zip");
        pack(&archive, b"{}", &source).expect("打包");

        let dest = temp.path().join("out");
        let report = unpack(&archive, &dest).expect("解包");
        assert_eq!(report.files, 2);
        assert!(!dest.join("skills/empty").exists());
    }

    fn reject_entries(entries: &[(&str, &[u8])], expected: &str) {
        let temp = TempDir::new("reject");
        let archive = temp.path().join("package.zip");
        build_zip(&archive, entries);
        let dest = temp.path().join("out");
        let error = unpack(&archive, &dest).expect_err("必须拒绝");
        assert_eq!(error.code, expected, "{}", error.message);
        assert!(!dest.exists(), "失败后不留解包目录");
    }

    #[test]
    fn rejects_parent_directory_entry() {
        reject_entries(
            &[("manifest.json", b"{}"), ("../evil", b"x")],
            "package_unsafe_entry",
        );
    }

    #[test]
    fn rejects_parent_directory_below_snapshot() {
        reject_entries(
            &[("manifest.json", b"{}"), ("snapshot/../../evil", b"x")],
            "package_unsafe_entry",
        );
    }

    #[test]
    fn rejects_absolute_path_entry() {
        reject_entries(
            &[("manifest.json", b"{}"), ("/etc/passwd", b"x")],
            "package_unsafe_entry",
        );
    }

    #[test]
    fn rejects_drive_letter_entry() {
        reject_entries(
            &[("manifest.json", b"{}"), ("C:\\x", b"x")],
            "package_unsafe_entry",
        );
    }

    #[test]
    fn rejects_backslash_entry() {
        reject_entries(
            &[("manifest.json", b"{}"), (r"snapshot\a", b"x")],
            "package_unsafe_entry",
        );
    }

    #[test]
    fn rejects_empty_segment_entry() {
        reject_entries(
            &[("manifest.json", b"{}"), ("snapshot//a", b"x")],
            "package_unsafe_entry",
        );
    }

    #[test]
    fn rejects_entry_outside_whitelist() {
        reject_entries(
            &[("manifest.json", b"{}"), ("evil/a.json", b"x")],
            "package_corrupt",
        );
    }

    #[test]
    fn rejects_package_without_manifest() {
        reject_entries(&[("snapshot/a.json", b"x")], "package_corrupt");
    }

    #[test]
    fn unpacked_directories_and_files_are_owner_only() {
        let temp = TempDir::new("modes");
        let archive = temp.path().join("package.zip");
        build_zip(
            &archive,
            &[
                ("manifest.json", b"{}"),
                ("snapshot/skills/antd/SKILL.md", b"# antd\n"),
            ],
        );
        let dest = temp.path().join("staging").join("out");
        unpack(&archive, &dest).expect("解包");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [dest.clone(), dest.join("snapshot/skills")] {
                let mode = fs::metadata(&dir).expect("stat").permissions().mode();
                assert_eq!(mode & 0o777, 0o700, "{} 应只有本用户可进入", dir.display());
            }
            let mode = fs::metadata(dest.join("snapshot/skills/antd/SKILL.md"))
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "解包文件应只有本用户可读写");
        }
    }

    #[test]
    fn refuses_to_unpack_onto_an_existing_directory() {
        let temp = TempDir::new("occupied");
        let archive = temp.path().join("package.zip");
        build_zip(&archive, &[("manifest.json", b"{}")]);
        let dest = temp.path().join("out");
        fs::create_dir_all(dest.join("keep")).expect("建已存在的暂存目录");
        write_file(&dest.join("keep/previous.json"), b"{}");

        let error = unpack(&archive, &dest).expect_err("已存在的目录必须拒绝");
        assert_eq!(error.code, "package_corrupt", "{}", error.message);
        assert!(
            dest.join("keep/previous.json").exists(),
            "拒绝时不得删除已存在目录的内容"
        );
    }

    #[test]
    fn rejects_symlink_entry() {
        let temp = TempDir::new("symlink");
        let archive = temp.path().join("package.zip");
        let file = File::create(&archive).expect("创建测试包");
        let mut writer = ZipWriter::new(file);
        writer
            .start_file("manifest.json", SimpleFileOptions::default())
            .expect("写入 manifest");
        writer.write_all(b"{}").expect("写入 manifest");
        writer
            .add_symlink("snapshot/link", "/etc/passwd", SimpleFileOptions::default())
            .expect("写入链接条目");
        writer.finish().expect("完成测试包");

        let dest = temp.path().join("out");
        let error = unpack(&archive, &dest).expect_err("必须拒绝符号链接条目");
        assert_eq!(error.code, "package_corrupt");
        assert!(!dest.exists());
    }

    #[test]
    fn rejects_special_file_entry() {
        let temp = TempDir::new("fifo");
        let archive = temp.path().join("package.zip");
        let file = File::create(&archive).expect("创建测试包");
        let mut writer = ZipWriter::new(file);
        writer
            .start_file("manifest.json", SimpleFileOptions::default())
            .expect("写入 manifest");
        writer.write_all(b"{}").expect("写入 manifest");
        // S_IFIFO：既不是普通文件也不是目录。类型位只在 Unix 系统标识下被读取。
        writer
            .start_file(
                "snapshot/fifo",
                SimpleFileOptions::default()
                    .system(zip::System::Unix)
                    .external_attributes(0o010644 << 16),
            )
            .expect("写入 FIFO 条目");
        writer.write_all(b"x").expect("写入 FIFO 内容");
        writer.finish().expect("完成测试包");

        let dest = temp.path().join("out");
        let error = unpack(&archive, &dest).expect_err("必须拒绝特殊类型条目");
        assert_eq!(error.code, "package_corrupt");
    }

    #[test]
    fn rejects_too_many_entries() {
        let temp = TempDir::new("count");
        let archive = temp.path().join("package.zip");
        build_zip(
            &archive,
            &[
                ("manifest.json", b"{}"),
                ("snapshot/a.json", b"a"),
                ("snapshot/b.json", b"b"),
            ],
        );
        let limits = Limits {
            entries: 2,
            ..Limits::default()
        };
        let dest = temp.path().join("out");
        let error = unpack_with(&archive, &dest, limits).expect_err("条目数超限必须拒绝");
        assert_eq!(error.code, "package_unsafe_entry");
        assert!(!dest.exists());
    }

    #[test]
    fn rejects_too_much_unpacked_volume() {
        let temp = TempDir::new("volume");
        let archive = temp.path().join("package.zip");
        let big = vec![b'x'; 4096];
        build_zip(
            &archive,
            &[("manifest.json", b"{}"), ("snapshot/big", &big)],
        );
        let limits = Limits {
            unpacked_bytes: 1024,
            ..Limits::default()
        };
        let dest = temp.path().join("out");
        let error = unpack_with(&archive, &dest, limits).expect_err("解压体积超限必须拒绝");
        assert_eq!(error.code, "package_unsafe_entry");
        assert!(!dest.exists(), "失败后不留解包目录");
    }

    #[test]
    fn rejects_too_large_package() {
        let temp = TempDir::new("size");
        let archive = temp.path().join("package.zip");
        build_zip(&archive, &[("manifest.json", b"{}")]);
        let limits = Limits {
            package_bytes: 16,
            ..Limits::default()
        };
        let dest = temp.path().join("out");
        let error = unpack_with(&archive, &dest, limits).expect_err("包体积超限必须拒绝");
        assert_eq!(error.code, "package_too_large");
        assert!(!dest.exists());
    }

    #[test]
    fn pack_rejects_symlink_in_snapshot() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let temp = TempDir::new("link");
            let source = temp.path().join("snapshot");
            write_file(&source.join("settings.json"), b"{}");
            symlink("/etc/passwd", source.join("leak.json")).expect("建符号链接");
            let archive = temp.path().join("package.zip");
            let error = pack(&archive, b"{}", &source).expect_err("符号链接必须报错");
            assert_eq!(error.code, "export_failed");
            assert!(!archive.exists(), "失败的包不留在 dest 上");
        }
    }

    #[test]
    fn build_name_uses_utc_and_lowercase_fingerprint() {
        assert_eq!(
            build_name(1_795_344_750, "A1B2C3D4E5F6"),
            "20261122-105230-a1b2c3d4.zip"
        );
        assert_eq!(
            build_name(0, "0123456789ab"),
            "19700101-000000-01234567.zip"
        );
    }

    #[test]
    fn parse_name_reads_back_the_timestamp() {
        let parsed = parse_name("20261122-105230-deadbeef.zip").expect("合法包名");
        assert_eq!(parsed.stamp, "20261122-105230");
        assert_eq!(parsed.fingerprint8, "deadbeef");
        assert_eq!(parsed.unix_seconds, 1_795_344_750);
        assert_eq!(
            snapshot::format_timestamp(parsed.unix_seconds),
            "2026-11-22T10:52:30Z"
        );
        let generated = build_name(parsed.unix_seconds, &parsed.fingerprint8);
        assert_eq!(parse_name(&generated), Some(parsed));
    }

    #[test]
    fn parse_name_rejects_every_other_shape() {
        for name in [
            "20261301-000000-deadbeef.zip",
            "20261122-105230-DEADBEEF.zip",
            "20261122-105230-deadbeef.ZIP",
            "20261122-10523-deadbeef.zip",
            "manifest.json",
            ".tmp-1-abc",
            "20261122-105230-deadbeef (冲突).zip",
            "20260230-000000-deadbeef.zip",
            "20261122-240000-deadbeef.zip",
            "20261122-105230-deadbee.zip",
            "19691122-105230-deadbeef.zip",
        ] {
            assert_eq!(parse_name(name), None, "{name} 必须拒绝");
        }
    }
}

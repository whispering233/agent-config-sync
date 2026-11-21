//! 文件原语的平台差异：POSIX 权限位在 Windows 上不存在，链接的判定、创建与删除语义也不同。
//!
//! 只放文件操作原语：业务判断（哪些路径受管、失败码怎么报）留在调用方，
//! 平台差异用编译期分段的并列实现，不留空实现占位。

use std::fs;
use std::io;
use std::path::Path;

/// 读权限位的低 9 位。Windows 没有 POSIX 权限位，返回 `None`：
/// 备份与还原清单因此记 `null`，还原时跳过权限还原。
#[cfg(unix)]
pub(crate) fn mode_of(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
pub(crate) fn mode_of(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

/// 设置权限位。Windows 上没有对应语义，不设权限：宿主文件与凭据的保护由
/// 用户配置目录的默认 ACL 承担，硬改属性反而会破坏用户设置。
#[cfg(unix)]
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub(crate) fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// 递归建目录（已存在不报错），Unix 下创建时即带 `mode`，不经过 umask。
#[cfg(unix)]
pub(crate) fn create_dir_all_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)
}

#[cfg(not(unix))]
pub(crate) fn create_dir_all_mode(path: &Path, _mode: u32) -> io::Result<()> {
    fs::create_dir_all(path)
}

/// 这条路径是不是「链接类」。Windows 上 junction 未必被 `FileType::is_symlink()` 认出来，
/// 但 `read_link` 读得到目标；普通文件与目录读不到目标，所以不会误判。
pub(crate) fn is_link_like(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.file_type().is_symlink() || fs::read_link(path).is_ok(),
        Err(_) => false,
    }
}

/// 删掉链接本身，不跟进去。Unix 上任何链接都走 `remove_file`；
/// Windows 上目录型链接（junction、目录符号链接）只能按目录删，否则报拒绝访问。
#[cfg(not(windows))]
pub(crate) fn remove_link(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

#[cfg(windows)]
pub(crate) fn remove_link(path: &Path) -> io::Result<()> {
    // 跟得过去就看目标类型；断链跟不过去，只能看链接自身的属性。
    let directory_link = match fs::metadata(path) {
        Ok(target) => target.is_dir(),
        Err(_) => fs::symlink_metadata(path).is_ok_and(|link| link.is_dir()),
    };
    if directory_link {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// 建一条链接。Windows 必须按目标类型分开调，`is_dir` 由调用方按目标判定；
/// 默认权限下建链接会失败（`ERROR_PRIVILEGE_NOT_HELD`），由调用方退回物化写入。
#[cfg(unix)]
pub(crate) fn create_link(target: &str, path: &Path, _is_dir: bool) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(windows)]
pub(crate) fn create_link(target: &str, path: &Path, is_dir: bool) -> io::Result<()> {
    if is_dir {
        std::os::windows::fs::symlink_dir(target, path)
    } else {
        std::os::windows::fs::symlink_file(target, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 临时目录夹具：Drop 时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-config-sync-platform-{}-{}-{}",
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

    #[test]
    fn ordinary_files_and_directories_are_not_links() {
        let temp = TempDir::new("plain");
        fs::write(temp.path().join("file.md"), "内容").expect("写文件");
        fs::create_dir_all(temp.path().join("tree/nested")).expect("建目录");

        assert!(!is_link_like(&temp.path().join("file.md")));
        assert!(!is_link_like(&temp.path().join("tree")));
        assert!(!is_link_like(&temp.path().join("tree/nested")));
        assert!(!is_link_like(&temp.path().join("missing")));
    }

    #[cfg(unix)]
    #[test]
    fn links_are_links_even_when_the_target_is_gone() {
        let temp = TempDir::new("link");
        fs::create_dir_all(temp.path().join("tree")).expect("建目录");
        std::os::unix::fs::symlink("tree", temp.path().join("tree-link")).expect("建目录链接");
        std::os::unix::fs::symlink("nowhere", temp.path().join("broken")).expect("建断链");

        assert!(is_link_like(&temp.path().join("tree-link")));
        assert!(is_link_like(&temp.path().join("broken")));
    }

    #[cfg(unix)]
    #[test]
    fn removing_a_link_leaves_its_target_alone() {
        let temp = TempDir::new("remove-link");
        fs::create_dir_all(temp.path().join("tree")).expect("建目录");
        fs::write(temp.path().join("tree/inside.md"), "内容").expect("写文件");
        std::os::unix::fs::symlink("tree", temp.path().join("tree-link")).expect("建目录链接");

        remove_link(&temp.path().join("tree-link")).expect("删链接");

        assert!(!temp.path().join("tree-link").exists());
        assert!(temp.path().join("tree/inside.md").is_file(), "目标不被删");
    }
}

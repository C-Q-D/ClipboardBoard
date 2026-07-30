//! 此模块用 Windows 目录句柄固定图片存储目录身份，并验证重解析点、最终路径与卷边界。
//!
//! 句柄故意不共享 `DELETE`：capability 存活期间，受管顶层目录不能被静默替换。

use std::{
    fs::{File, OpenOptions},
    io,
    mem::MaybeUninit,
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
};

use windows_sys::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        GetFileInformationByHandle, GetFinalPathNameByHandleW, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_NAME_NORMALIZED, FILE_SHARE_READ, FILE_SHARE_WRITE, VOLUME_NAME_DOS,
    },
};

use super::{ImageStoragePrepareError, ImageStoragePrepareErrorKind};

/// 可写入 marker 的稳定目录创建指纹。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DirectoryFingerprint {
    /// 卷序列号用于阻止恢复目录跨卷。
    pub volume_serial: u32,
    /// 卷内文件身份；目录移动后保持不变。
    pub file_index: u64,
    /// 创建时间补强文件身份复用边界。
    pub creation_time: u64,
}

/// 单个目录的不可克隆持有句柄和已验证身份。
#[derive(Debug)]
pub(super) struct HeldDirectory {
    /// 不共享删除权限的目录句柄。
    _file: File,
    /// 由句柄取得的规范最终路径。
    pub canonical_path: PathBuf,
    /// 由同一句柄取得的稳定指纹。
    pub fingerprint: DirectoryFingerprint,
}

/// 图片存储完整 Windows capability；字段不公开，调用方只能整体借用或持有。
#[derive(Debug)]
pub(super) struct WindowsStorageGuard {
    /// 资产根句柄。
    pub asset_root: HeldDirectory,
    /// 原图目录句柄。
    _original: HeldDirectory,
    /// 缩略图目录句柄。
    _thumbnail: HeldDirectory,
    /// 临时发布目录句柄。
    _staging: HeldDirectory,
    /// 根外恢复基目录句柄。
    _recovery_base: HeldDirectory,
}

/// 单次发布期间固定原图与缩略图哈希分片目录身份的不可克隆 capability。
#[derive(Debug)]
pub(super) struct PublishShardGuard {
    /// 原图分片目录句柄。
    _original_shard: HeldDirectory,
    /// 缩略图分片目录句柄。
    _thumbnail_shard: HeldDirectory,
}

impl WindowsStorageGuard {
    /// 打开全部受管目录，并校验子树边界和恢复目录同卷约束。
    pub fn open(
        asset_root: &Path,
        original: &Path,
        thumbnail: &Path,
        staging: &Path,
        recovery_base: &Path,
    ) -> Result<Self, ImageStoragePrepareError> {
        let asset_root = HeldDirectory::open(asset_root, "打开图片资产根")?;
        let original = HeldDirectory::open(original, "打开原图目录")?;
        let thumbnail = HeldDirectory::open(thumbnail, "打开缩略图目录")?;
        let staging = HeldDirectory::open(staging, "打开临时发布目录")?;
        let recovery_base = HeldDirectory::open(recovery_base, "打开恢复基目录")?;

        for (label, child) in [
            ("原图目录越出图片资产根", &original),
            ("缩略图目录越出图片资产根", &thumbnail),
            ("临时发布目录越出图片资产根", &staging),
        ] {
            let Some(parent) = child.canonical_path.parent() else {
                return Err(ImageStoragePrepareError::new(
                    ImageStoragePrepareErrorKind::UnsafePath,
                    label,
                ));
            };
            if !path_eq(parent, &asset_root.canonical_path) {
                return Err(ImageStoragePrepareError::new(
                    ImageStoragePrepareErrorKind::UnsafePath,
                    label,
                ));
            }
        }
        if recovery_base.fingerprint.volume_serial != asset_root.fingerprint.volume_serial {
            return Err(ImageStoragePrepareError::new(
                ImageStoragePrepareErrorKind::CrossVolumeRecovery,
                "恢复基目录与图片资产根不在同一卷",
            ));
        }

        Ok(Self {
            asset_root,
            _original: original,
            _thumbnail: thumbnail,
            _staging: staging,
            _recovery_base: recovery_base,
        })
    }

    /// 打开并固定两个哈希分片目录，拒绝重解析点及越出对应固定子树的路径。
    pub fn hold_publish_shards(
        &self,
        original_shard: &Path,
        thumbnail_shard: &Path,
    ) -> Result<PublishShardGuard, ImageStoragePrepareError> {
        let original_shard = HeldDirectory::open(original_shard, "打开原图分片目录")?;
        let thumbnail_shard = HeldDirectory::open(thumbnail_shard, "打开缩略图分片目录")?;
        for (label, shard, expected_parent) in [
            ("原图分片目录越出固定子树", &original_shard, &self._original),
            (
                "缩略图分片目录越出固定子树",
                &thumbnail_shard,
                &self._thumbnail,
            ),
        ] {
            let Some(parent) = shard.canonical_path.parent() else {
                return Err(ImageStoragePrepareError::new(
                    ImageStoragePrepareErrorKind::UnsafePath,
                    label,
                ));
            };
            if !path_eq(parent, &expected_parent.canonical_path) {
                return Err(ImageStoragePrepareError::new(
                    ImageStoragePrepareErrorKind::UnsafePath,
                    label,
                ));
            }
        }
        Ok(PublishShardGuard {
            _original_shard: original_shard,
            _thumbnail_shard: thumbnail_shard,
        })
    }
}

impl HeldDirectory {
    /// 用不共享 DELETE 的句柄打开目录，并从同一句柄读取最终路径和文件身份。
    pub fn open(path: &Path, operation: &'static str) -> Result<Self, ImageStoragePrepareError> {
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|error| ImageStoragePrepareError::from_io(operation, error))?;
        let metadata = file
            .metadata()
            .map_err(|error| ImageStoragePrepareError::from_io(operation, error))?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(ImageStoragePrepareError::new(
                ImageStoragePrepareErrorKind::ReparsePoint,
                operation,
            ));
        }

        let handle = file.as_raw_handle() as HANDLE;
        let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        // SAFETY: `handle` 来自仍存活的 File，输出指针指向完整且可写的结构体。
        if unsafe { GetFileInformationByHandle(handle, information.as_mut_ptr()) } == 0 {
            return Err(ImageStoragePrepareError::from_io(
                operation,
                io::Error::last_os_error(),
            ));
        }
        // SAFETY: Win32 调用成功后已完整初始化输出结构。
        let information = unsafe { information.assume_init() };
        let canonical_path = final_path(handle, operation)?;
        let expected = std::fs::canonicalize(path)
            .map_err(|error| ImageStoragePrepareError::from_io(operation, error))?;
        if !path_eq(&canonical_path, &expected) {
            return Err(ImageStoragePrepareError::new(
                ImageStoragePrepareErrorKind::UnsafePath,
                operation,
            ));
        }

        Ok(Self {
            _file: file,
            canonical_path,
            fingerprint: DirectoryFingerprint {
                volume_serial: information.dwVolumeSerialNumber,
                file_index: (u64::from(information.nFileIndexHigh) << 32)
                    | u64::from(information.nFileIndexLow),
                creation_time: (u64::from(information.ftCreationTime.dwHighDateTime) << 32)
                    | u64::from(information.ftCreationTime.dwLowDateTime),
            },
        })
    }
}

/// 从目录句柄读取 DOS 形式规范路径，并移除 Win32 扩展路径前缀以便与 Rust 路径比较。
fn final_path(
    handle: HANDLE,
    operation: &'static str,
) -> Result<PathBuf, ImageStoragePrepareError> {
    let flags = FILE_NAME_NORMALIZED | VOLUME_NAME_DOS;
    // 先获取所需 UTF-16 长度；返回值包含最终路径所需空间。
    let required = unsafe { GetFinalPathNameByHandleW(handle, std::ptr::null_mut(), 0, flags) };
    if required == 0 {
        return Err(ImageStoragePrepareError::from_io(
            operation,
            io::Error::last_os_error(),
        ));
    }
    let mut buffer = vec![0_u16; required as usize + 1];
    // SAFETY: buffer 长度按上一步结果分配，句柄在整个调用期间保持有效。
    let written = unsafe {
        GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, flags)
    };
    if written == 0 || written as usize >= buffer.len() {
        return Err(ImageStoragePrepareError::from_io(
            operation,
            io::Error::last_os_error(),
        ));
    }
    let value = String::from_utf16(&buffer[..written as usize]).map_err(|_| {
        ImageStoragePrepareError::new(ImageStoragePrepareErrorKind::UnsafePath, operation)
    })?;
    Ok(PathBuf::from(normalize_windows_path_text(&value)))
}

/// Windows 路径比较忽略大小写，但不放宽组件边界。
fn path_eq(left: &Path, right: &Path) -> bool {
    normalized_path_text(left).eq_ignore_ascii_case(&normalized_path_text(right))
}

/// 仅移除 Win32 扩展路径表示差异，不改变真实路径组件。
fn normalized_path_text(path: &Path) -> String {
    let value = path.as_os_str().to_string_lossy();
    normalize_windows_path_text(&value)
        .trim_end_matches(['\\', '/'])
        .to_owned()
}

/// 将 Win32 扩展路径转换为普通 DOS/UNC 绝对路径表示。
fn normalize_windows_path_text(value: &str) -> String {
    if let Some(unc_path) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc_path}")
    } else {
        value.strip_prefix(r"\\?\").unwrap_or(value).to_owned()
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证 Win32 扩展 DOS 与 UNC 路径不会丢失绝对路径语义。

    use std::path::Path;

    use super::{normalize_windows_path_text, path_eq};

    /// 验证普通盘符扩展前缀可安全移除。
    #[test]
    fn normalizes_extended_drive_path() {
        assert_eq!(
            normalize_windows_path_text(r"\\?\C:\ClipboardBoard\images"),
            r"C:\ClipboardBoard\images"
        );
        assert!(path_eq(
            Path::new(r"\\?\C:\ClipboardBoard\images"),
            Path::new(r"C:\ClipboardBoard\images")
        ));
    }

    /// 验证扩展 UNC 路径恢复双反斜杠，结果仍是绝对路径。
    #[test]
    fn normalizes_extended_unc_path() {
        let normalized = normalize_windows_path_text(r"\\?\UNC\server\share\images");
        assert_eq!(normalized, r"\\server\share\images");
        assert!(Path::new(&normalized).is_absolute());
    }
}

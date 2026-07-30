//! 此文件在 Windows 上封装 ReplaceFileW/MoveFileExW，并保留每类错误的确定后置状态。

use std::{io, os::windows::ffi::OsStrExt, path::Path, ptr};

use windows_sys::Win32::{
    Foundation::{
        ERROR_UNABLE_TO_MOVE_REPLACEMENT, ERROR_UNABLE_TO_MOVE_REPLACEMENT_2,
        ERROR_UNABLE_TO_REMOVE_REPLACED,
    },
    Storage::FileSystem::{
        MoveFileExW, ReplaceFileW, MOVEFILE_WRITE_THROUGH, REPLACEFILE_WRITE_THROUGH,
    },
};

/// ReplaceFileW 失败后的文档后置状态分类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReplaceFailureKind {
    /// 1175：主文件和 staging 保留原名。
    UnableToRemoveReplaced,
    /// 1176 且提供 backup：主文件和 staging 保留原名。
    UnableToMoveReplacementWithBackup,
    /// 1176 且不提供 backup：主文件不存在，staging 保留原名。
    UnableToMoveReplacementWithoutBackup,
    /// 1177：staging 保留原名且继承 streams/attributes，旧主文件被移走。
    UnableToMoveReplacement2,
    /// 其他 Win32 错误：主/staging 原名、backup 不存在，streams/attributes 不保证。
    OtherDocumented,
    /// 无法取得可信错误码时不能推断任何后置状态。
    UnknownPostState,
}

/// ReplaceFileW 失败同时保留分类与原始系统错误。
pub(super) struct ReplaceFailure {
    /// 精确后置状态类别。
    pub kind: ReplaceFailureKind,
    /// GetLastError 对应的 IO 错误。
    pub error: io::Error,
}

/// 原子替换已有主文件；backup 为 None 时绝不改写既有恢复备份。
pub(super) fn replace(
    primary: &Path,
    staging: &Path,
    backup: Option<&Path>,
) -> Result<(), ReplaceFailure> {
    let primary = wide(primary);
    let staging = wide(staging);
    let backup_wide = backup.map(wide);
    let backup_pointer = backup_wide
        .as_ref()
        .map_or(ptr::null(), |value| value.as_ptr());
    // SAFETY: 三个 UTF-16 缓冲在调用期间存活且以 NUL 结尾；保留参数按 API 要求为空。
    let result = unsafe {
        ReplaceFileW(
            primary.as_ptr(),
            staging.as_ptr(),
            backup_pointer,
            REPLACEFILE_WRITE_THROUGH,
            ptr::null(),
            ptr::null(),
        )
    };
    if result != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    let code = error.raw_os_error().map(|value| value as u32);
    let kind = classify_replace_error(code, backup.is_some());
    Err(ReplaceFailure { kind, error })
}

/// 首次保存时把同目录 staging write-through 移动为主文件。
pub(super) fn move_new(staging: &Path, primary: &Path) -> Result<(), io::Error> {
    let staging = wide(staging);
    let primary = wide(primary);
    // SAFETY: 两个 UTF-16 缓冲在调用期间存活且以 NUL 结尾。
    if unsafe { MoveFileExW(staging.as_ptr(), primary.as_ptr(), MOVEFILE_WRITE_THROUGH) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// 按错误码和 backup 参数固定后置状态，不依赖错误文本。
fn classify_replace_error(code: Option<u32>, has_backup: bool) -> ReplaceFailureKind {
    match code {
        Some(ERROR_UNABLE_TO_REMOVE_REPLACED) => ReplaceFailureKind::UnableToRemoveReplaced,
        Some(ERROR_UNABLE_TO_MOVE_REPLACEMENT) if has_backup => {
            ReplaceFailureKind::UnableToMoveReplacementWithBackup
        }
        Some(ERROR_UNABLE_TO_MOVE_REPLACEMENT) => {
            ReplaceFailureKind::UnableToMoveReplacementWithoutBackup
        }
        Some(ERROR_UNABLE_TO_MOVE_REPLACEMENT_2) => ReplaceFailureKind::UnableToMoveReplacement2,
        Some(0) | None => ReplaceFailureKind::UnknownPostState,
        Some(_) => ReplaceFailureKind::OtherDocumented,
    }
}

/// 把 Windows 路径转换为 NUL 结尾 UTF-16。
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证所有 Win32 文档错误码都映射到精确后置状态。

    use super::{classify_replace_error, ReplaceFailureKind};
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_UNABLE_TO_MOVE_REPLACEMENT, ERROR_UNABLE_TO_MOVE_REPLACEMENT_2,
        ERROR_UNABLE_TO_REMOVE_REPLACED,
    };

    /// 验证 1175、1176、1177、其他错误和未知状态分类。
    #[test]
    fn classifies_documented_and_unknown_replace_states() {
        assert_eq!(
            classify_replace_error(Some(ERROR_UNABLE_TO_REMOVE_REPLACED), true),
            ReplaceFailureKind::UnableToRemoveReplaced
        );
        assert_eq!(
            classify_replace_error(Some(ERROR_UNABLE_TO_MOVE_REPLACEMENT), true),
            ReplaceFailureKind::UnableToMoveReplacementWithBackup
        );
        assert_eq!(
            classify_replace_error(Some(ERROR_UNABLE_TO_MOVE_REPLACEMENT), false),
            ReplaceFailureKind::UnableToMoveReplacementWithoutBackup
        );
        assert_eq!(
            classify_replace_error(Some(ERROR_UNABLE_TO_MOVE_REPLACEMENT_2), true),
            ReplaceFailureKind::UnableToMoveReplacement2
        );
        assert_eq!(
            classify_replace_error(Some(ERROR_ACCESS_DENIED), true),
            ReplaceFailureKind::OtherDocumented
        );
        assert_eq!(
            classify_replace_error(None, false),
            ReplaceFailureKind::UnknownPostState
        );
    }
}

//! 此模块读取剪贴板事件发生时的前台来源进程。
//!
//! 读取只使用 `PROCESS_QUERY_LIMITED_INFORMATION` 和进程映像路径，不访问窗口标题、窗口
//! 内容或进程内存；查询失败统一返回 `None`，确保来源识别失败不会阻塞后续剪贴板捕获。

use std::ffi::OsString;
use std::fmt;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, HWND};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

/// 查询进程映像路径时使用的固定上限，Windows 长路径不会让缓冲区无限增长。
const PROCESS_IMAGE_PATH_CAPACITY: usize = 32_768;
/// 非 UTF-8 映像路径的内部标记；它只用于读取门禁，不进入历史或日志。
const UNSAFE_IMAGE_PATH_MARKER: &str = "\0";

/// 复制结果对外暴露的最小稳定来源；不包含窗口标题、路径目录或任意正文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSource {
    /// 可执行文件名，例如 `chrome.exe`；目录部分在读取后立即丢弃。
    pub executable: String,
    /// 从文件名派生的显示名称，例如 `chrome`；不读取版本资源或窗口标题。
    pub display_name: String,
    /// 捕获来源时的进程 ID，供后续事件关联和安全复核使用。
    pub process_id: u32,
}

/// 仅供 ClipboardIO 读取前排除判断使用的来源快照。
///
/// `image_path` 只会随请求进入 worker，不会进入捕获结果、历史 DTO 或 UI；自定义
/// `Debug` 也只输出是否存在路径，避免诊断日志泄露本机目录。
#[derive(Clone, Eq, PartialEq)]
pub struct ProcessSourceSnapshot {
    /// 对外来源字段的拥有型副本。
    pub source: ProcessSource,
    /// 受限权限查询得到的完整映像路径；非 UTF-8 时保存内部不安全标记，不进入结果/UI。
    pub image_path: Option<String>,
}

impl fmt::Debug for ProcessSourceSnapshot {
    /// 只输出 exe、PID 和路径存在性，不输出显示名或路径正文。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessSourceSnapshot")
            .field("executable_present", &(!self.source.executable.is_empty()))
            .field("process_id", &self.source.process_id)
            .field("image_path_present", &self.image_path.is_some())
            .finish()
    }
}

impl From<ProcessSource> for ProcessSourceSnapshot {
    /// 兼容没有完整路径的测试/调用方来源。
    fn from(source: ProcessSource) -> Self {
        Self {
            source,
            image_path: None,
        }
    }
}

impl ProcessSourceSnapshot {
    /// 返回不携带完整路径的结果来源。
    pub fn result_source(&self) -> ProcessSource {
        self.source.clone()
    }

    /// 返回该来源是否可安全用于规则匹配；非 UTF-8 路径必须在读取前 fail-closed。
    pub(crate) fn is_safe_for_rules(&self) -> bool {
        self.image_path.as_deref() != Some(UNSAFE_IMAGE_PATH_MARKER)
    }
}

/// 读取当前前台窗口对应的来源进程；无窗口、PID 无效或权限不足时返回无来源。
pub fn capture_foreground_source() -> Option<ProcessSource> {
    capture_foreground_source_snapshot().map(|snapshot| snapshot.result_source())
}

/// 捕获带有限完整映像路径的请求级来源快照；查询失败时返回无来源。
pub fn capture_foreground_source_snapshot() -> Option<ProcessSourceSnapshot> {
    let window = unsafe { GetForegroundWindow() };
    source_snapshot_from_hwnd(window)
}

/// 根据窗口句柄读取所属 PID，再使用有限权限查询进程映像名称。
#[cfg(test)]
fn source_from_hwnd(window: HWND) -> Option<ProcessSource> {
    source_snapshot_from_hwnd(window).map(|snapshot| snapshot.result_source())
}

/// 根据窗口句柄读取请求级来源快照。
fn source_snapshot_from_hwnd(window: HWND) -> Option<ProcessSourceSnapshot> {
    if window.is_null() {
        return None;
    }

    let mut process_id = 0_u32;
    unsafe {
        GetWindowThreadProcessId(window, &mut process_id);
    }
    source_snapshot_from_process_id(process_id)
}

/// 以最小查询权限打开进程并在关闭句柄前复制出稳定的 Rust 字符串。
#[cfg(test)]
fn source_from_process_id(process_id: u32) -> Option<ProcessSource> {
    source_snapshot_from_process_id(process_id).map(|snapshot| snapshot.result_source())
}

/// 以最小查询权限读取来源文件名及可选完整路径。
fn source_snapshot_from_process_id(process_id: u32) -> Option<ProcessSourceSnapshot> {
    if process_id == 0 {
        return None;
    }

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return None;
    }

    let source =
        query_process_image_name(process).and_then(|path| source_snapshot_from_image_path(&path));
    unsafe {
        // 句柄由本函数创建且只在查询期间使用；无论查询成功与否都必须关闭。
        let _ = CloseHandle(process);
    }
    source.map(|mut snapshot| {
        snapshot.source.process_id = process_id;
        snapshot
    })
}

/// 读取进程完整映像路径，并把 UTF-16 数据复制为拥有型 `OsString`。
fn query_process_image_name(process: windows_sys::Win32::Foundation::HANDLE) -> Option<OsString> {
    let mut buffer = vec![0_u16; PROCESS_IMAGE_PATH_CAPACITY];
    let mut length = buffer.len() as u32;
    let success =
        unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) } != 0;
    if !success || length == 0 || length as usize > buffer.len() {
        return None;
    }

    Some(OsString::from_wide(&buffer[..length as usize]))
}

/// 从映像路径提取文件名和显示名；目录不进入来源快照以降低本地路径泄露风险。
#[cfg(test)]
fn source_from_image_path(path: &OsString) -> Option<ProcessSource> {
    source_snapshot_from_image_path(path).map(|snapshot| snapshot.result_source())
}

/// 从映像路径同时构造结果来源和请求级完整路径。
fn source_snapshot_from_image_path(path: &OsString) -> Option<ProcessSourceSnapshot> {
    let file_name_os = Path::new(path).file_name()?;
    let Some(file_name) = file_name_os.to_str().map(str::to_owned) else {
        // 无法安全转换的文件名不能让排除规则绕过；保留有界 lossy basename，
        // 并设置内部标记，使 RecordingGate 在正文读取前 fail-closed。
        let lossy_name = file_name_os.to_string_lossy().into_owned();
        if lossy_name.is_empty() {
            return None;
        }
        let display_name = Path::new(&lossy_name)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty())
            .unwrap_or(&lossy_name)
            .to_owned();
        return Some(ProcessSourceSnapshot {
            source: ProcessSource {
                executable: lossy_name,
                display_name,
                process_id: 0,
            },
            image_path: Some(UNSAFE_IMAGE_PATH_MARKER.to_owned()),
        });
    };
    if file_name.is_empty() {
        return None;
    }

    let display_name = Path::new(&file_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or(&file_name)
        .to_owned();

    Some(ProcessSourceSnapshot {
        source: ProcessSource {
            executable: file_name,
            display_name,
            process_id: 0,
        },
        image_path: path.to_str().map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    //! 此测试模块覆盖来源路径解析、无效窗口、访问失败和当前进程查询边界。

    use super::{
        source_from_hwnd, source_from_image_path, source_from_process_id,
        source_snapshot_from_image_path,
    };
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::ptr::null_mut;

    /// 只保留 exe 文件名和派生显示名，目录不应泄露到快照。
    #[test]
    fn 路径解析只保留文件名() {
        let path = OsString::from(r"C:\Program Files\ClipboardBoard\ClipboardBoard.exe");
        let source = source_from_image_path(&path).expect("有效映像路径应能解析");
        assert_eq!(source.executable, "ClipboardBoard.exe");
        assert_eq!(source.display_name, "ClipboardBoard");
        assert_eq!(source.process_id, 0);
    }

    /// 请求级来源快照的 Debug 只报告路径存在性，不回显目录或显示名。
    #[test]
    fn 请求来源快照_debug不泄漏完整路径() {
        let path = OsString::from(r"C:\Secret\PasswordManager.exe");
        let snapshot = source_snapshot_from_image_path(&path).expect("有效来源快照应能构造");
        let debug = format!("{snapshot:?}");
        assert!(debug.contains("image_path_present"));
        assert!(!debug.contains("Secret"));
        assert!(!debug.contains("PasswordManager"));
    }

    /// 非 UTF-8 文件名仍保留 basename，但必须标记为不安全并在正文门禁前拒绝。
    #[test]
    fn 非_utf8文件名保留basename并标记不安全() {
        let mut units = r"C:\Secret\".encode_utf16().collect::<Vec<_>>();
        units.extend([0xD800, b'.' as u16]);
        units.extend("exe".encode_utf16());
        let path = OsString::from_wide(&units);
        let snapshot = source_snapshot_from_image_path(&path).expect("应保留可诊断 basename");
        assert!(snapshot.source.executable.contains('\u{FFFD}'));
        assert!(!snapshot.is_safe_for_rules());
        assert!(snapshot.image_path.is_some());
    }

    /// 没有文件名或非 UTF-8 文件名时安全返回无来源。
    #[test]
    fn 无法解析文件名时返回_none() {
        assert!(source_from_image_path(&OsString::from(r"C:\")).is_none());
    }

    /// 空窗口句柄不能被当作当前来源，避免把查询失败伪装成系统进程。
    #[test]
    fn 空窗口句柄返回_none() {
        assert!(source_from_hwnd(null_mut()).is_none());
    }

    /// 无效 PID 或进程已退出时查询失败，但不向调用方抛出错误。
    #[test]
    fn 无效进程返回_none() {
        assert!(source_from_process_id(u32::MAX).is_none());
    }

    /// 当前进程应可用受限查询权限读取，证明正常程序路径可工作。
    #[test]
    fn 当前进程来源可读取() {
        let source = source_from_process_id(unsafe {
            windows_sys::Win32::System::Threading::GetCurrentProcessId()
        })
        .expect("当前进程应能查询映像名称");
        assert!(!source.executable.is_empty());
        assert!(!source.display_name.is_empty());
        assert!(source.process_id > 0);
    }
}

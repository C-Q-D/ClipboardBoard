//! 此文件实现临时看板的目标窗口快照和显示器工作区定位。
//!
//! 这里不执行粘贴，也不向其他进程注入输入；当前原子只在面板显示时保存 HWND、PID
//! 和完整性级别，后续自动粘贴原子必须重新查询并逐字段比较，不一致时只能降级为复制。

use std::mem::size_of;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{CloseHandle, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTOPRIMARY,
};
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetCursorPos, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId,
    SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
};

/// 目标进程的 Windows 完整性级别；查询失败时保留 Unknown，但仍保存 HWND 和 PID。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IntegrityLevel {
    /// 无法读取目标令牌，后续粘贴必须采取保守降级路径。
    Unknown,
    /// 低完整性进程，例如受沙箱限制的应用。
    Low,
    /// 普通桌面应用通常使用的中完整性级别。
    Medium,
    /// 以管理员权限运行的高完整性进程。
    High,
    /// 系统服务使用的系统完整性级别。
    System,
    /// 受保护进程使用的受保护完整性级别。
    Protected,
}

/// 呼出面板前的前台目标窗口身份快照。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PanelTarget {
    /// 目标窗口句柄；只在快照有效期内用于后续身份复核。
    pub(crate) hwnd: isize,
    /// 目标窗口所属进程 ID；PID 单独不足以证明进程未重启，后续原子仍需加强复核。
    pub(crate) process_id: u32,
    /// 目标进程创建时的完整性级别，用于阻止权限变化时误注入输入。
    pub(crate) integrity: IntegrityLevel,
}

/// 鼠标所在显示器扣除任务栏后的物理工作区。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkArea {
    /// 工作区左边界，可能是负数（左侧扩展显示器）。
    pub(crate) left: i32,
    /// 工作区上边界，可能是负数（上方扩展显示器）。
    pub(crate) top: i32,
    /// 工作区右边界（不含边界像素）。
    pub(crate) right: i32,
    /// 工作区下边界（不含边界像素）。
    pub(crate) bottom: i32,
}

/// 面板左上角的物理像素位置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PanelPosition {
    /// 窗口左边的屏幕坐标。
    pub(crate) x: i32,
    /// 窗口上边的屏幕坐标。
    pub(crate) y: i32,
}

/// 通过稳定的 Slint 窗口类名和标题找到当前面板 HWND。
///
/// 找不到窗口是正常情况（例如首次创建尚未完成），调用方必须把 `None` 当作
/// “不能确认自身窗口”处理，不能因此误保存自身或发送自动粘贴输入。
pub(crate) fn panel_hwnd() -> Option<isize> {
    let hwnd = find_panel_hwnd();

    (!hwnd.is_null()).then_some(hwnd as isize)
}

/// 通过 Win32 物理像素移动已创建的面板窗口，修正部分后端在首次显示时覆盖位置的问题。
pub(crate) fn move_panel(position: PanelPosition) -> bool {
    let hwnd = find_panel_hwnd();
    if hwnd.is_null() {
        return false;
    }

    unsafe {
        SetWindowPos(
            hwnd,
            null_mut(),
            position.x,
            position.y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        ) != 0
    }
}

/// 读取当前 Windows 外框的物理尺寸，避免用 Slint 内容尺寸计算时忽略边框和标题区。
pub(crate) fn panel_size() -> Option<(u32, u32)> {
    let hwnd = find_panel_hwnd();
    if hwnd.is_null() {
        return None;
    }

    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return None;
    }

    let width = i64::from(rect.right) - i64::from(rect.left);
    let height = i64::from(rect.bottom) - i64::from(rect.top);
    (width > 0 && height > 0).then_some((width as u32, height as u32))
}

/// 使用 Slint 当前桌面窗口的类名和标题查找 HWND，单实例协议保证不会误命中其他版本。
fn find_panel_hwnd() -> windows_sys::Win32::Foundation::HWND {
    unsafe {
        FindWindowExW(
            null_mut(),
            null_mut(),
            windows_sys::core::w!("Window Class"),
            windows_sys::core::w!("ClipboardBoard"),
        )
    }
}

/// 捕获当前前台窗口的 HWND、PID 和完整性级别；无效或自身窗口不会被保存。
pub(crate) fn capture_target(panel_window: Option<isize>) -> Option<PanelTarget> {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_null() {
        return None;
    }

    let foreground_value = foreground as isize;
    if panel_window == Some(foreground_value) {
        return None;
    }

    let mut process_id = 0_u32;
    unsafe {
        GetWindowThreadProcessId(foreground, &mut process_id);
    }
    if process_id == 0 {
        return None;
    }

    Some(PanelTarget {
        hwnd: foreground_value,
        process_id,
        integrity: query_integrity_level(process_id),
    })
}

/// 读取鼠标所在显示器的任务栏避让工作区；显示器查询失败时返回 `None`。
pub(crate) fn cursor_work_area() -> Option<WorkArea> {
    let mut cursor = POINT::default();
    if unsafe { GetCursorPos(&mut cursor) } != 0 {
        let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
        if let Some(area) = monitor_work_area(monitor) {
            return Some(area);
        }
    }

    // 非交互桌面、远程会话或光标查询失败时退回主显示器，保证面板仍能进入可见区域。
    let primary_monitor = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
    monitor_work_area(primary_monitor)
}

/// 读取单个 HMONITOR 的任务栏避让工作区；句柄无效时返回 None 供上层继续降级。
fn monitor_work_area(monitor: windows_sys::Win32::Graphics::Gdi::HMONITOR) -> Option<WorkArea> {
    if monitor.is_null() {
        return None;
    }

    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..MONITORINFO::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }

    Some(work_area_from_rect(info.rcWork))
}

/// 根据物理工作区和面板尺寸计算居中位置，并在面板大于工作区时保持左上角可见。
pub(crate) fn center_position(
    area: WorkArea,
    panel_width: u32,
    panel_height: u32,
) -> PanelPosition {
    let area_width = i64::from(area.right) - i64::from(area.left);
    let area_height = i64::from(area.bottom) - i64::from(area.top);
    let width = i64::from(panel_width);
    let height = i64::from(panel_height);

    PanelPosition {
        x: (i64::from(area.left) + ((area_width - width).max(0) / 2))
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        y: (i64::from(area.top) + ((area_height - height).max(0) / 2))
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
    }
}

/// 将 Win32 RECT 转成不依赖 API 布局命名的值对象。
fn work_area_from_rect(rect: RECT) -> WorkArea {
    WorkArea {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

/// 读取目标 PID 的令牌完整性级别；权限不足或令牌格式异常统一返回 Unknown。
fn query_integrity_level(process_id: u32) -> IntegrityLevel {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return IntegrityLevel::Unknown;
    }

    let mut token = null_mut();
    let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } != 0;
    let level = if opened {
        query_token_integrity_level(token)
    } else {
        IntegrityLevel::Unknown
    };

    unsafe {
        if opened {
            let _ = CloseHandle(token);
        }
        let _ = CloseHandle(process);
    }
    level
}

/// 从令牌的 Mandatory Label SID 读取最后一个 RID，并映射为业务枚举。
fn query_token_integrity_level(token: windows_sys::Win32::Foundation::HANDLE) -> IntegrityLevel {
    let mut required_length = 0_u32;
    unsafe {
        let _ = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            null_mut(),
            0,
            &mut required_length,
        );
    }
    if required_length < size_of::<TOKEN_MANDATORY_LABEL>() as u32 {
        return IntegrityLevel::Unknown;
    }

    let mut buffer = vec![0_u8; required_length as usize];
    let success = unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            buffer.as_mut_ptr().cast(),
            required_length,
            &mut required_length,
        )
    } != 0;
    if !success {
        return IntegrityLevel::Unknown;
    }

    let label = unsafe { &*(buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()) };
    let sid = label.Label.Sid;
    if sid.is_null() {
        return IntegrityLevel::Unknown;
    }
    // 令牌结构由 Windows 返回，异常或损坏的令牌可能让辅助函数返回空指针；
    // 这里宁可放弃完整性判断，也不能在安全边界上直接解引用未知地址。
    let count_ptr = unsafe { GetSidSubAuthorityCount(sid) };
    if count_ptr.is_null() {
        return IntegrityLevel::Unknown;
    }
    let count = unsafe { *count_ptr };
    if count == 0 {
        return IntegrityLevel::Unknown;
    }
    let rid_ptr = unsafe { GetSidSubAuthority(sid, u32::from(count) - 1) };
    if rid_ptr.is_null() {
        return IntegrityLevel::Unknown;
    }
    let rid = unsafe { *rid_ptr };
    integrity_from_rid(rid)
}

/// 将 Windows 标准完整性 RID 映射成有限枚举，未知区间保守归入低完整性。
fn integrity_from_rid(rid: u32) -> IntegrityLevel {
    match rid {
        0x5000..=u32::MAX => IntegrityLevel::Protected,
        0x4000..=0x4fff => IntegrityLevel::System,
        0x3000..=0x3fff => IntegrityLevel::High,
        0x2000..=0x2fff => IntegrityLevel::Medium,
        _ => IntegrityLevel::Low,
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块覆盖不依赖桌面状态的定位和权限映射边界。

    use super::{center_position, integrity_from_rid, IntegrityLevel, PanelPosition, WorkArea};

    /// 负坐标显示器也必须按工作区真实坐标居中，而不能错误地从零点计算。
    #[test]
    fn 负坐标工作区居中() {
        let area = WorkArea {
            left: -1920,
            top: -80,
            right: 0,
            bottom: 1000,
        };
        assert_eq!(
            center_position(area, 560, 640),
            PanelPosition { x: -1240, y: 140 }
        );
    }

    /// 面板大于工作区时位置应钳制在工作区左上角，避免完全落在屏幕外。
    #[test]
    fn 超大面板位置钳制() {
        let area = WorkArea {
            left: 100,
            top: 50,
            right: 300,
            bottom: 200,
        };
        assert_eq!(
            center_position(area, 560, 640),
            PanelPosition { x: 100, y: 50 }
        );
    }

    /// Windows 完整性 RID 的分段映射必须保持稳定，供后续权限降级使用。
    #[test]
    fn 完整性级别映射() {
        assert_eq!(integrity_from_rid(0x1000), IntegrityLevel::Low);
        assert_eq!(integrity_from_rid(0x2000), IntegrityLevel::Medium);
        assert_eq!(integrity_from_rid(0x3000), IntegrityLevel::High);
        assert_eq!(integrity_from_rid(0x4000), IntegrityLevel::System);
        assert_eq!(integrity_from_rid(0x5000), IntegrityLevel::Protected);
    }
}

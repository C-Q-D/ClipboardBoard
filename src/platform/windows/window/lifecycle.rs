//! 此文件实现临时看板的原生窗口查找、显示器工作区定位和 topmost 断言。
//!
//! 应用只用 Win32 修正 Slint 窗口的物理位置与尺寸，不保存外部目标窗口，也不向
//! 其他进程注入键盘输入。

// 资源 ID 由 build.rs 从仓库内受控文本生成；标题栏和托盘必须复用同一资源。
include!(concat!(env!("OUT_DIR"), "/clipboard_board_resources.rs"));

use std::mem::size_of;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTOPRIMARY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetActiveWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::HWND_TOPMOST;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, FindWindowExW, GetCursorPos, GetForegroundWindow, GetWindowRect, LoadIconW,
    SendMessageW, SetForegroundWindow, SetWindowPos, SwitchToThisWindow, ICON_BIG, ICON_SMALL,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WM_SETICON,
};

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

/// 通过 Win32 物理像素移动已创建的面板并断言 topmost，修正后端覆盖位置或层级的问题。
pub(crate) fn move_panel(position: PanelPosition) -> bool {
    set_panel_topmost(Some(position))
}

/// 在不改变当前位置和尺寸的前提下重新断言 topmost，供重复热键激活路径使用。
pub(crate) fn reassert_panel_topmost() -> bool {
    set_panel_topmost(None)
}

/// 将应用资源图标显式设置到 Slint/Winit 创建的真实 HWND；托盘图标不会自动同步到标题栏。
///
/// 该函数只使用模块内共享资源句柄，不负责销毁由 Windows 资源表管理的图标；窗口销毁时
/// Windows 会清理 `WM_SETICON` 的引用。找不到窗口或资源时返回 `false`，不阻断面板显示。
pub(crate) fn apply_panel_icon() -> bool {
    let hwnd = find_panel_hwnd();
    if hwnd.is_null() {
        return false;
    }
    let module = unsafe { GetModuleHandleW(null()) };
    if module.is_null() {
        return false;
    }
    let icon = unsafe { LoadIconW(module, APP_ICON_RESOURCE_ID as usize as *const u16) };
    if icon.is_null() {
        return false;
    }
    unsafe {
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, icon as isize);
        SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, icon as isize);
    }
    true
}

/// 使用统一的 `HWND_TOPMOST` 调用设置面板层级；`None` 只改变 Z 序。
fn set_panel_topmost(position: Option<PanelPosition>) -> bool {
    let hwnd = find_panel_hwnd();
    if hwnd.is_null() {
        return false;
    }
    let (x, y) = position.map(|value| (value.x, value.y)).unwrap_or((0, 0));

    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            0,
            0,
            panel_position_flags(position.is_some()),
        ) != 0
    }
}

/// 根据是否需要移动计算稳定的 `SetWindowPos` flags；任何路径都不能禁用 Z 序更新。
const fn panel_position_flags(reposition: bool) -> u32 {
    let base = SWP_NOSIZE | SWP_NOACTIVATE;
    if reposition {
        base
    } else {
        base | SWP_NOMOVE
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

/// 尝试重新激活已经创建的面板窗口；Windows 前台锁拒绝请求时返回 `false`。
///
/// 失败只影响本次输入焦点申请，不改变 UI reducer 的可见状态；用户仍可直接点击面板。
pub(crate) fn activate_panel() -> bool {
    let hwnd = find_panel_hwnd();
    if hwnd.is_null() {
        return false;
    }

    unsafe {
        let foreground = GetForegroundWindow();
        if foreground == hwnd {
            return true;
        }

        let _ = BringWindowToTop(hwnd);
        let _ = SetActiveWindow(hwnd);
        if SetForegroundWindow(hwnd) == 0 || GetForegroundWindow() != hwnd {
            // Windows 前台锁可能拒绝普通激活；该 API 与系统任务切换行为一致，作为
            // 有界降级确保用户通过热键呼出后可立即输入搜索或按 Esc 关闭。
            SwitchToThisWindow(hwnd, 1);
        }
        let activated = GetForegroundWindow() == hwnd;
        if activated {
            let _ = SetFocus(hwnd);
        }
        activated
    }
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

#[cfg(test)]
mod tests {
    //! 此测试模块覆盖不依赖桌面状态的窗口定位边界。

    use super::{center_position, panel_position_flags, PanelPosition, WorkArea};

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
            center_position(area, 720, 520),
            PanelPosition { x: -1320, y: 200 }
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
            center_position(area, 720, 520),
            PanelPosition { x: 100, y: 50 }
        );
    }

    /// 首次定位和重复激活都必须允许修改 Z 序，重复激活额外禁止移动。
    #[test]
    fn topmost_调用保留_z_序更新() {
        let positioned = panel_position_flags(true);
        let reasserted = panel_position_flags(false);
        assert_eq!(
            positioned & windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
            0
        );
        assert_eq!(
            reasserted & windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
            0
        );
        assert_ne!(
            reasserted & windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOMOVE,
            0
        );
    }
}

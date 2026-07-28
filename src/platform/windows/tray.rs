//! 此模块负责在热键消息线程上创建临时系统托盘图标和“打开/退出”菜单。
//!
//! 托盘回调只把用户动作投递到 UI 事件队列；`TrayGuard` 独占通知数据，保证
//! `NIM_DELETE` 先于 message-only HWND 销毁，并对菜单、系统图标和错误路径做闭合处理。

use super::hotkey::HotkeyError;
use crate::app::post_ui_event;
use crate::command::UiEvent;
use std::mem::size_of;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{GetLastError, SetLastError, HWND, POINT};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIM_ADD, NIM_DELETE, NIM_SETVERSION, NOTIFYICONDATAW,
    NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CopyIcon, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos, LoadIconW,
    SetForegroundWindow, TrackPopupMenu, HICON, IDI_APPLICATION, MF_STRING, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_LBUTTONUP, WM_RBUTTONUP,
};

/// 托盘回调消息使用独立编号，避免与单实例唤起消息混淆。
pub(crate) const TRAY_CALLBACK_MESSAGE: u32 =
    windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 2;
/// 当前进程只创建一个托盘图标，固定 ID 便于拒绝伪造通知。
pub(crate) const TRAY_ICON_ID: u32 = 1;
/// “打开”菜单命令 ID；不复用系统保留值。
const TRAY_MENU_OPEN: usize = 0x5001;
/// “退出”菜单命令 ID；不复用系统保留值。
const TRAY_MENU_EXIT: usize = 0x5002;

/// 持有托盘通知数据直到消息线程退出，确保图标生命周期覆盖整个消息泵。
pub(crate) struct TrayGuard {
    data: NOTIFYICONDATAW,
    /// 这是由 `CopyIcon` 创建的私有副本，Drop 必须调用 `DestroyIcon`。
    icon: HICON,
    removed: bool,
}

impl TrayGuard {
    /// 向 Shell 注册图标并设置回调版本；任一步失败都回滚已创建的通知图标。
    pub(crate) fn create(window: HWND) -> Result<Self, HotkeyError> {
        if window.is_null() {
            return Err(HotkeyError::Tray("创建托盘图标时收到空窗口句柄".to_owned()));
        }

        let system_icon = unsafe { LoadIconW(null_mut(), IDI_APPLICATION) };
        if system_icon.is_null() {
            return Err(last_error("LoadIconW"));
        }
        // LoadIconW 返回系统共享图标，不能销毁；复制一份私有图标后由 TrayGuard 管理。
        let icon = unsafe { CopyIcon(system_icon) };
        if icon.is_null() {
            return Err(last_error("CopyIcon"));
        }

        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            uID: TRAY_ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON,
            uCallbackMessage: TRAY_CALLBACK_MESSAGE,
            hIcon: icon,
            ..NOTIFYICONDATAW::default()
        };
        set_tip(&mut data, "ClipboardBoard");

        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data) } != 0;
        if !added {
            let error_code = last_error_code();
            destroy_icon(icon);
            return Err(tray_error("Shell_NotifyIconW(NIM_ADD)", error_code));
        }

        // 采用版本 4 让现代 Shell 将鼠标消息原样放到 lParam，仍由本模块校验固定 uID。
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let version_set = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) } != 0;
        if !version_set {
            let error_code = last_error_code();
            let cleanup_result = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) } != 0;
            if !cleanup_result {
                let cleanup_error_code = last_error_code();
                eprintln!(
                    "托盘版本设置失败后的 NIM_DELETE 也失败：{}",
                    cleanup_error_code
                );
            }
            destroy_icon(icon);
            return Err(tray_error("Shell_NotifyIconW(NIM_SETVERSION)", error_code));
        }

        Ok(Self {
            data,
            icon,
            removed: false,
        })
    }

    /// 从 Shell 移除图标；显式清理成功后 Drop 不会重复调用。
    pub(crate) fn remove(&mut self) -> Result<(), HotkeyError> {
        if self.removed {
            return Ok(());
        }

        let removed = unsafe { Shell_NotifyIconW(NIM_DELETE, &self.data) } != 0;
        if !removed {
            let error_code = last_error_code();
            return Err(tray_error("Shell_NotifyIconW(NIM_DELETE)", error_code));
        }
        self.removed = true;
        Ok(())
    }
}

impl Drop for TrayGuard {
    /// 异常展开时尽力移除图标；正常退出应优先调用 `remove` 传播错误。
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            eprintln!("退出时移除托盘图标失败：{error}");
        }

        if unsafe { DestroyIcon(self.icon) } == 0 {
            eprintln!("销毁托盘私有图标失败，错误码 {}", last_error_code());
        }
    }
}

/// 处理 Shell 托盘回调；版本 4 将图标 ID 和鼠标消息打包在 lParam 的高低字中。
pub(crate) fn handle_callback(window: HWND, _wparam: usize, lparam: isize) -> bool {
    let Some(_) = decode_v4_callback(lparam) else {
        return false;
    };

    if let Err(error) = show_menu(window) {
        eprintln!("显示托盘菜单失败：{error}");
    }
    true
}

/// 解码 `NOTIFYICON_VERSION_4` 的回调布局：高 16 位是图标 ID，低 16 位是鼠标消息。
fn decode_v4_callback(lparam: isize) -> Option<u32> {
    let packed = lparam as u32;
    let icon_id = (packed >> 16) & 0xffff;
    let message = packed & 0xffff;
    if icon_id != TRAY_ICON_ID || !is_supported_mouse_message(message) {
        return None;
    }
    Some(message)
}

/// 仅允许左键或右键释放触发菜单，忽略 Shell 的其他通知类型。
fn is_supported_mouse_message(message: u32) -> bool {
    matches!(message, WM_LBUTTONUP | WM_RBUTTONUP)
}

/// 创建并同步显示最小托盘菜单；菜单句柄在所有分支都由 `DestroyMenu` 回收。
fn show_menu(window: HWND) -> Result<(), HotkeyError> {
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return Err(last_error("CreatePopupMenu"));
    }

    let append_result = append_menu_items(menu);
    if let Err(error) = append_result {
        if let Err(cleanup_error) = destroy_menu(menu) {
            eprintln!("托盘菜单添加失败后的 DestroyMenu 也失败：{cleanup_error}");
        }
        return Err(error);
    }

    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) } == 0 {
        let error = last_error("GetCursorPos");
        if let Err(cleanup_error) = destroy_menu(menu) {
            eprintln!("读取托盘菜单位置失败后的 DestroyMenu 也失败：{cleanup_error}");
        }
        return Err(error);
    }
    if unsafe { SetForegroundWindow(window) } == 0 {
        // message-only HWND 不能成为前台窗口；TPM_RETURNCMD 仍能返回菜单命令，
        // 因此这里只记录诊断并继续显示，避免托盘菜单在隐藏后台窗口上完全不可用。
        eprintln!(
            "托盘菜单宿主无法置前，继续使用消息窗口显示：{}",
            last_error_code()
        );
    }

    // TPM_RETURNCMD 让菜单命令在本消息线程同步返回；WM_QUIT 会被系统的模态菜单循环
    // 取出并重新投递，HotkeyManager::stop 因而仍能唤醒该线程完成 join。
    unsafe { SetLastError(0) };
    let command = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            window,
            null(),
        )
    } as usize;
    let track_error_code = last_error_code();
    let destroy_result = destroy_menu(menu);
    destroy_result?;
    if command == 0 {
        // 用户点击菜单外部时返回零且没有 Win32 错误，这是正常取消而不是故障。
        if track_error_code != 0 {
            return Err(HotkeyError::Tray(format!(
                "TrackPopupMenu 失败，错误码 {track_error_code}"
            )));
        }
        return Ok(());
    }

    match command {
        TRAY_MENU_OPEN => post_ui_event(UiEvent::ShowPanel)
            .map_err(|error| HotkeyError::Tray(format!("托盘打开事件无法进入 UI 队列：{error}"))),
        TRAY_MENU_EXIT => post_ui_event(UiEvent::Quit)
            .map_err(|error| HotkeyError::Tray(format!("托盘退出事件无法进入 UI 队列：{error}"))),
        _ => Ok(()),
    }
}

/// 添加固定的“打开/退出”菜单项，任何失败都交给调用方清理菜单句柄。
fn append_menu_items(
    menu: windows_sys::Win32::UI::WindowsAndMessaging::HMENU,
) -> Result<(), HotkeyError> {
    let open_added = unsafe {
        AppendMenuW(
            menu,
            MF_STRING,
            TRAY_MENU_OPEN,
            windows_sys::core::w!("打开"),
        )
    } != 0;
    if !open_added {
        return Err(last_error("AppendMenuW(打开)"));
    }

    let exit_added = unsafe {
        AppendMenuW(
            menu,
            MF_STRING,
            TRAY_MENU_EXIT,
            windows_sys::core::w!("退出"),
        )
    } != 0;
    if !exit_added {
        return Err(last_error("AppendMenuW(退出)"));
    }
    Ok(())
}

/// 销毁菜单；菜单销毁失败必须被观察到，避免句柄泄漏被静默吞掉。
fn destroy_menu(
    menu: windows_sys::Win32::UI::WindowsAndMessaging::HMENU,
) -> Result<(), HotkeyError> {
    let destroyed = unsafe { DestroyMenu(menu) } != 0;
    if !destroyed {
        let destroy_error = last_error("DestroyMenu");
        return Err(destroy_error);
    }
    Ok(())
}

/// 销毁由 `CopyIcon` 生成的私有 HICON；失败只记录，因为调用方已进入错误清理路径。
fn destroy_icon(icon: HICON) {
    if unsafe { DestroyIcon(icon) } == 0 {
        eprintln!("清理托盘私有图标失败，错误码 {}", last_error_code());
    }
}

/// 将固定托盘提示复制进 Win32 的 UTF-16 缓冲区并保证 NUL 终止。
fn set_tip(data: &mut NOTIFYICONDATAW, tip: &str) {
    let mut encoded = tip.encode_utf16();
    let content_capacity = data.szTip.len().saturating_sub(1);
    for slot in data.szTip.iter_mut().take(content_capacity) {
        *slot = encoded.next().unwrap_or(0);
    }
    // 始终保留最后一个槽位作为 NUL，避免过长提示破坏 Shell 的边界。
    if let Some(last) = data.szTip.last_mut() {
        *last = 0;
    }
}

/// 生成包含 Win32 操作和错误码的统一托盘错误。
fn last_error(operation: &'static str) -> HotkeyError {
    tray_error(operation, last_error_code())
}

/// 使用已捕获的错误码构造托盘错误，避免清理 API 覆盖原始诊断信息。
fn tray_error(operation: &'static str, code: u32) -> HotkeyError {
    HotkeyError::Tray(format!("{operation} 失败，错误码 {code}"))
}

/// 读取当前线程的 Win32 错误码；调用方只在刚刚完成 API 调用后使用它。
fn last_error_code() -> u32 {
    unsafe { GetLastError() }
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证固定托盘消息和命令值，不依赖桌面 Shell 状态。

    use super::{
        decode_v4_callback, is_supported_mouse_message, TRAY_CALLBACK_MESSAGE, TRAY_ICON_ID,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{WM_APP, WM_LBUTTONUP, WM_RBUTTONUP};

    /// 托盘回调必须和单实例唤起、热键消息使用不同编号。
    #[test]
    fn 托盘回调编号独立() {
        assert_eq!(TRAY_CALLBACK_MESSAGE, WM_APP + 2);
        assert_ne!(TRAY_CALLBACK_MESSAGE, WM_APP + 1);
        assert_eq!(TRAY_ICON_ID, 1);
    }

    /// 仅接受托盘支持的鼠标释放消息，其他 Shell 通知不会打开菜单。
    #[test]
    fn 托盘交互消息边界稳定() {
        assert!(is_supported_mouse_message(WM_LBUTTONUP));
        assert!(is_supported_mouse_message(WM_RBUTTONUP));
        assert!(!is_supported_mouse_message(WM_LBUTTONUP + 1));
    }

    /// 版本 4 的 Shell 回调必须从 lParam 高低字正确取出图标 ID 和鼠标消息。
    #[test]
    fn 版本四托盘回调解码稳定() {
        let left_click = ((TRAY_ICON_ID << 16) | WM_LBUTTONUP) as isize;
        let right_click = ((TRAY_ICON_ID << 16) | WM_RBUTTONUP) as isize;
        let wrong_icon = (2_u32 << 16 | WM_LBUTTONUP) as isize;

        assert_eq!(decode_v4_callback(left_click), Some(WM_LBUTTONUP));
        assert_eq!(decode_v4_callback(right_click), Some(WM_RBUTTONUP));
        assert_eq!(decode_v4_callback(wrong_icon), None);
    }
}

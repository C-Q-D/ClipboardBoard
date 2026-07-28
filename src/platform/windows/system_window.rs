//! 此模块创建 message-only HWND，并在其所属线程注册和处理 Alt+V 热键、单实例唤起及托盘消息。
//!
//! Win32 回调只负责把匹配的消息转成 UI 事件并排入 UI 事件队列；它不会直接访问
//! Slint 对象、剪贴板或其他业务状态。

use super::hotkey::{HotkeyError, HotkeySpec};
use super::tray::{handle_callback, TrayGuard, TRAY_CALLBACK_MESSAGE};
use crate::app::post_ui_event;
use crate::command::UiEvent;
use std::ptr::{null, null_mut};
use std::sync::mpsc::SyncSender;

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, ERROR_HOTKEY_ALREADY_REGISTERED, HINSTANCE,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PeekMessageW,
    RegisterClassExW, TranslateMessage, HWND_MESSAGE, MSG, PM_NOREMOVE, WM_APP, WM_HOTKEY,
    WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

/// Win32 注册的窗口类名称；message-only 窗口不出现在任务栏或屏幕上。
pub(crate) const WINDOW_CLASS_NAME: windows_sys::core::PCWSTR =
    windows_sys::core::w!("ClipboardBoardHotkey");

/// 单实例二次启动使用的进程间消息；消息不携带剪贴板正文或其他敏感数据。
pub(crate) const OPEN_PANEL_MESSAGE: u32 = WM_APP + 1;

/// 在专用线程创建隐藏窗口、注册热键并运行消息泵。
pub(crate) fn run(
    hotkey: HotkeySpec,
    ready_sender: SyncSender<Result<u32, HotkeyError>>,
) -> Result<(), HotkeyError> {
    let thread_id = unsafe { GetCurrentThreadId() };

    // 先创建消息队列，再允许主线程在启动阶段投递停止消息。
    unsafe {
        let mut message = MSG::default();
        let _ = PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE);
    }

    let instance = unsafe { GetModuleHandleW(null()) };
    if instance.is_null() {
        let error = HotkeyError::Windows {
            operation: "GetModuleHandleW",
            code: unsafe { GetLastError() },
        };
        let _ = ready_sender.send(Err(error.clone()));
        return Err(error);
    }

    if let Err(error) = unsafe { register_window_class(instance as HINSTANCE) } {
        let _ = ready_sender.send(Err(error.clone()));
        return Err(error);
    }

    let window = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            WINDOW_CLASS_NAME,
            windows_sys::core::w!(""),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance as HINSTANCE,
            null(),
        )
    };
    if window.is_null() {
        let error = HotkeyError::Windows {
            operation: "CreateWindowExW",
            code: unsafe { GetLastError() },
        };
        let _ = ready_sender.send(Err(error.clone()));
        return Err(error);
    }

    let registration_result = unsafe {
        if RegisterHotKey(window, hotkey.id, hotkey.modifiers, hotkey.virtual_key) == 0 {
            let code = GetLastError();
            Err(classify_registration_error(code, hotkey.label))
        } else {
            Ok(())
        }
    };

    if let Err(error) = registration_result {
        unsafe {
            let _ = DestroyWindow(window);
        }
        let _ = ready_sender.send(Err(error.clone()));
        return Err(error);
    }

    // 托盘图标必须绑定到同一个 message-only HWND，确保回调和热键共享消息线程。
    let mut tray = match TrayGuard::create(window) {
        Ok(tray) => tray,
        Err(error) => {
            unsafe {
                let _ = UnregisterHotKey(window, hotkey.id);
                let _ = DestroyWindow(window);
            }
            let _ = ready_sender.send(Err(error.clone()));
            return Err(error);
        }
    };

    if ready_sender.send(Ok(thread_id)).is_err() {
        let _ = tray.remove();
        // 若第一次 NIM_DELETE 失败，Drop 会在 DestroyWindow 前再尝试一次。
        drop(tray);
        unsafe {
            let _ = UnregisterHotKey(window, hotkey.id);
            let _ = DestroyWindow(window);
        }
        return Err(HotkeyError::StartupChannelClosed);
    }

    let message_loop_result = message_loop();
    // NIM_DELETE 必须发生在 DestroyWindow 之前；即使删除失败也继续注销热键和销毁窗口。
    let tray_result = tray.remove();
    // Drop 的兜底重试仍发生在 DestroyWindow 前，避免把通知数据绑定到已销毁 HWND。
    drop(tray);
    unsafe {
        let _ = UnregisterHotKey(window, hotkey.id);
        let _ = DestroyWindow(window);
    }
    message_loop_result.and(tray_result)
}

/// 注册窗口类；类已存在时复用它，避免重复启动测试造成无意义失败。
unsafe fn register_window_class(instance: HINSTANCE) -> Result<(), HotkeyError> {
    let window_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: WINDOW_CLASS_NAME,
        ..WNDCLASSEXW::default()
    };

    if RegisterClassExW(&window_class) == 0 {
        let code = GetLastError();
        if code != ERROR_CLASS_ALREADY_EXISTS {
            return Err(HotkeyError::Windows {
                operation: "RegisterClassExW",
                code,
            });
        }
    }
    Ok(())
}

/// 处理隐藏窗口收到的消息；只有固定热键 ID 才能产生 UI 事件。
unsafe extern "system" fn window_proc(
    window: windows_sys::Win32::Foundation::HWND,
    message: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    if is_tray_callback_message(message) && handle_callback(window, wparam, lparam) {
        return 0;
    }

    if is_open_panel_message(message) || is_default_hotkey_message(message, wparam) {
        if let Err(error) = post_ui_event(UiEvent::OpenPanel) {
            eprintln!("打开面板事件无法进入 UI 事件队列：{error}");
        }
        return 0;
    }

    DefWindowProcW(window, message, wparam, lparam)
}

/// 只接受固定托盘回调编号，避免把任意 WM_APP 消息当作 Shell 通知。
fn is_tray_callback_message(message: u32) -> bool {
    message == TRAY_CALLBACK_MESSAGE
}

/// 将 RegisterHotKey 的错误码转换为不会被静默吞掉的领域错误。
fn classify_registration_error(code: u32, shortcut: &'static str) -> HotkeyError {
    if code == ERROR_HOTKEY_ALREADY_REGISTERED {
        HotkeyError::RegistrationConflict { shortcut }
    } else {
        HotkeyError::Windows {
            operation: "RegisterHotKey",
            code,
        }
    }
}

/// 只接受默认热键的 WM_HOTKEY 消息，其他消息交给 DefWindowProcW。
fn is_default_hotkey_message(message: u32, wparam: usize) -> bool {
    message == WM_HOTKEY && wparam == super::hotkey::DEFAULT_HOTKEY.id as usize
}

/// 只接受固定的单实例唤起消息，避免把任意 WM_APP 消息当作业务命令。
fn is_open_panel_message(message: u32) -> bool {
    message == OPEN_PANEL_MESSAGE
}

/// 拉取并分发消息，返回值 -1 被视为 Win32 错误，0 表示收到退出消息。
fn message_loop() -> Result<(), HotkeyError> {
    loop {
        let mut message = MSG::default();
        let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        if result == -1 {
            return Err(HotkeyError::Windows {
                operation: "GetMessageW",
                code: unsafe { GetLastError() },
            });
        }
        if result == 0 {
            return Ok(());
        }

        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证热键 ID 过滤和冲突错误映射，不依赖桌面上的其他热键占用者。

    use super::TRAY_CALLBACK_MESSAGE;
    use super::{
        classify_registration_error, is_default_hotkey_message, is_open_panel_message,
        is_tray_callback_message, OPEN_PANEL_MESSAGE,
    };
    use crate::platform::windows::hotkey::HotkeyError;
    use windows_sys::Win32::Foundation::ERROR_HOTKEY_ALREADY_REGISTERED;
    use windows_sys::Win32::UI::WindowsAndMessaging::WM_HOTKEY;

    /// 只有固定 ID 的 WM_HOTKEY 才能进入 UI 事件转换分支。
    #[test]
    fn 只接受默认热键消息() {
        assert!(is_default_hotkey_message(WM_HOTKEY, 0x4342));
        assert!(!is_default_hotkey_message(WM_HOTKEY, 0x4343));
        assert!(!is_default_hotkey_message(WM_HOTKEY + 1, 0x4342));
    }

    /// 二次启动只能使用固定消息编号唤起主实例，其他消息必须被忽略。
    #[test]
    fn 只接受固定打开消息() {
        assert!(is_open_panel_message(OPEN_PANEL_MESSAGE));
        assert!(!is_open_panel_message(OPEN_PANEL_MESSAGE + 1));
    }

    /// 只有固定托盘回调消息才进入托盘处理器。
    #[test]
    fn 只接受固定托盘消息() {
        assert!(is_tray_callback_message(TRAY_CALLBACK_MESSAGE));
        assert!(!is_tray_callback_message(TRAY_CALLBACK_MESSAGE + 1));
    }

    /// Win32 的热键占用错误必须转换成带快捷键名称的明确错误。
    #[test]
    fn 热键冲突错误映射明确() {
        assert_eq!(
            classify_registration_error(ERROR_HOTKEY_ALREADY_REGISTERED, "Alt + V"),
            HotkeyError::RegistrationConflict {
                shortcut: "Alt + V"
            }
        );
        assert_eq!(
            classify_registration_error(5, "Alt + V"),
            HotkeyError::Windows {
                operation: "RegisterHotKey",
                code: 5
            }
        );
    }
}

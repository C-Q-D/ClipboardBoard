//! 此模块创建 message-only HWND，并在其所属线程注册和处理 Alt+V 热键。
//!
//! Win32 回调只负责把匹配的 WM_HOTKEY 转成 `UiEvent::OpenPanel` 并排入 UI 事件队列；
//! 它不会直接访问 Slint 对象、剪贴板或其他业务状态。

use super::hotkey::{HotkeyError, HotkeySpec};
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
    RegisterClassExW, TranslateMessage, HWND_MESSAGE, MSG, PM_NOREMOVE, WM_HOTKEY, WNDCLASSEXW,
    WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

/// Win32 注册的窗口类名称；message-only 窗口不出现在任务栏或屏幕上。
const WINDOW_CLASS_NAME: windows_sys::core::PCWSTR = windows_sys::core::w!("ClipboardBoardHotkey");

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

    if ready_sender.send(Ok(thread_id)).is_err() {
        unsafe {
            let _ = UnregisterHotKey(window, hotkey.id);
            let _ = DestroyWindow(window);
        }
        return Err(HotkeyError::StartupChannelClosed);
    }

    let message_loop_result = message_loop();
    unsafe {
        let _ = UnregisterHotKey(window, hotkey.id);
        let _ = DestroyWindow(window);
    }
    message_loop_result
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
    if is_default_hotkey_message(message, wparam) {
        if let Err(error) = post_ui_event(UiEvent::OpenPanel) {
            eprintln!("全局快捷键事件无法进入 UI 事件队列：{error}");
        }
        return 0;
    }

    DefWindowProcW(window, message, wparam, lparam)
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

    use super::{classify_registration_error, is_default_hotkey_message};
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

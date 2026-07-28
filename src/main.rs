//! 此二进制入口负责创建主窗口并启动 Slint 事件循环。
//!
//! 当前入口先完成单实例判定，再创建 UI、绑定弱窗口并启动热键、剪贴板结果泵和托盘消息线程；
//! 剪贴板写回、历史持久化和完整窗口交互仍由后续原子接入。

#[cfg(windows)]
use clipboard_board::app::bind_app_window;
#[cfg(windows)]
use clipboard_board::app::post_ui_event;
#[cfg(windows)]
use clipboard_board::clipboard::ClipboardCaptureInbox;
#[cfg(windows)]
use clipboard_board::command::{UiClipboardItem, UiEvent};
#[cfg(windows)]
use clipboard_board::diagnostics::{self, DiagnosticEvent, ThreadState};
#[cfg(windows)]
use clipboard_board::platform::windows::{acquire_or_activate, HotkeyManager, SingleInstanceRole};
#[cfg(windows)]
use slint::ComponentHandle;
#[cfg(windows)]
use std::thread::{self, JoinHandle};

/// 启动 ClipboardBoard 的最小桌面窗口。
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 单实例检查必须早于 UI、SQLite 和热键初始化，二次进程只负责通知主实例。
    let _instance_guard = match acquire_or_activate()? {
        SingleInstanceRole::Primary(guard) => guard,
        SingleInstanceRole::Secondary => return Ok(()),
    };

    // 仅主实例初始化日志，并且早于 Win32 消息线程；事件只经过隐私字段白名单序列化。
    diagnostics::init();
    diagnostics::emit(DiagnosticEvent::thread_state(ThreadState::Starting));

    let window = clipboard_board::create_app_window()?;
    bind_app_window(&window);
    window.hide()?;

    let hotkey_manager = HotkeyManager::start()?;
    let capture_pump = match start_clipboard_pump(hotkey_manager.clipboard_inbox()) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = hotkey_manager.stop();
            return Err(error.into());
        }
    };
    diagnostics::emit(DiagnosticEvent::thread_state(ThreadState::Running));
    let event_loop_result = slint::run_event_loop_until_quit();
    diagnostics::emit(DiagnosticEvent::thread_state(ThreadState::Stopping));
    let hotkey_result = hotkey_manager.stop();
    let capture_pump_result = capture_pump
        .join()
        .map_err(|_| "剪贴板结果泵线程异常退出")
        .map(|_| ());
    diagnostics::emit(DiagnosticEvent::thread_state(ThreadState::Stopped));

    event_loop_result?;
    hotkey_result?;
    capture_pump_result?;
    Ok(())
}

/// 启动结果桥消费线程；该线程只做 DTO 转换和 UI 事件投递，不触碰 Slint 对象。
#[cfg(windows)]
fn start_clipboard_pump(inbox: ClipboardCaptureInbox) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("clipboard-board-capture-pump".to_owned())
        .spawn(move || {
            while let Some(event) = inbox.wait_take() {
                let Ok(capture) = event else {
                    // sequence 失配或格式错误只丢弃本次结果，不能终止后续复制事件。
                    continue;
                };
                let item = UiClipboardItem::from_capture(&capture);
                if post_ui_event(UiEvent::ClipboardCaptured(item)).is_err() {
                    // UI 事件循环已停止时退出泵，避免继续堆积无效闭包。
                    break;
                }
            }
        })
}

/// 非 Windows 目标仅保留骨架启动能力，正式热键实现由 Windows 平台模块提供。
#[cfg(not(windows))]
fn main() -> Result<(), slint::PlatformError> {
    let window = clipboard_board::create_app_window()?;
    slint::ComponentHandle::run(&window)
}

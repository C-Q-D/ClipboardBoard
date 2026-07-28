//! 此二进制入口负责创建主窗口并启动 Slint 事件循环。
//!
//! 当前入口先完成单实例判定，再创建 UI、初始化 SQLite、绑定弱窗口并启动热键、剪贴板
//! 结果泵和托盘消息线程；启动恢复、剪贴板写回和完整窗口交互仍由后续原子接入。

#[cfg(windows)]
use clipboard_board::app::bind_app_window;
#[cfg(windows)]
use clipboard_board::app::post_ui_event;
#[cfg(windows)]
use clipboard_board::clipboard::ClipboardCaptureInbox;
#[cfg(windows)]
use clipboard_board::command::UiEvent;
#[cfg(windows)]
use clipboard_board::diagnostics::{self, DiagnosticEvent, ThreadState};
#[cfg(windows)]
use clipboard_board::history_bridge::{process_capture, unix_millis_now, CaptureProcessOutcome};
#[cfg(windows)]
use clipboard_board::history_restore::load_startup_snapshot;
#[cfg(windows)]
use clipboard_board::platform::windows::{acquire_or_activate, HotkeyManager, SingleInstanceRole};
#[cfg(windows)]
use clipboard_board::storage::StorageExecutor;
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

    // 存储执行器必须在热键和剪贴板监听前唯一创建，并先完成启动恢复。
    let mut storage = StorageExecutor::open()?;
    let startup_snapshot = load_startup_snapshot(&mut storage)?;
    post_ui_event(UiEvent::ReplaceSnapshot(startup_snapshot))?;
    let hotkey_manager = HotkeyManager::start()?;
    let capture_pump = match start_clipboard_pump(hotkey_manager.clipboard_inbox(), storage) {
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

/// 启动结果桥消费线程；该线程先提交 SQLite，再投递 DTO，不触碰 Slint 对象。
#[cfg(windows)]
fn start_clipboard_pump(
    inbox: ClipboardCaptureInbox,
    mut storage: StorageExecutor,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("clipboard-board-capture-pump".to_owned())
        .spawn(move || {
            while let Some(event) = inbox.wait_take() {
                let Ok(capture) = event else {
                    // sequence 失配或格式错误只丢弃本次结果，不能终止后续复制事件。
                    continue;
                };
                let result = process_capture(&mut storage, capture, unix_millis_now(), |event| {
                    post_ui_event(event).is_ok()
                });
                match result {
                    Ok(CaptureProcessOutcome::Posted) => {}
                    Ok(CaptureProcessOutcome::UiClosed) => {
                        // sink=false 代表 UI 已停止；离开闭包会 drop 唯一存储执行器。
                        break;
                    }
                    Ok(CaptureProcessOutcome::Skipped) => {
                        // 当前有效文本不会走此分支，保留分支以兼容未来非 UI 捕获类型。
                    }
                    Err(error) => {
                        // 错误不携带正文；记录后继续处理后续捕获，避免一次失败拖垮常驻工具。
                        eprintln!("剪贴板捕获处理失败：{error}");
                    }
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

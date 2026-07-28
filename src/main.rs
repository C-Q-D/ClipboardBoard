//! 此二进制入口负责创建主窗口并启动 Slint 事件循环。
//!
//! 当前入口先完成单实例判定，再创建 UI、绑定弱窗口并启动热键和托盘消息线程；
//! 剪贴板读写与存储行为由后续原子接入。

#[cfg(windows)]
use clipboard_board::app::bind_app_window;
#[cfg(windows)]
use clipboard_board::platform::windows::{acquire_or_activate, HotkeyManager, SingleInstanceRole};
#[cfg(windows)]
use slint::ComponentHandle;

/// 启动 ClipboardBoard 的最小桌面窗口。
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 单实例检查必须早于 UI、SQLite 和热键初始化，二次进程只负责通知主实例。
    let _instance_guard = match acquire_or_activate()? {
        SingleInstanceRole::Primary(guard) => guard,
        SingleInstanceRole::Secondary => return Ok(()),
    };

    let window = clipboard_board::create_app_window()?;
    bind_app_window(&window);
    window.hide()?;

    let hotkey_manager = HotkeyManager::start()?;
    let event_loop_result = slint::run_event_loop_until_quit();
    let hotkey_result = hotkey_manager.stop();

    event_loop_result?;
    hotkey_result?;
    Ok(())
}

/// 非 Windows 目标仅保留骨架启动能力，正式热键实现由 Windows 平台模块提供。
#[cfg(not(windows))]
fn main() -> Result<(), slint::PlatformError> {
    let window = clipboard_board::create_app_window()?;
    slint::ComponentHandle::run(&window)
}

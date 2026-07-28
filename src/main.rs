//! 此二进制入口负责创建主窗口并启动 Slint 事件循环。
//!
//! 当前入口只负责创建 UI、绑定弱窗口并启动热键管理器；托盘和剪贴板行为由后续原子接入。

#[cfg(windows)]
use clipboard_board::app::bind_app_window;
#[cfg(windows)]
use clipboard_board::platform::windows::HotkeyManager;
#[cfg(windows)]
use slint::ComponentHandle;

/// 启动 ClipboardBoard 的最小桌面窗口。
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
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

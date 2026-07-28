//! 此二进制入口负责创建主窗口并启动 Slint 事件循环。
//!
//! 当前阶段故意保持入口极小，避免在热键、托盘和剪贴板原子之前引入后台行为。

use slint::ComponentHandle;

/// 启动 ClipboardBoard 的最小桌面窗口。
fn main() -> Result<(), slint::PlatformError> {
    let window = clipboard_board::create_app_window()?;
    window.run()
}

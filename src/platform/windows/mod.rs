//! 此模块承载 Windows 原生消息窗口和全局快捷键实现。

mod hotkey;
mod system_window;

pub use hotkey::{HotkeyError, HotkeyManager};

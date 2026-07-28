//! 此模块承载 Windows 原生消息窗口、全局快捷键和单实例进程边界实现。

mod hotkey;
mod single_instance;
mod system_window;

pub use hotkey::{HotkeyError, HotkeyManager};
pub use single_instance::{
    acquire_or_activate, SingleInstanceError, SingleInstanceGuard, SingleInstanceRole,
};

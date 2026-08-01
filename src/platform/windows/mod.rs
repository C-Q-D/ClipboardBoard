//! 此模块承载 Windows 原生消息窗口、全局快捷键和单实例进程边界实现。

mod hotkey;
mod single_instance;
mod source;
mod system_window;
mod tray;
pub(crate) mod window;

pub use hotkey::{HotkeyError, HotkeyManager};
pub use single_instance::{
    acquire_or_activate, SingleInstanceError, SingleInstanceGuard, SingleInstanceRole,
};
pub use source::{
    capture_foreground_source, capture_foreground_source_snapshot, ProcessSource,
    ProcessSourceSnapshot,
};

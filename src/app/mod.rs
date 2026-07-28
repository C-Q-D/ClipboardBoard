//! 此模块承载应用层的 UI 线程边界，不直接处理 Windows、剪贴板或持久化细节。

mod ui_event;

pub use ui_event::{bind_app_window, post_ui_event, ui_state_snapshot, UiStateSnapshot};

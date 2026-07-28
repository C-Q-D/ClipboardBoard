//! 此模块承载应用层的 UI 线程边界，不读取剪贴板正文、不访问持久化连接；仅复制动作只
//! 通过 ID/哈希桥接到后台结果泵，避免把正文或 Win32 句柄带入 UI 状态。

mod ui_event;

#[cfg(windows)]
pub use ui_event::bind_copy_request_inbox;
pub use ui_event::{
    bind_app_window, bind_history_query_bridge, post_ui_event, ui_state_snapshot, UiStateSnapshot,
};

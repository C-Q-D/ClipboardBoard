//! 此模块承载应用层的 UI 线程边界，不读取剪贴板正文、不访问持久化连接；仅复制动作只
//! 通过 ID/哈希桥接到后台结果泵，避免把正文或 Win32 句柄带入 UI 状态。

/// 历史混合高度的纯 Rust prefix-sum 和窗口计算，不依赖 UI 运行时。
pub mod history_geometry;
mod ui_event;

#[cfg(windows)]
pub use ui_event::bind_copy_request_inbox;
pub use ui_event::{
    bind_app_window, bind_clear_history_mutation_sender, bind_delete_mutation_sender,
    bind_history_query_bridge, bind_pin_mutation_sender, bind_thumbnail_loader_sender,
    post_ui_event, set_history_geometry_metadata, set_window_commit, ui_state_snapshot,
    UiStateSnapshot,
};

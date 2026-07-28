//! 此模块封装面板生命周期需要的 Windows 原生窗口、显示器和目标进程查询。
//!
//! 上层只接收不可变的值对象；具体 Win32 句柄只在本模块内短暂使用，避免把平台
//! 资源泄漏到 Slint 状态或后台线程。自动粘贴的目标复核和输入注入也固定在 UI 线程。

mod lifecycle;

pub(crate) use lifecycle::{
    capture_target, center_position, cursor_work_area, execute_paste, move_panel, panel_hwnd,
    panel_size, PanelTarget, PasteExecutionError,
};

#[cfg(test)]
pub(crate) use lifecycle::IntegrityLevel;

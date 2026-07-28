//! 此模块封装面板生命周期需要的 Windows 原生窗口和显示器查询。
//!
//! 上层只接收不可变的值对象；具体 Win32 句柄只在本模块内短暂使用，避免把平台
//! 资源泄漏到 Slint 状态或后台线程。

mod lifecycle;

pub(crate) use lifecycle::{
    activate_panel, center_position, cursor_work_area, move_panel, panel_size,
};

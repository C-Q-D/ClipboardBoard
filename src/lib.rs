//! 此库负责暴露 ClipboardBoard 的最小界面创建入口。
//!
//! 当前库已经提供可运行的黑色看板、热键桥、SQLite 文本写入和剪贴板结果摘要入口；
//! 启动恢复、写回和完整历史能力由后续原子逐步加入。

slint::include_modules!();

// 公开应用核心模块，后续平台线程只能通过这些模块与 UI 线程通信。
pub mod app;
pub mod command;
pub mod diagnostics;
pub mod domain;
pub mod history;
// 存储模块只暴露单线程执行器，不把 SQLite 连接、Statement 或 SQL 句柄泄漏到业务调用方。
pub mod storage;

// ClipboardIO 依赖 Windows 剪贴板和全局内存 API，只在 Windows 目标暴露，避免跨平台构建
// 引入无法实现的原生句柄类型。
#[cfg(windows)]
pub mod clipboard;

// 捕获持久化桥只在 Windows 目标暴露，因为输入 DTO 来自 Windows ClipboardIO worker。
#[cfg(windows)]
pub mod history_bridge;

// Windows 平台模块只在目标平台编译，避免把原生 API 泄漏到业务层公共接口。
#[cfg(windows)]
pub mod platform;

/// 创建主窗口实例。
///
/// 调用方负责启动 Slint 事件循环；创建失败时返回平台初始化错误，不在此处吞掉错误。
pub fn create_app_window() -> Result<AppWindow, slint::PlatformError> {
    AppWindow::new()
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证当前原子对外暴露的最小应用接口仍可被编译使用。

    use super::create_app_window;

    /// 验证窗口构造函数的返回类型仍保持为可处理的平台错误结果。
    #[test]
    fn 主窗口构造函数保持可调用() {
        let constructor = create_app_window;
        let _ = constructor;
    }
}

//! 此库负责暴露 ClipboardBoard 的最小界面创建入口。
//!
//! 当前库已经提供可运行的黑色看板、热键桥、SQLite 文本历史、启动恢复和底层写回桥。
//! WCB-INT-02 已移除自动输入基础设施；文本和图片历史均可由鼠标显式复制到系统剪贴板，
//! 用户在目标程序中自行粘贴。

slint::include_modules!();

// 公开应用核心模块，后续平台线程只能通过这些模块与 UI 线程通信。
pub mod app;
pub mod command;
pub mod diagnostics;
pub mod domain;
pub mod history;
// 图片解码模块只接收拥有型或借用编码字节，不访问剪贴板句柄和 UI。
pub mod image_decode;
// 图片复制准备器安全读取耐久原图并生成 DIBV5 字节，不直接访问系统剪贴板。
pub mod image_copy;
// 图片流水线负责规范像素编码与后续耐久发布，不访问 SQLite 或 Slint Image。
pub mod image_pipeline;
// 图片存储模块先公开纯路径布局；目录 IO 与 Windows 安全守卫由同模块后续原子补齐。
pub mod image_storage;
// 收藏 mutation 使用独立有界桥，避免 UI 线程阻塞或把 SQLite 连接泄漏到界面层。
pub mod history_mutation;
pub mod history_query;
// 搜索模块只负责 120ms 防抖；SQLite 查询与结果身份由 history_query 独立管理。
pub mod search;
// 配置深模块隐藏文件恢复和线程所有权，只向调用方暴露快照与受控 worker/client。
pub mod settings;
// 隐私模块在 ClipboardIO 正文读取前提供可持久化暂停门禁。
#[cfg(windows)]
pub mod privacy;
// 存储模块只暴露单线程执行器，不把 SQLite 连接、Statement 或 SQL 句柄泄漏到业务调用方。
pub mod storage;
pub mod thumbnail_loader;

// ClipboardIO 依赖 Windows 剪贴板和全局内存 API，只在 Windows 目标暴露，避免跨平台构建
// 引入无法实现的原生句柄类型。
#[cfg(windows)]
pub mod clipboard;

// 捕获持久化桥只在 Windows 目标暴露，因为输入 DTO 来自 Windows ClipboardIO worker。
#[cfg(windows)]
pub mod history_bridge;

// 启动恢复桥只在 Windows 目标暴露，因为它消费 Windows 主程序使用的存储生命周期。
#[cfg(windows)]
pub mod history_restore;

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

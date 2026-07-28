//! 此模块定义跨线程传入 UI 层的拥有型命令和数据传输对象。
//!
//! 这里禁止携带借用引用、Slint 模型、窗口句柄或闭包，确保事件可以安全地进入
//! `slint::invoke_from_event_loop` 的 `Send + 'static` 闭包，并且不会把 UI 所有权泄漏给后台线程。

/// 单条剪贴板历史的最小 UI 展示数据。
///
/// 当前原子只保留能够证明跨线程传输安全的字段；完整剪贴板内容会在后续历史原子中扩展。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiClipboardItem {
    /// 历史记录的稳定标识，后续用于选择、删除和粘贴。
    pub id: u64,
    /// 展示给用户的纯文本预览。
    pub preview: String,
}

/// UI 一次性替换历史列表时使用的不可变快照。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiSnapshot {
    /// 当前需要展示的历史条目，顺序由上游历史服务决定。
    pub items: Vec<UiClipboardItem>,
    /// 当前选中项；为空表示列表没有可选记录。
    pub selected_index: Option<usize>,
}

/// 后台线程可以提交给 UI 线程的事件集合。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiEvent {
    /// 请求 UI 线程切换临时看板；全局热键使用该语义，重复按键会隐藏面板。
    OpenPanel,
    /// 请求 UI 线程幂等显示临时看板；托盘“打开”不能使用热键的切换语义。
    ShowPanel,
    /// 请求 UI 线程退出事件循环；退出只允许由 UI 线程触发，便于统一清理后台线程。
    Quit,
    /// 请求 UI 线程关闭指定一次打开操作对应的临时看板。
    ///
    /// 事件携带打开代次，避免旧的 Esc 或失焦事件误关闭后来重新打开的面板。
    HidePanel { generation: u64 },
    /// 用一个完整且拥有所有权的快照替换 UI 历史状态。
    ReplaceSnapshot(UiSnapshot),
}

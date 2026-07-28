//! 此模块定义跨线程传入 UI 层的拥有型命令和数据传输对象。
//!
//! 这里禁止携带借用引用、Slint 模型、窗口句柄或闭包，确保事件可以安全地进入
//! `slint::invoke_from_event_loop` 的 `Send + 'static` 闭包，并且不会把 UI 所有权泄漏给后台线程。

#[cfg(windows)]
use crate::clipboard::ClipboardCaptureResult;

/// 单条剪贴板历史的最小 UI 展示数据。
///
/// 当前原子只保留跨线程传输安全的摘要和轻量历史元数据；完整剪贴板内容仍不进入 UI DTO。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiClipboardItem {
    /// 历史记录的稳定标识，后续用于选择、删除和粘贴。
    pub id: u64,
    /// 展示给用户的纯文本预览。
    pub preview: String,
    /// 复制来源的稳定显示名称；来源查询失败时由转换层填充“未知来源”。
    pub source: String,
    /// 面向当前列表的相对时间文案；本原子只产生“刚刚”，历史刷新由后续原子负责。
    pub relative_time: String,
    /// 规范化文本哈希；历史协调器据此合并重复内容，不需要再次读取正文。
    pub content_hash: [u8; 32],
    /// 同一规范内容被复制的次数；由历史协调器做饱和递增。
    pub copy_count: u64,
    /// 用户是否收藏该记录；合并重复内容时必须保留旧值。
    pub is_pinned: bool,
}

impl UiClipboardItem {
    /// 将 ClipboardIO 的拥有型结果转换为不含完整正文的 UI 卡片数据。
    #[cfg(windows)]
    pub fn from_capture(capture: &ClipboardCaptureResult) -> Self {
        let summary = capture.payload.summary();
        let content_hash = summary.content_hash;
        let preview = summary.preview;
        let source = capture
            .source
            .as_ref()
            .map(|source| source.display_name.clone())
            .unwrap_or_else(|| "未知来源".to_owned());

        Self {
            id: capture.sequence as u64,
            preview,
            source,
            relative_time: "刚刚".to_owned(),
            content_hash,
            copy_count: 1,
            is_pinned: false,
        }
    }
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
    /// 将一条已转换为摘要的剪贴板记录交给 UI 线程置顶显示。
    ClipboardCaptured(UiClipboardItem),
}

#[cfg(all(test, windows))]
mod tests {
    //! 此测试模块验证 ClipboardIO 结果只转换为受限摘要、来源和时间文案。

    use super::UiClipboardItem;
    use crate::clipboard::{ClipboardCaptureRequest, ClipboardCaptureResult};
    use crate::domain::ClipboardPayload;
    use crate::platform::windows::ProcessSource;

    /// 转换层必须丢弃完整正文，只保留领域摘要和来源显示名。
    #[test]
    fn 捕获结果转换为轻量卡片() {
        let capture = ClipboardCaptureResult {
            sequence: 42,
            source: Some(ProcessSource {
                executable: "code.exe".to_owned(),
                display_name: "Code".to_owned(),
                process_id: 42,
            }),
            payload: ClipboardPayload::from_text("第一行\n第二行"),
        };

        let item = UiClipboardItem::from_capture(&capture);
        assert_eq!(item.id, 42);
        assert_eq!(item.preview, "第一行\n第二行");
        assert_eq!(item.source, "Code");
        assert_eq!(item.relative_time, "刚刚");
        assert_eq!(item.content_hash, capture.payload.summary().content_hash);
        assert_eq!(item.copy_count, 1);
        assert!(!item.is_pinned);
    }

    /// 来源查询失败时仍必须生成可显示的卡片，避免一次权限失败终止 UI 流程。
    #[test]
    fn 缺少来源时使用稳定回退文案() {
        let capture = ClipboardCaptureResult {
            sequence: 7,
            source: None,
            payload: ClipboardPayload::from_text("无来源文本"),
        };
        let item = UiClipboardItem::from_capture(&capture);
        assert_eq!(item.source, "未知来源");
    }

    /// 保证测试不会误把仅用于构造请求的类型当成 UI 正文模型的一部分。
    #[test]
    fn 捕获请求仍只携带序号和来源() {
        let request = ClipboardCaptureRequest::new(9, None);
        assert_eq!(request.sequence, 9);
        assert!(request.source.is_none());
    }
}

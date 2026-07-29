//! 此模块定义跨线程传入 UI 层的拥有型命令和数据传输对象。
//!
//! 这里禁止携带借用引用、Slint 模型、窗口句柄或闭包，确保事件可以安全地进入
//! `slint::invoke_from_event_loop` 的 `Send + 'static` 闭包，并且不会把 UI 所有权泄漏给后台线程。

#[cfg(windows)]
use crate::clipboard::ClipboardCaptureResult;
use crate::history_mutation::{
    ClearHistoryMutationResult, DeleteMutationResult, PinMutationResult,
};
#[cfg(windows)]
use crate::storage::TextUpsertResult;

/// 单条剪贴板历史的最小 UI 展示数据。
///
/// 当前原子只保留跨线程传输安全的摘要和轻量历史元数据；完整剪贴板内容仍不进入 UI DTO。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiClipboardItem {
    /// 历史记录的稳定标识，后续用于选择、删除和显式复制。
    pub id: u64,
    /// 展示给用户的纯文本预览。
    pub preview: String,
    /// 复制来源的稳定显示名称；来源查询失败时由转换层填充“未知来源”。
    pub source: String,
    /// 面向当前列表的相对时间文案；本原子只产生“刚刚”，历史刷新由后续原子负责。
    pub relative_time: String,
    /// 规范化文本哈希；历史协调器据此合并重复内容，不需要再次读取正文。
    pub content_hash: [u8; 32],
    /// 同一规范内容被复制的次数；捕获路径必须采用 SQLite 返回的最终饱和值。
    pub copy_count: u64,
    /// 用户是否收藏该记录；捕获路径必须采用 SQLite 返回的最终收藏状态。
    pub is_pinned: bool,
}

impl UiClipboardItem {
    /// 将旧的 ClipboardIO 拥有型结果转换为不含完整正文的 UI 卡片数据。
    ///
    /// 该方法仅供非持久化测试或恢复前路径使用；生产捕获必须使用
    /// [`Self::from_persisted_result`]，避免 UI 在 SQLite 之外猜测 ID 和计数。
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

    /// 将 SQLite 事务返回的最终快照转换为 UI 卡片；不再信任捕获前的临时序号或摘要。
    ///
    /// ID 和计数必须是可展示的正数，任何不满足约束的持久化 DTO 都返回 `None`，
    /// 由上层把它记录为转换错误，避免先写入数据库再制造幽灵 UI 记录。
    #[cfg(windows)]
    pub fn from_persisted_result(result: &TextUpsertResult) -> Option<Self> {
        let id = u64::try_from(result.id).ok()?;
        let copy_count = u64::try_from(result.copy_count).ok()?;
        if id == 0 || copy_count == 0 {
            return None;
        }

        let source = result
            .source_app
            .as_deref()
            .filter(|source| !source.is_empty())
            .or_else(|| {
                result
                    .source_exe
                    .as_deref()
                    .filter(|source| !source.is_empty())
            })
            .unwrap_or("未知来源")
            .to_owned();

        Some(Self {
            id,
            preview: result.preview_text.clone(),
            source,
            relative_time: "刚刚".to_owned(),
            content_hash: result.content_hash,
            copy_count,
            is_pinned: result.is_pinned,
        })
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

/// 看板当前支持的基础历史筛选；图片筛选要等图片历史原子完成后再启用。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SearchFilter {
    /// 不限制内容类型或收藏状态。
    #[default]
    All,
    /// 只显示当前已经支持的文本记录。
    Text,
    /// 只显示用户收藏的记录。
    Pinned,
}

impl SearchFilter {
    /// 将 UI 标签索引转换为受限枚举；未知值保守回退到“全部”。
    pub const fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Text,
            2 => Self::Pinned,
            _ => Self::All,
        }
    }

    /// 返回 Slint 标签使用的稳定索引，避免 UI 直接依赖 Rust 枚举布局。
    pub const fn as_index(self) -> i32 {
        match self {
            Self::All => 0,
            Self::Text => 1,
            Self::Pinned => 2,
        }
    }
}

/// 搜索结果的可观察状态；UI 必须区分加载中、空结果和错误，避免空列表造成歧义。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SearchStatus {
    /// 未输入关键词且尚未触发搜索。
    #[default]
    Idle,
    /// 已提交新代次，仍在等待防抖或结果应用。
    Loading,
    /// 当前筛选得到至少一条结果。
    Results,
    /// 当前筛选已经完成但没有匹配记录。
    Empty,
    /// 搜索代次无法分配等不可恢复状态；不展示底层错误详情。
    Error,
}

impl SearchStatus {
    /// 返回 Slint 使用的稳定状态标识；文案仍由 UI 负责排版。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Loading => "loading",
            Self::Results => "results",
            Self::Empty => "empty",
            Self::Error => "error",
        }
    }
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
    /// 将持久化捕获及其存储修订号交给 UI；修订号只用于清空前后顺序隔离。
    ClipboardCaptured {
        /// 已由 SQLite 最终快照转换且不含正文的 UI 摘要。
        item: UiClipboardItem,
        /// 唯一存储线程为本次成功 upsert 分配的单调修订号。
        mutation_revision: u64,
    },
    /// 搜索框文本变化；完整正文只作为用户主动输入的查询词进入 UI 状态。
    SearchTextChanged(String),
    /// 搜索筛选标签变化；未知索引在 UI 回调边界转换为“全部”。
    SearchFilterChanged(SearchFilter),
    /// 防抖计时器到期事件；代次不匹配时必须丢弃，不能消费新请求。
    SearchDebounceElapsed { generation: u64 },
    /// SQLite 查询 worker 的无正文唤醒；UI 从绑定的 latest 结果槽提取真实结果。
    HistoryQueryWake,
    /// 收藏 worker 已完成一次事务；结果不携带正文且必须与活动 mutation 完整匹配。
    PinMutationCompleted(PinMutationResult),
    /// 删除 worker 已完成一次事务；DEL-02 只建立事件接缝，DEL-03 再消费并更新快照。
    DeleteMutationCompleted(DeleteMutationResult),
    /// 单一清空 worker 已完成一次显式范围事务；UI 依据 pending scope 消费结果。
    ClearHistoryMutationCompleted(ClearHistoryMutationResult),
    /// 历史列表真实视口几何变化；reducer 只在进入底部阈值的边沿请求续页。
    HistoryViewportChanged {
        /// Flickable 的负向纵向视口坐标。
        viewport_y: i32,
        /// 当前可见区域高度。
        visible_height: i32,
        /// 全部已加载卡片对应的内容高度。
        content_height: i32,
    },
    /// 用户点击固定失败提示后显式重试当前游标。
    RetryHistoryPage,
    /// 请求 UI reducer 按方向移动当前首批卡片的选中索引；只允许来自 UI 线程键盘回调。
    MoveSelection { delta: i32 },
    /// 请求选中一次点击时解析出的稳定记录身份；异步应用时必须重新校验面板代次和内容身份。
    SelectItem {
        /// 点击发生时的面板打开代次，旧会话事件必须被拒绝。
        panel_generation: u64,
        /// 点击发生时对应的持久化记录 ID。
        id: u64,
        /// 点击发生时对应的内容哈希，用于防止同一 ID 或索引被迟到事件误用。
        content_hash: [u8; 32],
    },
    /// 请求显式复制按钮按稳定身份写回某条记录；正文不进入事件。
    CopyItem {
        /// 按钮点击发生时的面板打开代次，旧会话事件必须被拒绝。
        panel_generation: u64,
        /// 按钮点击时对应的持久化记录 ID。
        id: u64,
        /// 按钮点击时对应的内容哈希，后台读取正文后还会再次复核。
        content_hash: [u8; 32],
    },
    /// 请求把卡片设置为明确收藏状态；UI reducer 会分配 mutation 令牌后再投递后台。
    PinItem {
        /// 点击发生时的面板代次。
        panel_generation: u64,
        /// 点击时对应的持久化记录 ID。
        id: u64,
        /// 点击时对应的固定内容哈希。
        content_hash: [u8; 32],
        /// 根据点击瞬间卡片状态计算出的明确期望状态。
        is_pinned: bool,
    },
    /// 请求删除一次点击时解析出的稳定记录身份；事务成功前 UI 不得移除卡片。
    DeleteItem {
        /// 点击发生时的面板打开代次。
        panel_generation: u64,
        /// 数据库稳定 ID。
        id: u64,
        /// 与 ID 共同校验的固定内容哈希。
        content_hash: [u8; 32],
    },
    /// 用户点击清理入口；该事件只打开确认区，不提交存储请求。
    ClearUnpinnedRequested,
    /// 用户取消清理确认；未提交请求时只关闭确认区。
    ClearUnpinnedCancelled,
    /// 用户确认清空未收藏文本；面板代次用于拒绝旧会话确认。
    ClearUnpinnedConfirmed {
        /// 确认发生时的面板打开代次。
        panel_generation: u64,
    },
}

#[cfg(all(test, windows))]
mod tests {
    //! 此测试模块验证 ClipboardIO 结果只转换为受限摘要、来源和时间文案。

    use super::UiClipboardItem;
    use crate::clipboard::{ClipboardCaptureRequest, ClipboardCaptureResult};
    use crate::domain::ClipboardPayload;
    use crate::platform::windows::ProcessSource;
    use crate::storage::TextUpsertResult;

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

    /// 持久化快照必须使用数据库最终字段，并按应用、可执行文件、未知来源依次回退。
    #[test]
    fn 持久化结果使用最终字段和来源优先级() {
        let result = TextUpsertResult {
            mutation_revision: 1,
            id: 9,
            content_hash: [3; 32],
            preview_text: "数据库预览".to_owned(),
            source_exe: Some("fallback.exe".to_owned()),
            source_app: Some("持久化应用".to_owned()),
            copy_count: 4,
            is_pinned: true,
            created_at: 10,
            copied_at: 20,
            last_used_at: Some(30),
        };

        let item = UiClipboardItem::from_persisted_result(&result).expect("有效 DTO 应可转换");
        assert_eq!(item.id, 9);
        assert_eq!(item.preview, "数据库预览");
        assert_eq!(item.source, "持久化应用");
        assert_eq!(item.copy_count, 4);
        assert!(item.is_pinned);
        assert_eq!(item.content_hash, [3; 32]);
        assert_eq!(item.relative_time, "刚刚");
    }

    /// ID 或计数不合法时必须拒绝 DTO，避免把不可追踪的数据送入 UI。
    #[test]
    fn 不可转换的持久化结果被拒绝() {
        let result = TextUpsertResult {
            mutation_revision: 1,
            id: -1,
            content_hash: [4; 32],
            preview_text: "错误 DTO".to_owned(),
            source_exe: None,
            source_app: None,
            copy_count: 1,
            is_pinned: false,
            created_at: 1,
            copied_at: 1,
            last_used_at: None,
        };

        assert!(UiClipboardItem::from_persisted_result(&result).is_none());
    }

    /// 保证测试不会误把仅用于构造请求的类型当成 UI 正文模型的一部分。
    #[test]
    fn 捕获请求仍只携带序号和来源() {
        let request = ClipboardCaptureRequest::new(9, None);
        assert_eq!(request.sequence, 9);
        assert!(request.source.is_none());
    }
}

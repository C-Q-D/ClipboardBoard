//! 此模块定义跨线程传入 UI 层的拥有型命令和数据传输对象。
//!
//! 这里禁止携带借用引用、Slint 模型、窗口句柄或闭包，确保事件可以安全地进入
//! `slint::invoke_from_event_loop` 的 `Send + 'static` 闭包，并且不会把 UI 所有权泄漏给后台线程。

#[cfg(windows)]
use crate::clipboard::{ClipboardCapturePayload, ClipboardCaptureResult};
use crate::history_mutation::{
    ClearHistoryMutationResult, DeleteMutationResult, PinMutationResult,
};
#[cfg(windows)]
use crate::storage::{ImageUpsertResult, TextUpsertResult};

/// 图片卡片在 UI 层使用的轻量定位和尺寸信息；不包含原图或缩略图像素。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiImageSummary {
    /// 受管图片根内缩略图的绝对定位；只交给后续受限加载器，不进入日志。
    pub thumbnail_path: std::path::PathBuf,
    /// 原图宽度。
    pub width: u32,
    /// 原图高度。
    pub height: u32,
}

/// UI 卡片的受限内容类型；文本和图片都能通过稳定身份请求后台复制。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UiClipboardItemKind {
    /// 可通过按钮写回剪贴板的文本。
    #[default]
    Text,
    /// 只展示摘要和缩略图的图片。
    Image(UiImageSummary),
}

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
    /// 条目类型及图片轻量定位；默认值保持既有文本测试兼容。
    pub kind: UiClipboardItemKind,
}

impl UiClipboardItem {
    /// 将旧的 ClipboardIO 拥有型结果转换为不含完整正文的 UI 卡片数据。
    ///
    /// 该方法仅供非持久化测试或恢复前路径使用；生产捕获必须使用
    /// [`Self::from_persisted_result`]，避免 UI 在 SQLite 之外猜测 ID 和计数。
    #[cfg(windows)]
    pub fn from_capture(capture: &ClipboardCaptureResult) -> Self {
        let ClipboardCapturePayload::Text(payload) = &capture.payload else {
            return Self::default();
        };
        let summary = payload.summary();
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
            kind: UiClipboardItemKind::Text,
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
            kind: UiClipboardItemKind::Text,
        })
    }

    /// 将图片事务最终快照转换为可复制的 UI 卡片摘要。
    #[cfg(windows)]
    pub fn from_persisted_image_result(result: &ImageUpsertResult) -> Option<Self> {
        let id = u64::try_from(result.id).ok()?;
        let copy_count = u64::try_from(result.copy_count).ok()?;
        if id == 0 || copy_count == 0 {
            return None;
        }
        let source = result
            .source_app
            .as_deref()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                result
                    .source_exe
                    .as_deref()
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or("未知来源")
            .to_owned();
        let metadata = &result.metadata;

        Some(Self {
            id,
            preview: result.preview_text.clone(),
            source,
            relative_time: "刚刚".to_owned(),
            content_hash: *metadata.content_hash(),
            copy_count,
            is_pinned: result.is_pinned,
            kind: UiClipboardItemKind::Image(UiImageSummary {
                thumbnail_path: result
                    .canonical_root
                    .join("thumbnail")
                    .join(metadata.thumbnail_path().as_path()),
                width: metadata.width().get(),
                height: metadata.height().get(),
            }),
        })
    }

    /// 返回当前卡片是否允许触发系统剪贴板写回。
    pub const fn copy_enabled(&self) -> bool {
        matches!(
            self.kind,
            UiClipboardItemKind::Text | UiClipboardItemKind::Image(_)
        )
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

/// 窗口化历史模型中的绝对定位和稳定身份；不携带 Slint 模型或剪贴板正文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowOffset {
    /// 完整数据集中的绝对索引。
    pub absolute_index: u64,
    /// 数据库稳定 ID。
    pub id: u64,
    /// 与 ID 共同组成稳定身份的内容哈希。
    pub content_hash: [u8; 32],
    /// 卡片在精确内容画布中的整数像素顶部。
    pub top: i64,
    /// 卡片外层整数像素高度。
    pub height: i64,
}

/// WindowCommitBuilder 的一次性窗口字段载荷。
///
/// 将几何、来源令牌和拥有型卡片放进单一 DTO，避免构造器调用方在多个位置
/// 依赖长参数列表而遗漏字段；`set_window` 仍会对所有字段执行统一校验。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowCommitPayload {
    /// 窗口起始绝对索引。
    pub start: u64,
    /// 完整逻辑数据集数量。
    pub total_count: u64,
    /// 精确前缀和总高度。
    pub total_height: i64,
    /// 当前 UI 可见区域高度。
    pub visible_height: i64,
    /// clamp 后的负向视口坐标。
    pub clamped_viewport_y: i64,
    /// 程序主动修正视口时使用的一次性来源 token。
    pub origin_token: Option<u64>,
    /// 当前有界窗口的摘要卡片。
    pub cards: Vec<UiClipboardItem>,
    /// 与卡片顺序一一对应的绝对定位和稳定身份。
    pub offsets: Vec<WindowOffset>,
}

/// 显式窗口卡片允许的有限操作；事件身份校验通过后才进入对应 reducer 分支。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowCardAction {
    /// 只更新当前选中项。
    Select,
    /// 请求把当前记录写回系统剪贴板。
    Copy,
    /// 请求设置明确的收藏状态，禁止使用隐式 toggle。
    Pin { is_pinned: bool },
    /// 请求删除当前记录。
    Delete,
}

/// 一次窗口模型发布的完整拥有型提交单元。
///
/// 所有字段在 `WindowCommitBuilder` 的 Ready 阶段一次性校验，UI 只能接受 checksum
/// 与 revision 同时匹配的 Published 提交，避免 cards 与 offsets 跨代次撕裂。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowCommit {
    /// 当前进程的不可持久化 session nonce。
    pub session_nonce: u128,
    /// 完整逻辑数据集修订号。
    pub dataset_revision: u64,
    /// 当前窗口修订号。
    pub window_revision: u64,
    /// 发布修订号；Published 时必须与 window_revision 相同。
    pub commit_revision: u64,
    /// 窗口起始绝对索引。
    pub start: u64,
    /// 窗口长度。
    pub length: u64,
    /// 完整逻辑数据集数量。
    pub total_count: u64,
    /// 精确前缀和总高度。
    pub total_height: i64,
    /// 当前 UI 可见区域高度。
    pub visible_height: i64,
    /// clamp 后的负向视口坐标。
    pub clamped_viewport_y: i64,
    /// 程序主动修正视口时使用的一次性来源 token。
    pub origin_token: Option<u64>,
    /// 当前有界窗口的摘要卡片，长度不超过 100。
    pub cards: Vec<UiClipboardItem>,
    /// 每张卡片的绝对定位和身份，顺序必须与 cards 一致。
    pub offsets: Vec<WindowOffset>,
    /// 固定小端 canonical descriptor 的 BLAKE3 校验和。
    pub commit_checksum: [u8; 32],
}

impl WindowCommit {
    /// 依据固定字段宽度生成 canonical descriptor，禁止使用平台相关 f32 bits。
    pub fn canonical_descriptor(&self) -> Vec<u8> {
        let mut descriptor = Vec::with_capacity(128 + self.offsets.len() * 88);
        descriptor.extend_from_slice(&self.session_nonce.to_le_bytes());
        descriptor.extend_from_slice(&self.dataset_revision.to_le_bytes());
        descriptor.extend_from_slice(&self.window_revision.to_le_bytes());
        descriptor.extend_from_slice(&self.commit_revision.to_le_bytes());
        descriptor.extend_from_slice(&self.start.to_le_bytes());
        descriptor.extend_from_slice(&self.length.to_le_bytes());
        descriptor.extend_from_slice(&self.total_count.to_le_bytes());
        descriptor.extend_from_slice(&self.total_height.to_le_bytes());
        descriptor.extend_from_slice(&self.visible_height.to_le_bytes());
        descriptor.extend_from_slice(&self.clamped_viewport_y.to_le_bytes());
        match self.origin_token {
            Some(token) => {
                descriptor.push(1);
                descriptor.extend_from_slice(&token.to_le_bytes());
            }
            None => descriptor.push(0),
        }
        for offset in &self.offsets {
            descriptor.extend_from_slice(&offset.absolute_index.to_le_bytes());
            descriptor.extend_from_slice(&offset.id.to_le_bytes());
            descriptor.extend_from_slice(&offset.content_hash);
            descriptor.extend_from_slice(&offset.top.to_le_bytes());
            descriptor.extend_from_slice(&offset.height.to_le_bytes());
        }
        descriptor
    }

    /// 计算当前字段的 BLAKE3 checksum。
    pub fn calculate_checksum(&self) -> [u8; 32] {
        *blake3::hash(&self.canonical_descriptor()).as_bytes()
    }

    /// 校验提交自身的字段、窗口边界和 checksum；失败时 fail-closed。
    pub fn validate(&self) -> bool {
        let Some(window_end) = self.start.checked_add(self.length) else {
            return false;
        };
        if self.session_nonce == 0
            || self.dataset_revision == 0
            || self.window_revision == 0
            || self.commit_revision != self.window_revision
            || self.length != self.cards.len() as u64
            || self.cards.len() != self.offsets.len()
            || self.cards.len() > 100
            || self.total_height < 0
            || self.visible_height < 0
            || self.total_count < window_end
        {
            return false;
        }
        let max_offset = self.total_height.saturating_sub(self.visible_height).max(0);
        let Some(negative_offset) = self.clamped_viewport_y.checked_neg() else {
            return false;
        };
        if self.clamped_viewport_y > 0 || negative_offset > max_offset {
            return false;
        }
        if self.length == 0 {
            if self.start != 0 || self.total_count != 0 || self.total_height != 0 {
                return false;
            }
        } else if self.total_count == 0 {
            return false;
        }
        let mut previous_end = 0_i64;
        for (position, (card, offset)) in self.cards.iter().zip(&self.offsets).enumerate() {
            let Some(expected_index) = self.start.checked_add(position as u64) else {
                return false;
            };
            if card.id != offset.id
                || card.content_hash != offset.content_hash
                || offset.height <= 0
                || offset.top < 0
                || offset.absolute_index != expected_index
                || (position > 0 && offset.top != previous_end)
            {
                return false;
            }
            let Some(offset_end) = offset.top.checked_add(offset.height) else {
                return false;
            };
            if offset_end > self.total_height {
                return false;
            }
            previous_end = offset_end;
        }
        self.calculate_checksum() == self.commit_checksum
    }

    /// 判断事件身份是否完整匹配当前 Published 提交。
    pub fn accepts_identity(&self, identity: &WindowEventIdentity) -> bool {
        self.validate()
            && identity.session_nonce == self.session_nonce
            && identity.dataset_revision == self.dataset_revision
            && identity.window_revision == self.window_revision
            && identity.commit_revision == self.commit_revision
            && identity.commit_checksum == self.commit_checksum
    }
}

/// 视口/卡片事件携带的最小提交身份；任一字段不匹配都必须丢弃。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowEventIdentity {
    /// 进程 session nonce。
    pub session_nonce: u128,
    /// 完整数据集修订号。
    pub dataset_revision: u64,
    /// 窗口修订号。
    pub window_revision: u64,
    /// 发布修订号。
    pub commit_revision: u64,
    /// Published checksum。
    pub commit_checksum: [u8; 32],
}

/// WindowCommit 的构建阶段，避免中间 setters 被 UI 当作已发布模型消费。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowCommitState {
    /// 尚未完成字段校验和 checksum 计算。
    Building,
    /// 字段和 checksum 已就绪，但尚未发布。
    Ready,
    /// checksum 已安装，事件可以消费。
    Published,
}

/// WindowCommit 的受限构造器；发布前不暴露半成品。
#[derive(Clone, Debug)]
pub struct WindowCommitBuilder {
    state: WindowCommitState,
    commit: WindowCommit,
}

impl WindowCommitBuilder {
    /// 创建 Building 状态，revision 和 session nonce 必须为非零。
    pub fn new(session_nonce: u128, dataset_revision: u64, window_revision: u64) -> Option<Self> {
        if session_nonce == 0 || dataset_revision == 0 || window_revision == 0 {
            return None;
        }
        Some(Self {
            state: WindowCommitState::Building,
            commit: WindowCommit {
                session_nonce,
                dataset_revision,
                window_revision,
                commit_revision: window_revision,
                start: 0,
                length: 0,
                total_count: 0,
                total_height: 0,
                visible_height: 0,
                clamped_viewport_y: 0,
                origin_token: None,
                cards: Vec::new(),
                offsets: Vec::new(),
                commit_checksum: [0; 32],
            },
        })
    }

    /// 设置所有窗口字段；Building 之外拒绝中间更新。
    pub fn set_window(&mut self, payload: WindowCommitPayload) -> bool {
        if self.state != WindowCommitState::Building
            || payload.cards.len() != payload.offsets.len()
            || payload.cards.len() > 100
        {
            return false;
        }
        self.commit.start = payload.start;
        self.commit.length = payload.cards.len() as u64;
        self.commit.total_count = payload.total_count;
        self.commit.total_height = payload.total_height;
        self.commit.visible_height = payload.visible_height;
        self.commit.clamped_viewport_y = payload.clamped_viewport_y;
        self.commit.origin_token = payload.origin_token;
        self.commit.cards = payload.cards;
        self.commit.offsets = payload.offsets;
        true
    }

    /// 进入 Ready 阶段并计算 checksum；非法几何直接 fail-closed。
    pub fn ready(&mut self) -> bool {
        if self.state != WindowCommitState::Building {
            return false;
        }
        self.commit.commit_checksum = self.commit.calculate_checksum();
        if !self.commit.validate() {
            return false;
        }
        self.state = WindowCommitState::Ready;
        true
    }

    /// 单一发布闩锁；只有 Ready 能进入 Published。
    pub fn publish_commit_stamp(&mut self) -> Option<WindowCommit> {
        if self.state != WindowCommitState::Ready {
            return None;
        }
        self.state = WindowCommitState::Published;
        Some(self.commit.clone())
    }

    /// 返回当前阶段。
    pub const fn state(&self) -> WindowCommitState {
        self.state
    }
}

/// 看板当前支持的受限历史筛选；UI 索引只能转换成这里声明的固定查询类型。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SearchFilter {
    /// 不限制内容类型或收藏状态。
    #[default]
    All,
    /// 只显示文本记录。
    Text,
    /// 只显示图片记录。
    Image,
    /// 只显示用户收藏的记录。
    Pinned,
}

impl SearchFilter {
    /// 将 UI 标签索引转换为受限枚举；未知值保守回退到“全部”。
    pub const fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Text,
            2 => Self::Image,
            3 => Self::Pinned,
            _ => Self::All,
        }
    }

    /// 返回 Slint 标签使用的稳定索引，避免 UI 直接依赖 Rust 枚举布局。
    pub const fn as_index(self) -> i32 {
        match self {
            Self::All => 0,
            Self::Text => 1,
            Self::Image => 2,
            Self::Pinned => 3,
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
    /// 请求 UI 线程幂等显示临时看板；托盘和二次启动不能使用热键的切换语义。
    ShowPanel,
    /// 请求 UI 线程退出事件循环；退出只允许由 UI 线程触发，便于统一清理后台线程。
    Quit,
    /// 开机启动命令完成后的稳定状态反馈；只携带枚举，不携带路径或原始错误。
    #[cfg(windows)]
    StartupStatus {
        /// 启动设置 owner 分配的单调事务身份，用于拒绝迟到回执。
        transaction_id: std::num::NonZeroU64,
        /// 命令代次，用于拒绝旧会话回执。
        generation: u64,
        /// 不含路径和底层错误正文的稳定结果。
        kind: crate::platform::windows::startup::StartupResultKind,
    },
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
    /// 后台缩略图线程完成一次受限像素读取；UI 线程必须重新校验面板代次和卡片身份。
    ThumbnailLoaded(crate::thumbnail_loader::ThumbnailLoadResult),
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
    /// Append 绑定等待期间捕获的旧几何通知；只更新缩略图视口，不参与分页边沿。
    HistoryViewportChangedDuringAppend {
        /// 有可投递探针时携带修订；`None` 表示修订耗尽但绑定门禁仍有效。
        append_revision: Option<u64>,
        /// Flickable 的负向纵向视口坐标。
        viewport_y: i32,
        /// 当前可见区域高度。
        visible_height: i32,
        /// 事件产生时的内容高度；绑定后探针会读取最终值。
        content_height: i32,
    },
    /// 续页模型完成绑定和视口恢复后的单次几何探针；只接受当前待处理追加修订。
    HistoryPostAppendProbe {
        /// Append 接受时分配的单调修订号，用于拒绝迟到或重复探针。
        append_revision: u64,
        /// 模型绑定并恢复后的 Flickable 负向纵向视口坐标。
        viewport_y: i32,
        /// 状态区稳定后的当前可见区域高度。
        visible_height: i32,
        /// 新模型全部混合卡片对应的真实内容高度。
        content_height: i32,
    },
    /// 显式几何 Flickable 的带提交身份视口事件；旧 ListView 事件仍保留给 legacy 适配器。
    HistoryWindowViewportChanged {
        /// session、dataset、window、commit revision 与 checksum 的完整身份。
        identity: WindowEventIdentity,
        /// 原始负向视口坐标，应用层会再次 clamp。
        viewport_y: i64,
        /// 当前可见区域整数高度。
        visible_height: i64,
        /// 回调携带的 origin token；用户手势必须为 None。
        origin_token: Option<u64>,
    },
    /// 显式窗口卡片操作事件；local index 不能单独决定目标记录。
    HistoryWindowCardRequested {
        /// 当前 Published WindowCommit 身份。
        identity: WindowEventIdentity,
        /// 窗口内对应的绝对索引。
        absolute_index: u64,
        /// 稳定数据库 ID。
        id: u64,
        /// 与 ID 共同校验的内容哈希。
        content_hash: [u8; 32],
        /// 通过身份校验后要执行的有限卡片操作。
        action: WindowCardAction,
    },
    /// 用户点击固定失败提示后显式重试当前游标。
    RetryHistoryPage,
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
    /// 用户点击带文字的清空全部危险入口；该事件只打开强确认区。
    ClearAllRequested,
    /// 强确认输入变化；只保存固定短语输入，不访问存储。
    ClearAllConfirmationTextChanged(String),
    /// 用户取消清空全部强确认；不得产生后台请求。
    ClearAllCancelled,
    /// 用户尝试确认清空全部；reducer 必须再次精确校验输入文字。
    ClearAllConfirmed {
        /// 确认发生时的面板打开代次。
        panel_generation: u64,
        /// 确认点击时输入框的完整值；不得自动 trim 或宽松匹配。
        confirmation_text: String,
    },
}

#[cfg(all(test, windows))]
mod tests {
    //! 此测试模块验证 ClipboardIO 结果只转换为受限摘要、来源和时间文案。

    use super::{SearchFilter, UiClipboardItem};
    use crate::clipboard::{
        ClipboardCapturePayload, ClipboardCaptureRequest, ClipboardCaptureResult,
    };
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
            payload: ClipboardPayload::from_text("第一行\n第二行").into(),
        };

        let item = UiClipboardItem::from_capture(&capture);
        assert_eq!(item.id, 42);
        assert_eq!(item.preview, "第一行\n第二行");
        assert_eq!(item.source, "Code");
        assert_eq!(item.relative_time, "刚刚");
        let ClipboardCapturePayload::Text(payload) = &capture.payload else {
            panic!("测试捕获应为文本");
        };
        assert_eq!(item.content_hash, payload.summary().content_hash);
        assert_eq!(item.copy_count, 1);
        assert!(!item.is_pinned);
    }

    /// 来源查询失败时仍必须生成可显示的卡片，避免一次权限失败终止 UI 流程。
    #[test]
    fn 缺少来源时使用稳定回退文案() {
        let capture = ClipboardCaptureResult {
            sequence: 7,
            source: None,
            payload: ClipboardPayload::from_text("无来源文本").into(),
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

    /// 四个标签使用稳定索引；负数和未来索引必须保守回退全部，不能构造任意查询类型。
    #[test]
    fn 搜索筛选索引只接受四个固定标签() {
        for (index, filter) in [
            (0, SearchFilter::All),
            (1, SearchFilter::Text),
            (2, SearchFilter::Image),
            (3, SearchFilter::Pinned),
        ] {
            assert_eq!(SearchFilter::from_index(index), filter);
            assert_eq!(filter.as_index(), index);
        }
        assert_eq!(SearchFilter::from_index(-1), SearchFilter::All);
        assert_eq!(SearchFilter::from_index(4), SearchFilter::All);
        assert_eq!(SearchFilter::from_index(i32::MAX), SearchFilter::All);
    }
}

#[cfg(test)]
mod window_commit_tests {
    //! WindowCommit checksum、状态闩锁和稳定身份隔离的窄测试。

    use super::{
        UiClipboardItem, UiClipboardItemKind, WindowCommitBuilder, WindowCommitPayload,
        WindowOffset,
    };

    fn card(id: u64) -> UiClipboardItem {
        UiClipboardItem {
            id,
            preview: format!("摘要-{id}"),
            source: "测试".to_owned(),
            relative_time: "刚刚".to_owned(),
            content_hash: [id as u8; 32],
            copy_count: 1,
            is_pinned: false,
            kind: UiClipboardItemKind::Text,
        }
    }

    fn commit() -> super::WindowCommit {
        let mut builder = WindowCommitBuilder::new(9, 1, 1).unwrap();
        assert!(builder.set_window(WindowCommitPayload {
            start: 0,
            total_count: 1,
            total_height: 106,
            visible_height: 50,
            clamped_viewport_y: 0,
            origin_token: None,
            cards: vec![card(1)],
            offsets: vec![WindowOffset {
                absolute_index: 0,
                id: 1,
                content_hash: [1; 32],
                top: 0,
                height: 106,
            }],
        }));
        assert!(builder.ready());
        builder.publish_commit_stamp().unwrap()
    }

    #[test]
    fn canonical_checksum_逐字段篡改即失效() {
        let original = commit();
        assert!(original.validate());
        let mut changed = original.clone();
        changed.offsets[0].top = 1;
        assert!(!changed.validate());
        assert_ne!(
            original.canonical_descriptor(),
            changed.canonical_descriptor()
        );
    }

    /// 即使攻击者重新计算 checksum，窗口内部出现 gap 或越过总高度也必须拒绝。
    #[test]
    fn 几何窗口必须连续且落在总高度内() {
        let mut builder = WindowCommitBuilder::new(9, 1, 1).unwrap();
        assert!(builder.set_window(WindowCommitPayload {
            start: 0,
            total_count: 2,
            total_height: 212,
            visible_height: 50,
            clamped_viewport_y: 0,
            origin_token: None,
            cards: vec![card(1), card(2)],
            offsets: vec![
                WindowOffset {
                    absolute_index: 0,
                    id: 1,
                    content_hash: [1; 32],
                    top: 0,
                    height: 106,
                },
                WindowOffset {
                    absolute_index: 1,
                    id: 2,
                    content_hash: [2; 32],
                    top: 106,
                    height: 106,
                },
            ],
        }));
        assert!(builder.ready());
        let mut valid = builder.publish_commit_stamp().unwrap();
        assert!(valid.validate());

        valid.offsets[1].top = 107;
        valid.commit_checksum = valid.calculate_checksum();
        assert!(!valid.validate());

        valid.offsets[1].top = 106;
        valid.offsets[1].height = 107;
        valid.commit_checksum = valid.calculate_checksum();
        assert!(!valid.validate());
    }

    #[test]
    fn building_ready_published_只能单向推进() {
        let mut builder = WindowCommitBuilder::new(9, 1, 1).unwrap();
        assert_eq!(builder.state(), super::WindowCommitState::Building);
        assert!(builder.publish_commit_stamp().is_none());
        assert!(builder.set_window(WindowCommitPayload {
            start: 0,
            total_count: 0,
            total_height: 0,
            visible_height: 50,
            clamped_viewport_y: 0,
            origin_token: None,
            cards: Vec::new(),
            offsets: Vec::new(),
        }));
        assert!(builder.ready());
        assert_eq!(builder.state(), super::WindowCommitState::Ready);
        assert!(builder.publish_commit_stamp().is_some());
        assert_eq!(builder.state(), super::WindowCommitState::Published);
        assert!(!builder.set_window(WindowCommitPayload {
            start: 0,
            total_count: 0,
            total_height: 0,
            visible_height: 50,
            clamped_viewport_y: 0,
            origin_token: None,
            cards: Vec::new(),
            offsets: Vec::new(),
        }));
        assert!(builder.publish_commit_stamp().is_none());
    }
}

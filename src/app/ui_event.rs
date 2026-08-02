//! 此模块提供唯一的 UI 事件投递入口，并将可变 UI 状态限制在事件循环线程。
//!
//! `thread_local!` 是这里的刻意选择：它让后台线程拿不到 UI 状态的可变引用，
//! 只有 `invoke_from_event_loop` 执行的闭包才会触碰 reducer。窗口显示、隐藏、位置和
//! 原生窗口定位也必须在这个 UI 线程闭包内完成，避免原生消息线程直接碰 Slint 对象。

use crate::app::history_geometry::{HistoryGeometry, HistoryGeometryItem};
#[cfg(windows)]
use crate::clipboard::{ClipboardCaptureInbox, ClipboardCopyRequest};
use crate::command::{
    SearchFilter, SearchStatus, UiClipboardItem, UiEvent, UiSnapshot, WindowCardAction,
    WindowCommit, WindowCommitBuilder, WindowCommitPayload, WindowEventIdentity, WindowOffset,
};
use crate::history::MemoryHistory;
use crate::history_mutation::{
    ClearHistoryMutationRequest, ClearHistoryMutationResult, ClearHistoryMutationSender,
    ClearHistoryScope, DeleteMutationFailure, DeleteMutationRequest, DeleteMutationResult,
    DeleteMutationSender, PinMutationRequest, PinMutationResult, PinMutationSender,
};
use crate::history_query::{
    HistoryPageApplication, HistoryPageCoordinator, HistoryPageCoordinatorError,
    HistoryPageRequest, HistoryPageResult, HistoryPerformanceSnapshot, HistoryRequestSender,
    HistoryResultReceiver, MAX_LOADED_ITEMS,
};
use crate::search::{SearchCoordinator, SearchCoordinatorError};
use crate::storage::HistoryQuery;
use crate::thumbnail_loader::{ThumbnailLoadRequest, ThumbnailLoadResult, ThumbnailLoaderSender};
use crate::AppWindow;
use slint::{
    CloseRequestResponse, ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer,
    SharedString, VecModel,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(not(windows))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, ThreadId};
#[cfg(windows)]
use std::time::Duration;
use std::time::Instant;

#[cfg(windows)]
use crate::platform::windows::window::{
    activate_panel, apply_panel_icon, center_position, cursor_work_area, move_panel, panel_size,
    reassert_panel_topmost,
};
#[cfg(windows)]
use slint::PhysicalPosition;

/// 启动恢复缓存与分页数据集共享 2,000 条摘要上限；完整正文不进入该缓存。
pub const UI_HISTORY_MEMORY_CAPACITY: usize = MAX_LOADED_ITEMS;
/// SQLite 首页固定批量。
pub const UI_FIRST_BATCH_SIZE: usize = 30;
/// 文本历史项固定外层高度；必须与 Theme.history-text-row-height 的 78px 保持一致。
const TEXT_HISTORY_ROW_HEIGHT: i32 = 78;
/// 图片历史项固定外层高度；必须与 Theme.history-image-row-height 的 92px 保持一致。
const IMAGE_HISTORY_ROW_HEIGHT: i32 = 92;
/// 文本和图片中较高的固定行高；续页阈值必须按最高项计算而不是取三分之二。
const MAX_HISTORY_ROW_HEIGHT: i32 = if TEXT_HISTORY_ROW_HEIGHT > IMAGE_HISTORY_ROW_HEIGHT {
    TEXT_HISTORY_ROW_HEIGHT
} else {
    IMAGE_HISTORY_ROW_HEIGHT
};
/// 距离底部两张最高卡片以内进入续页区，明确为 max(78, 92)×2 = 184。
const HISTORY_BOTTOM_ENTER_THRESHOLD: i32 = MAX_HISTORY_ROW_HEIGHT * 2;
/// 离开阈值为 max(78, 92)×3 = 276，吸收 Slint 多属性布局回调抖动。
const HISTORY_BOTTOM_EXIT_THRESHOLD: i32 = MAX_HISTORY_ROW_HEIGHT * 3;
/// 视口前后各保留十条卡片；范围按卡片条目计算而不是按像素猜测。
const THUMBNAIL_ITEM_BUFFER: usize = 10;
/// UI 纹理和失败占位的硬容量上限；滚动大量图片时仍保持有界。
const THUMBNAIL_CACHE_CAPACITY: usize = 500;
/// Slint f32 length 可无损承载的连续整数范围；超出范围直接关闭显式几何提交。
const MAX_EXACT_SLINT_INTEGER: i64 = 1_i64 << 24;
/// 清空全部强确认必须逐字匹配的固定短语；不做 trim 或大小写等宽松处理。
const CLEAR_ALL_CONFIRMATION_PHRASE: &str = "清空全部";

/// reducer 应用事件后交给 UI 窗口的最小副作用集合。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UiAction {
    /// 显示面板，并在显示前完成目标快照与工作区定位。
    Show,
    /// 面板已经可见时重新显示并申请激活，不创建新会话或重置任何状态。
    Reassert,
    /// 鼠标已经选择一张当前卡片，只同步选中视觉，不改变列表视口。
    SelectItem,
    /// 请求将按钮身份对应的记录排入后台复制邮箱；不隐藏面板也不重建模型。
    QueueCopy { id: u64, content_hash: [u8; 32] },
    /// 请求把完整稳定身份投递到收藏单槽；提交失败由事件入口回写固定错误状态。
    QueuePin(PinMutationRequest),
    /// 请求把完整稳定身份投递到删除单槽；事务成功前卡片保持可见。
    QueueDelete(DeleteMutationRequest),
    /// 请求把无正文清空身份投递到清空单槽；事务成功前卡片保持可见。
    QueueClearHistory(ClearHistoryMutationRequest),
    /// 防抖协调器已经接收新查询；由 UI 线程安排一个代次绑定的计时器。
    ScheduleSearch { generation: u64 },
    /// 隐藏面板；实际调用必须仍在 UI 线程执行。
    Hide,
    /// 该事件只改变数据或已经过期，不触发窗口副作用。
    None,
    /// 退出 Slint 事件循环；只允许第一次 Quit 事件触发。
    Quit,
}

/// 单次历史结果对窗口模型的最小刷新语义。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HistoryModelRefresh {
    /// 结果被拒绝或失败，窗口模型保持原样。
    None,
    /// 首页替换为新数据集，允许既有选择滚入逻辑定位首项。
    Replace,
    /// 续页追加保留旧视口；修订耗尽时没有绑定后探针。
    AppendPreservingViewport { append_revision: Option<u64> },
}

/// Append 模型绑定期间的独立门禁；修订耗尽仍必须冻结旧布局回调。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppendBindingGate {
    /// 当前没有 Append 模型等待绑定。
    Idle,
    /// 绑定完成后允许投递一次携带修订的真实几何探针。
    ProbePending(u64),
    /// 修订号已经耗尽；只冻结绑定期间回调，绑定完成后不自动探测。
    RevisionExhausted,
}

/// 为每个进程生成不可持久化的非零窗口会话 nonce；系统 RNG 失败时关闭显式窗口协议。
fn new_session_nonce() -> Option<u128> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::Cryptography::{
            BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        };
        let mut bytes = [0_u8; 16];
        // SAFETY: 缓冲区由本函数独占且长度与 API 参数一致，系统 RNG 不保留指针。
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status == 0 {
            let value = u128::from_le_bytes(bytes);
            if value != 0 {
                return Some(value);
            }
        }
        // RNG 失败时关闭显式窗口协议；调用方会继续使用 legacy 摘要路径。
        None
    }
    #[cfg(not(windows))]
    {
        // 非 Windows 测试后端没有 BCrypt，进程内计数器仍保证每次新会话不同。
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
        Some(0xCB43_52A7_2026_0801_0000_0000_0000_0000_u128 | counter)
    }
}

/// 单次原生定位与激活尝试的有限结果；调用方据此决定重试或只记录固定诊断。
#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivationAttempt {
    /// 原生窗口已定位且 Windows 接受激活请求。
    Done,
    /// HWND 尚未就绪或激活暂时被拒绝，剩余预算允许再次尝试。
    Retry,
    /// 重试预算已经耗尽且 topmost 断言仍失败，只记录固定诊断并保持面板状态。
    TopmostRejected,
    /// 重试预算已经耗尽且激活仍被拒绝，只记录诊断并保持面板状态。
    ActivationRejected,
}

/// 执行显示副作用并仅在成功时安排激活；函数不接收 UI 状态，因此失败不能回滚会话。
fn perform_show_action<E, S, A>(show: S, schedule_activation: A) -> Result<(), E>
where
    S: FnOnce() -> Result<(), E>,
    A: FnOnce(),
{
    show()?;
    schedule_activation();
    Ok(())
}

/// 执行一次可注入的定位和激活尝试，隔离 Win32 失败与 UI reducer 状态。
#[cfg(windows)]
fn activation_attempt<P, A>(position: P, activate: A, remaining_attempts: u8) -> ActivationAttempt
where
    P: FnOnce() -> bool,
    A: FnOnce() -> bool,
{
    let positioned = position();
    let activated = activate();
    if positioned && activated {
        ActivationAttempt::Done
    } else if remaining_attempts > 0 {
        ActivationAttempt::Retry
    } else if !positioned {
        ActivationAttempt::TopmostRejected
    } else if !activated {
        ActivationAttempt::ActivationRejected
    } else {
        // 前面的穷尽分支已经覆盖所有失败组合。
        ActivationAttempt::Done
    }
}

/// UI 线程独占的内部状态，外部线程不能直接取得其实例或引用。
struct UiState {
    snapshot: UiSnapshot,
    /// 当前筛选数据集的显式混合高度 prefix-sum；为空时仍保持 legacy set_cards 路径。
    history_geometry: Option<HistoryGeometry>,
    /// 进程内不可持久化会话 nonce；旧进程窗口事件不得重放到当前状态。
    session_nonce: Option<u128>,
    /// 数据集和窗口提交的单调修订号，溢出时 fail-closed。
    dataset_revision: u64,
    window_revision: u64,
    /// 程序主动 clamp 视口使用的 checked 来源 token。
    next_origin_token: u64,
    /// 最近一次完整发布的窗口提交；显式事件只接受该快照身份。
    published_window: Option<WindowCommit>,
    /// 当前程序 clamp 产生的一次性 origin token；用户滚动不会错误清除它。
    pending_origin_token: Option<u64>,
    /// 当前显式几何视口；负向坐标在事件入口先被 clamp。
    history_viewport_y: i64,
    /// 当前可见区域的整数高度；测试后端未布局时使用稳定默认值。
    history_visible_height: i64,
    /// 历史顺序和重复合并由独立协调器维护，UI 状态只同步其摘要快照。
    history: MemoryHistory,
    /// UI 线程独占的防抖协调器；它只负责查询代次，不访问 SQLite 或 Slint。
    search: SearchCoordinator,
    /// 当前搜索框原始输入；结果匹配时使用去除首尾空白后的副本。
    search_text: String,
    /// 当前基础筛选标签；只能取全部、文本、图片或收藏四个受限值。
    search_filter: SearchFilter,
    /// 当前搜索结果状态，供 UI 区分加载中、空结果和错误。
    search_status: SearchStatus,
    /// 当前启动设置的稳定反馈文案；不保存路径或底层错误正文。
    startup_status: String,
    /// 最近一次启动设置反馈的身份；旧事务/代次回执必须被拒绝。
    #[cfg(windows)]
    startup_status_identity: Option<(u64, u64)>,
    /// 最近一次搜索提交代次；用于验证迟到计时器身份。
    search_generation: Option<u64>,
    /// SQLite 数据集和单次请求身份协调器；只由 UI 线程修改。
    history_pages: HistoryPageCoordinator,
    /// 本次 reducer 事件产生的最新后台请求；事件入口会在离开状态借用后提交。
    pending_history_request: Option<HistoryPageRequest>,
    /// 上一次几何通知是否位于底部阈值内，用于检测 outside→inside 边沿。
    history_was_near_bottom: bool,
    /// 续页已经签发且尚未收口；首页加载不使用该状态。
    history_next_page_loading: bool,
    /// 最近分配的追加修订号；仅通过检查加法推进，耗尽后禁止回绕。
    next_append_revision: u64,
    /// Append 绑定门禁独立于可投递修订；耗尽状态也必须隔离旧布局回调。
    append_binding_gate: AppendBindingGate,
    /// 当前唯一在途收藏请求；隐藏面板不会取消已经接受的持久化事务。
    pending_pin_mutation: Option<PinMutationRequest>,
    /// 下一次收藏请求使用的单调令牌；耗尽时拒绝新请求而不回绕。
    next_pin_mutation_token: u64,
    /// 固定收藏失败提示的可见状态，不保存底层错误详情。
    pin_error_visible: bool,
    /// 当前唯一在途删除请求；隐藏面板不会取消已经接受的持久化事务。
    pending_delete_mutation: Option<DeleteMutationRequest>,
    /// 下一次删除请求使用的单调令牌；耗尽时拒绝新请求而不回绕。
    next_delete_mutation_token: u64,
    /// 固定删除失败提示的可见状态，不保存底层错误详情。
    delete_error_visible: bool,
    /// 清空确认区是否可见；首次点击只打开该区域，不访问存储。
    clear_unpinned_confirmation_visible: bool,
    /// 当前唯一在途清空请求；隐藏面板不能取消已经接受的事务。
    pending_clear_history_mutation: Option<ClearHistoryMutationRequest>,
    /// 下一次清空请求使用的单调令牌；耗尽时拒绝而不回绕。
    next_clear_history_mutation_token: u64,
    /// 固定清空失败提示的可见状态，不保存底层错误详情。
    clear_unpinned_error_visible: bool,
    /// 清空全部强确认区是否可见；危险入口只打开该区域。
    clear_all_confirmation_visible: bool,
    /// 用户在强确认输入框中的原始文本；只允许精确匹配固定短语。
    clear_all_confirmation_text: String,
    /// 固定清空全部失败提示的可见状态，不保存底层错误详情。
    clear_all_error_visible: bool,
    /// 已成功消费的最大清空修订号；进程生命周期内只增不减。
    active_clear_revision: u64,
    /// 捕获稳定身份到存储修订号的有界旁路索引；查询结果不得覆盖该顺序证据。
    capture_revisions: HashMap<(u64, [u8; 32]), u64>,
    /// 清空在途期间的有界捕获账本；筛选查询不得抹掉事务前后判断所需的 DTO 与修订号。
    pending_clear_captures: Vec<(UiClipboardItem, u64)>,
    panel_visible: bool,
    /// 只有从隐藏进入可见的新会话才递增，用来隔离旧的 Esc 事件。
    panel_generation: u64,
    /// 退出请求的一次性闩锁；置位后拒绝所有后续 UI 事件。
    quitting: bool,
    applied_event_count: u64,
    applied_on_thread: Option<ThreadId>,
}

impl Default for UiState {
    /// 使用与窗口分页一致的 2,000 条摘要上限初始化 UI 状态。
    fn default() -> Self {
        Self {
            snapshot: UiSnapshot::default(),
            history_geometry: None,
            session_nonce: new_session_nonce(),
            dataset_revision: 1,
            window_revision: 1,
            next_origin_token: 0,
            published_window: None,
            pending_origin_token: None,
            history_viewport_y: 0,
            history_visible_height: 500,
            history: MemoryHistory::new(UI_HISTORY_MEMORY_CAPACITY),
            search: SearchCoordinator::default(),
            search_text: String::new(),
            search_filter: SearchFilter::All,
            search_status: SearchStatus::Idle,
            startup_status: String::new(),
            #[cfg(windows)]
            startup_status_identity: None,
            search_generation: None,
            history_pages: HistoryPageCoordinator::default(),
            pending_history_request: None,
            history_was_near_bottom: false,
            history_next_page_loading: false,
            next_append_revision: 0,
            append_binding_gate: AppendBindingGate::Idle,
            pending_pin_mutation: None,
            next_pin_mutation_token: 1,
            pin_error_visible: false,
            pending_delete_mutation: None,
            next_delete_mutation_token: 1,
            delete_error_visible: false,
            clear_unpinned_confirmation_visible: false,
            pending_clear_history_mutation: None,
            next_clear_history_mutation_token: 1,
            clear_unpinned_error_visible: false,
            clear_all_confirmation_visible: false,
            clear_all_confirmation_text: String::new(),
            clear_all_error_visible: false,
            active_clear_revision: 0,
            capture_revisions: HashMap::new(),
            pending_clear_captures: Vec::new(),
            panel_visible: false,
            panel_generation: 0,
            quitting: false,
            applied_event_count: 0,
            applied_on_thread: None,
        }
    }
}

impl UiState {
    /// 在 UI 事件循环线程内应用一个事件并记录线程证据。
    fn apply(&mut self, event: UiEvent) -> UiAction {
        self.apply_at(event, Instant::now())
    }

    /// 在指定时间点应用 UI 事件；测试可注入时间而不真实等待防抖窗口。
    fn apply_at(&mut self, event: UiEvent, now: Instant) -> UiAction {
        self.applied_event_count += 1;
        self.applied_on_thread = Some(thread::current().id());

        // 退出后不再允许旧热键、托盘或后台结果改变 UI 状态，避免清理阶段重新打开窗口。
        if self.quitting {
            return UiAction::None;
        }

        match event {
            UiEvent::OpenPanel => {
                if self.panel_visible {
                    // 全局热键是显式开关：第二次按下必须与 Esc 一样完整收口当前会话。
                    self.hide_current_panel();
                    UiAction::Hide
                } else {
                    // 饱和递增保证长时间运行后不会回到零，从而避免旧事件碰巧匹配新代次。
                    self.panel_generation = self.panel_generation.saturating_add(1).max(1);
                    self.panel_visible = true;
                    self.reset_search_state();
                    self.select_first_if_needed();
                    self.begin_history_dataset(true);
                    UiAction::Show
                }
            }
            UiEvent::ShowPanel => {
                if self.panel_visible {
                    UiAction::Reassert
                } else {
                    self.panel_generation = self.panel_generation.saturating_add(1).max(1);
                    self.panel_visible = true;
                    self.reset_search_state();
                    self.select_first_if_needed();
                    self.begin_history_dataset(true);
                    UiAction::Show
                }
            }
            UiEvent::Quit => {
                self.quitting = true;
                self.panel_visible = false;
                self.search.cancel();
                self.history_pages.invalidate();
                self.pending_history_request = None;
                self.reset_history_scroll_dataset();
                UiAction::Quit
            }
            #[cfg(windows)]
            UiEvent::StartupStatus {
                transaction_id,
                generation,
                kind,
            } => {
                // 只接受更新身份的回执，禁止并发反馈线程把旧 Busy/Applied 文案覆盖新状态。
                let identity = (generation, transaction_id.get());
                if self
                    .startup_status_identity
                    .is_none_or(|previous| identity > previous)
                {
                    self.startup_status_identity = Some(identity);
                    self.startup_status = kind.ui_label().to_owned();
                }
                UiAction::None
            }
            UiEvent::HidePanel { generation } => {
                if self.panel_visible && generation == self.panel_generation {
                    self.hide_current_panel();
                    UiAction::Hide
                } else {
                    // 旧代次的 Esc 关闭事件只能被记录，不能关闭新一轮面板。
                    UiAction::None
                }
            }
            UiEvent::ReplaceSnapshot(snapshot) => {
                let selected_hash = snapshot
                    .selected_index
                    .map(|index| index.min(snapshot.items.len().saturating_sub(1)))
                    .and_then(|index| snapshot.items.get(index))
                    .map(|item| item.content_hash);
                self.history.replace(snapshot.items.clone());
                self.snapshot = snapshot;
                self.rebuild_visible_snapshot(selected_hash);
                UiAction::None
            }
            UiEvent::ClipboardCaptured {
                item,
                mutation_revision,
            } => {
                if mutation_revision < self.active_clear_revision {
                    // 捕获携带的是 upsert 提交时的收藏快照；其后可能已取消收藏并被清空。
                    // 因此所有早于清空事务的迟到捕获都必须拒绝，不能依赖旧 is_pinned 判断。
                    return UiAction::None;
                }
                let selected_hash = self
                    .snapshot
                    .selected_index
                    .and_then(|index| self.snapshot.items.get(index))
                    .map(|item| item.content_hash);
                // 捕获事件已经由 SQLite 返回最终快照，不能再走本地“旧值加一”的兼容路径。
                let identity = (item.id, item.content_hash);
                self.capture_revisions.insert(identity, mutation_revision);
                self.record_pending_clear_capture(&item, mutation_revision);
                self.history.record_persisted(item);
                self.prune_capture_revisions();
                // 捕获先推进数据集，立即屏蔽已经完成 SQLite 但尚未到 UI 的旧首页。
                // 当前筛选会被立即查询，因此取消尚未到期的同筛选防抖，避免重复首页。
                self.search.cancel();
                self.begin_history_dataset(self.panel_visible);
                if !self.panel_visible {
                    self.rebuild_visible_snapshot(selected_hash);
                }
                UiAction::None
            }
            UiEvent::SearchTextChanged(text) => self.begin_search(text, self.search_filter, now),
            UiEvent::SearchFilterChanged(filter) => {
                self.begin_search(self.search_text.clone(), filter, now)
            }
            UiEvent::SearchDebounceElapsed { generation } => {
                self.apply_search_if_current(generation, now)
            }
            UiEvent::HistoryQueryWake => UiAction::None,
            UiEvent::ThumbnailLoaded(_) => UiAction::None,
            UiEvent::PinMutationCompleted(result) => {
                self.apply_pin_mutation_result(result);
                UiAction::None
            }
            UiEvent::DeleteMutationCompleted(result) => {
                self.apply_delete_mutation_result(result);
                UiAction::None
            }
            UiEvent::ClearHistoryMutationCompleted(result) => {
                self.apply_clear_history_result(result);
                UiAction::None
            }
            UiEvent::HistoryViewportChanged {
                viewport_y,
                visible_height,
                content_height,
            } => {
                self.handle_history_viewport(viewport_y, visible_height, content_height);
                UiAction::None
            }
            UiEvent::HistoryViewportChangedDuringAppend { .. } => {
                // 事件在 pending 存在时已经冻结为旧布局通知，即使迟到也不能参与分页。
                UiAction::None
            }
            UiEvent::HistoryPostAppendProbe {
                append_revision,
                viewport_y,
                visible_height,
                content_height,
            } => {
                self.handle_post_append_probe(
                    append_revision,
                    viewport_y,
                    visible_height,
                    content_height,
                );
                UiAction::None
            }
            UiEvent::HistoryWindowViewportChanged {
                identity,
                viewport_y,
                visible_height,
                origin_token,
            } => {
                // 显式路径先验证完整提交身份；迟到窗口事件不得影响新的 prefix 快照。
                if self
                    .published_window
                    .as_ref()
                    .is_some_and(|window| window.accepts_identity(&identity))
                {
                    let Some(window) = self.published_window.as_ref() else {
                        return UiAction::None;
                    };
                    if let Some(token) = origin_token {
                        // 程序 clamp 回调只能消费当前提交尚未消费的一次性 token；
                        // 重复/迟到回调即使 checksum 相同也不得再次改变视口状态。
                        if window.origin_token != Some(token)
                            || self.pending_origin_token != Some(token)
                        {
                            return UiAction::None;
                        }
                        self.pending_origin_token = None;
                    }
                    if let (Ok(viewport_y), Ok(visible_height)) =
                        (i32::try_from(viewport_y), i32::try_from(visible_height))
                    {
                        self.handle_history_viewport(
                            viewport_y,
                            visible_height,
                            i32::try_from(
                                self.published_window
                                    .as_ref()
                                    .map(|window| window.total_height)
                                    .unwrap_or(0),
                            )
                            .unwrap_or(i32::MAX),
                        );
                    }
                }
                UiAction::None
            }
            UiEvent::HistoryWindowCardRequested {
                identity,
                absolute_index,
                id,
                content_hash,
                action,
            } => {
                let Some(window) = self.published_window.as_ref() else {
                    return UiAction::None;
                };
                if !window.accepts_identity(&identity) {
                    return UiAction::None;
                }
                let Some(local_index) = absolute_index.checked_sub(window.start) else {
                    return UiAction::None;
                };
                let Some(offset) = window.offsets.get(local_index as usize) else {
                    return UiAction::None;
                };
                if offset.absolute_index != absolute_index
                    || offset.id != id
                    || offset.content_hash != content_hash
                {
                    return UiAction::None;
                }
                let Some(item) = self.snapshot.items.get(absolute_index as usize) else {
                    return UiAction::None;
                };
                if item.id == id && item.content_hash == content_hash {
                    self.snapshot.selected_index = Some(absolute_index as usize);
                    match action {
                        WindowCardAction::Select => UiAction::SelectItem,
                        WindowCardAction::Copy if item.copy_enabled() => {
                            UiAction::QueueCopy { id, content_hash }
                        }
                        WindowCardAction::Copy => UiAction::None,
                        WindowCardAction::Pin { is_pinned } => self.begin_pin_mutation(
                            self.panel_generation,
                            id,
                            content_hash,
                            is_pinned,
                        ),
                        WindowCardAction::Delete => {
                            self.begin_delete_mutation(self.panel_generation, id, content_hash)
                        }
                    }
                } else {
                    UiAction::None
                }
            }
            UiEvent::RetryHistoryPage => {
                if self.panel_visible && self.history_pages.retry_required() {
                    self.history_pages.allow_retry();
                    self.request_next_history_page();
                }
                UiAction::None
            }
            UiEvent::SelectItem {
                panel_generation,
                id,
                content_hash,
            } => {
                // 点击事件跨过异步 UI 队列后，索引可能已经被搜索、捕获或重排复用。
                // 因此只接受仍属于当前面板代次、且当前首批列表中 ID/哈希同时匹配的身份。
                if !self.panel_visible || panel_generation != self.panel_generation {
                    return UiAction::None;
                }
                let Some(index) = self
                    .snapshot
                    .items
                    .iter()
                    .take(selection_limit(&self.snapshot))
                    .position(|item| item.id == id && item.content_hash == content_hash)
                else {
                    return UiAction::None;
                };
                self.snapshot.selected_index = Some(index);
                UiAction::SelectItem
            }
            UiEvent::CopyItem {
                panel_generation,
                id,
                content_hash,
            } => {
                // 按钮事件与选择事件使用相同身份门禁，但复制还会在后台按存储正文再次复核哈希。
                if !self.panel_visible || panel_generation != self.panel_generation {
                    return UiAction::None;
                }
                let Some(index) = self
                    .snapshot
                    .items
                    .iter()
                    .take(selection_limit(&self.snapshot))
                    .position(|item| item.id == id && item.content_hash == content_hash)
                else {
                    return UiAction::None;
                };
                if !self.snapshot.items[index].copy_enabled() {
                    return UiAction::None;
                }
                self.snapshot.selected_index = Some(index);
                UiAction::QueueCopy { id, content_hash }
            }
            UiEvent::PinItem {
                panel_generation,
                id,
                content_hash,
                is_pinned,
            } => self.begin_pin_mutation(panel_generation, id, content_hash, is_pinned),
            UiEvent::DeleteItem {
                panel_generation,
                id,
                content_hash,
            } => self.begin_delete_mutation(panel_generation, id, content_hash),
            UiEvent::ClearUnpinnedRequested => self.show_clear_unpinned_confirmation(),
            UiEvent::ClearUnpinnedCancelled => {
                if self.pending_clear_history_mutation.is_none() {
                    self.clear_unpinned_confirmation_visible = false;
                }
                UiAction::None
            }
            UiEvent::ClearUnpinnedConfirmed { panel_generation } => {
                self.begin_clear_unpinned_mutation(panel_generation)
            }
            UiEvent::ClearAllRequested => self.show_clear_all_confirmation(),
            UiEvent::ClearAllConfirmationTextChanged(text) => {
                if self.panel_visible
                    && self.clear_all_confirmation_visible
                    && self.pending_clear_history_mutation.is_none()
                {
                    self.clear_all_confirmation_text = text;
                }
                UiAction::None
            }
            UiEvent::ClearAllCancelled => {
                if self.pending_clear_history_mutation.is_none() {
                    self.clear_all_confirmation_visible = false;
                    self.clear_all_confirmation_text.clear();
                }
                UiAction::None
            }
            UiEvent::ClearAllConfirmed {
                panel_generation,
                confirmation_text,
            } => self.begin_clear_all_mutation(panel_generation, confirmation_text),
        }
    }

    /// 收口当前面板会话；Esc 与第二次全局热键必须复用完全相同的状态清理规则。
    fn hide_current_panel(&mut self) {
        self.panel_visible = false;
        self.clear_unpinned_confirmation_visible = false;
        self.clear_all_confirmation_visible = false;
        self.clear_all_confirmation_text.clear();
        self.search.cancel();
        self.history_pages.invalidate();
        self.pending_history_request = None;
        self.reset_history_scroll_dataset();
    }

    /// 首次显示副作用失败时回滚匹配代次，避免下一次热键把实际隐藏窗口误判为可见。
    fn mark_panel_show_failed(&mut self, generation: u64) {
        if self.panel_visible && self.panel_generation == generation {
            self.hide_current_panel();
        }
    }

    /// 校验卡片稳定身份并建立唯一在途收藏请求；卡片状态在此阶段保持不变。
    fn begin_pin_mutation(
        &mut self,
        panel_generation: u64,
        id: u64,
        content_hash: [u8; 32],
        is_pinned: bool,
    ) -> UiAction {
        if !self.panel_visible
            || panel_generation != self.panel_generation
            || self.pending_pin_mutation.is_some()
            || self.pending_delete_mutation.is_some()
            || self.pending_clear_history_mutation.is_some()
        {
            return UiAction::None;
        }
        let Some(item) = self
            .snapshot
            .items
            .iter()
            .find(|item| item.id == id && item.content_hash == content_hash)
        else {
            return UiAction::None;
        };
        if item.is_pinned == is_pinned || self.next_pin_mutation_token == u64::MAX {
            return UiAction::None;
        }

        let request = PinMutationRequest {
            mutation_token: self.next_pin_mutation_token,
            panel_generation,
            id,
            content_hash,
            is_pinned,
        };
        self.next_pin_mutation_token += 1;
        self.pending_pin_mutation = Some(request);
        self.pin_error_visible = false;
        UiAction::QueuePin(request)
    }

    /// 只接受与活动 mutation 五元身份完全一致的结果，并在提交成功后更新卡片。
    fn apply_pin_mutation_result(&mut self, result: PinMutationResult) {
        let Some(pending) = self.pending_pin_mutation else {
            return;
        };
        if result.mutation_token != pending.mutation_token
            || result.panel_generation != pending.panel_generation
            || result.id != pending.id
            || result.content_hash != pending.content_hash
            || result.is_pinned != pending.is_pinned
        {
            return;
        }

        self.pending_pin_mutation = None;
        match result.outcome {
            Ok(()) => {
                self.pin_error_visible = false;
                self.history
                    .set_pinned(result.id, result.content_hash, result.is_pinned);
                let selected_identity = self
                    .snapshot
                    .selected_index
                    .and_then(|index| self.snapshot.items.get(index))
                    .map(|item| (item.id, item.content_hash));
                if self.search_filter == SearchFilter::Pinned && !result.is_pinned {
                    // 收藏筛选下取消后立即移除，避免等待异步首页期间仍显示已取消记录。
                    self.snapshot.items.retain(|item| {
                        item.id != result.id || item.content_hash != result.content_hash
                    });
                } else if let Some(item) =
                    self.snapshot.items.iter_mut().find(|item| {
                        item.id == result.id && item.content_hash == result.content_hash
                    })
                {
                    // 直接更新当前 2,000 条快照，不能依赖可能属于另一筛选集合的缓存。
                    item.is_pinned = result.is_pinned;
                }
                self.snapshot.selected_index =
                    selected_identity.and_then(|(selected_id, selected_hash)| {
                        self.snapshot.items.iter().position(|item| {
                            item.id == selected_id && item.content_hash == selected_hash
                        })
                    });
                self.select_first_if_needed();
                self.search.cancel();
                // 先推进数据集使旧首页/续页失效，再按当前筛选查询 SQLite 最终状态。
                self.begin_history_dataset(self.panel_visible);
            }
            Err(_) => {
                // 所有失败共享固定提示；身份变化也推进数据集，防止用户重试陈旧卡片。
                self.pin_error_visible = true;
                if matches!(
                    result.outcome,
                    Err(crate::history_mutation::PinMutationFailure::IdentityChanged)
                ) {
                    self.search.cancel();
                    self.begin_history_dataset(self.panel_visible);
                }
            }
        }
    }

    /// 收藏请求未进入后台单槽时清除 pending，恢复按钮并显示固定失败提示。
    fn mark_pin_submission_failed(&mut self, request: &PinMutationRequest) {
        if self.pending_pin_mutation.as_ref() == Some(request) {
            self.pending_pin_mutation = None;
            self.pin_error_visible = true;
        }
    }

    /// 校验卡片稳定身份并建立唯一在途删除请求；事务成功前快照保持不变。
    fn begin_delete_mutation(
        &mut self,
        panel_generation: u64,
        id: u64,
        content_hash: [u8; 32],
    ) -> UiAction {
        if !self.panel_visible
            || panel_generation != self.panel_generation
            || self.pending_pin_mutation.is_some()
            || self.pending_delete_mutation.is_some()
            || self.pending_clear_history_mutation.is_some()
            || self.next_delete_mutation_token == u64::MAX
        {
            return UiAction::None;
        }
        if !self
            .snapshot
            .items
            .iter()
            .any(|item| item.id == id && item.content_hash == content_hash)
        {
            return UiAction::None;
        }

        let request = DeleteMutationRequest {
            mutation_token: self.next_delete_mutation_token,
            panel_generation,
            id,
            content_hash,
        };
        self.next_delete_mutation_token += 1;
        self.pending_delete_mutation = Some(request);
        self.delete_error_visible = false;
        UiAction::QueueDelete(request)
    }

    /// 只接受与活动删除四元身份完全一致的结果；不要求当前面板仍处于旧会话。
    fn apply_delete_mutation_result(&mut self, result: DeleteMutationResult) {
        let Some(pending) = self.pending_delete_mutation else {
            return;
        };
        if result.mutation_token != pending.mutation_token
            || result.panel_generation != pending.panel_generation
            || result.id != pending.id
            || result.content_hash != pending.content_hash
        {
            return;
        }

        self.pending_delete_mutation = None;
        match result.outcome {
            Ok(()) => {
                self.delete_error_visible = false;
                let previous_selected_index = self.snapshot.selected_index;
                let selected_identity = previous_selected_index
                    .and_then(|index| self.snapshot.items.get(index))
                    .map(|item| (item.id, item.content_hash));
                self.snapshot.items.retain(|item| {
                    item.id != result.id || item.content_hash != result.content_hash
                });
                self.history.remove(result.id, result.content_hash);
                self.capture_revisions
                    .remove(&(result.id, result.content_hash));
                self.snapshot.selected_index =
                    selected_identity.and_then(|(selected_id, selected_hash)| {
                        self.snapshot.items.iter().position(|item| {
                            item.id == selected_id && item.content_hash == selected_hash
                        })
                    });
                if self.snapshot.selected_index.is_none() && !self.snapshot.items.is_empty() {
                    // 删除当前选中项时尽量保留相邻位置，末项删除则夹到新的末项。
                    self.snapshot.selected_index = Some(
                        previous_selected_index
                            .unwrap_or(0)
                            .min(self.snapshot.items.len().saturating_sub(1)),
                    );
                }
                self.select_first_if_needed();
                self.search.cancel();
                // 先推进数据集拒绝旧首页/续页，再按当前筛选查询数据库最终状态。
                self.begin_history_dataset(self.panel_visible);
            }
            Err(failure) => {
                self.delete_error_visible = true;
                if matches!(
                    failure,
                    DeleteMutationFailure::IdentityChanged | DeleteMutationFailure::NotDeletable
                ) {
                    self.search.cancel();
                    self.begin_history_dataset(self.panel_visible);
                }
            }
        }
    }

    /// 删除请求未进入后台单槽时清除 pending，保留卡片并显示固定失败提示。
    fn mark_delete_submission_failed(&mut self, request: &DeleteMutationRequest) {
        if self.pending_delete_mutation.as_ref() == Some(request) {
            self.pending_delete_mutation = None;
            self.delete_error_visible = true;
        }
    }

    /// 首次点击只打开确认区；任一 mutation 在途时不得进入确认流程。
    fn show_clear_unpinned_confirmation(&mut self) -> UiAction {
        if !self.panel_visible
            || self.pending_pin_mutation.is_some()
            || self.pending_delete_mutation.is_some()
            || self.pending_clear_history_mutation.is_some()
        {
            return UiAction::None;
        }
        self.clear_unpinned_confirmation_visible = true;
        self.clear_unpinned_error_visible = false;
        self.clear_all_confirmation_visible = false;
        self.clear_all_confirmation_text.clear();
        self.clear_all_error_visible = false;
        UiAction::None
    }

    /// 用户二次确认后建立唯一清空请求；事务成功前不修改任何卡片。
    fn begin_clear_unpinned_mutation(&mut self, panel_generation: u64) -> UiAction {
        if !self.panel_visible
            || panel_generation != self.panel_generation
            || !self.clear_unpinned_confirmation_visible
            || self.pending_pin_mutation.is_some()
            || self.pending_delete_mutation.is_some()
            || self.pending_clear_history_mutation.is_some()
        {
            return UiAction::None;
        }
        if self.next_clear_history_mutation_token == u64::MAX {
            self.clear_unpinned_confirmation_visible = false;
            self.clear_unpinned_error_visible = true;
            return UiAction::None;
        }

        let request = ClearHistoryMutationRequest {
            mutation_token: self.next_clear_history_mutation_token,
            panel_generation,
            // 旧入口必须显式选择未收藏文本，绝不能依赖危险默认值。
            scope: ClearHistoryScope::UnpinnedText,
        };
        self.next_clear_history_mutation_token += 1;
        self.pending_clear_captures.clear();
        self.pending_clear_history_mutation = Some(request);
        self.clear_unpinned_confirmation_visible = false;
        self.clear_unpinned_error_visible = false;
        UiAction::QueueClearHistory(request)
    }

    /// 打开清空全部强确认区；明确关闭普通清空确认，避免两个范围同时可见。
    fn show_clear_all_confirmation(&mut self) -> UiAction {
        if !self.panel_visible
            || self.pending_pin_mutation.is_some()
            || self.pending_delete_mutation.is_some()
            || self.pending_clear_history_mutation.is_some()
        {
            return UiAction::None;
        }
        self.clear_unpinned_confirmation_visible = false;
        self.clear_unpinned_error_visible = false;
        self.clear_all_confirmation_visible = true;
        self.clear_all_confirmation_text.clear();
        self.clear_all_error_visible = false;
        UiAction::None
    }

    /// 只有 UI 状态和点击事件都精确携带固定短语时才建立全量清空请求。
    fn begin_clear_all_mutation(
        &mut self,
        panel_generation: u64,
        confirmation_text: String,
    ) -> UiAction {
        if !self.panel_visible
            || panel_generation != self.panel_generation
            || !self.clear_all_confirmation_visible
            || self.clear_all_confirmation_text != CLEAR_ALL_CONFIRMATION_PHRASE
            || confirmation_text != CLEAR_ALL_CONFIRMATION_PHRASE
            || self.pending_pin_mutation.is_some()
            || self.pending_delete_mutation.is_some()
            || self.pending_clear_history_mutation.is_some()
        {
            return UiAction::None;
        }
        if self.next_clear_history_mutation_token == u64::MAX {
            self.clear_all_confirmation_visible = false;
            self.clear_all_confirmation_text.clear();
            self.clear_all_error_visible = true;
            return UiAction::None;
        }

        let request = ClearHistoryMutationRequest {
            mutation_token: self.next_clear_history_mutation_token,
            panel_generation,
            scope: ClearHistoryScope::All,
        };
        self.next_clear_history_mutation_token += 1;
        self.pending_clear_captures.clear();
        self.pending_clear_history_mutation = Some(request);
        self.clear_all_confirmation_visible = false;
        self.clear_all_confirmation_text.clear();
        self.clear_all_error_visible = false;
        UiAction::QueueClearHistory(request)
    }

    /// 只消费与活动清空三元身份匹配的结果；成功后按范围和修订号收口全部派生状态。
    fn apply_clear_history_result(&mut self, result: ClearHistoryMutationResult) {
        let Some(pending) = self.pending_clear_history_mutation else {
            return;
        };
        if result.mutation_token != pending.mutation_token
            || result.panel_generation != pending.panel_generation
            || result.scope != pending.scope
        {
            return;
        }

        self.pending_clear_history_mutation = None;
        match result.outcome {
            Ok(success) => {
                self.clear_unpinned_error_visible = false;
                self.clear_all_error_visible = false;
                self.clear_all_confirmation_text.clear();
                self.active_clear_revision = self.active_clear_revision.max(success.clear_revision);
                // 当前筛选首页可能不含清空期间的新捕获；先从独立账本恢复事务后条目，
                // 再执行范围过滤，确保查询替换不能抹掉 storage revision 顺序证据。
                let pending_captures = std::mem::take(&mut self.pending_clear_captures);
                for (item, revision) in pending_captures.into_iter().rev() {
                    if revision >= success.clear_revision {
                        self.capture_revisions
                            .insert((item.id, item.content_hash), revision);
                        self.history.record_persisted(item);
                    }
                }
                let selected_identity = self
                    .snapshot
                    .selected_index
                    .and_then(|index| self.snapshot.items.get(index))
                    .map(|item| (item.id, item.content_hash));
                let revisions = &self.capture_revisions;
                let keep = |item: &UiClipboardItem| {
                    if pending.scope == ClearHistoryScope::UnpinnedText && item.is_pinned {
                        return true;
                    }
                    revisions
                        .get(&(item.id, item.content_hash))
                        .is_some_and(|revision| *revision >= success.clear_revision)
                };
                self.snapshot.items.retain(keep);
                self.history.retain(keep);
                // 捕获可能已经进入 MemoryHistory，但其刷新页被清空结果作废；从已过滤的
                // 内存真相重建可见列表，避免后续首页提交失败时把事务后捕获暂时藏起来。
                self.rebuild_visible_snapshot(
                    selected_identity.map(|(_selected_id, selected_hash)| selected_hash),
                );
                self.select_first_if_needed();
                self.prune_capture_revisions();
                self.search.cancel();
                // 新数据集同时拒绝清空前已经在途的首页、续页和搜索结果。
                self.begin_history_dataset(self.panel_visible);
            }
            Err(_) => {
                self.pending_clear_captures.clear();
                match pending.scope {
                    ClearHistoryScope::UnpinnedText => {
                        self.clear_unpinned_error_visible = true;
                    }
                    ClearHistoryScope::All => {
                        self.clear_all_error_visible = true;
                    }
                }
            }
        }
    }

    /// 清空请求未进入后台单槽时清除 pending，保留全部卡片并显示固定提示。
    fn mark_clear_history_submission_failed(&mut self, request: &ClearHistoryMutationRequest) {
        if self.pending_clear_history_mutation.as_ref() == Some(request) {
            self.pending_clear_history_mutation = None;
            self.pending_clear_captures.clear();
            match request.scope {
                ClearHistoryScope::UnpinnedText => {
                    self.clear_unpinned_error_visible = true;
                }
                ClearHistoryScope::All => {
                    self.clear_all_error_visible = true;
                }
            }
        }
    }

    /// 将捕获修订索引限制在当前缓存或可见快照身份内，避免常驻运行后无界增长。
    fn prune_capture_revisions(&mut self) {
        let identities = self
            .history
            .items()
            .iter()
            .chain(self.snapshot.items.iter())
            .map(|item| (item.id, item.content_hash))
            .collect::<HashSet<_>>();
        self.capture_revisions
            .retain(|identity, _| identities.contains(identity));
    }

    /// 记录清空在途期间到达的捕获；同一身份只保留最新快照，并限制为 UI 历史上限。
    fn record_pending_clear_capture(&mut self, item: &UiClipboardItem, revision: u64) {
        if self.pending_clear_history_mutation.is_none() {
            return;
        }
        let identity = (item.id, item.content_hash);
        self.pending_clear_captures
            .retain(|(entry, _)| (entry.id, entry.content_hash) != identity);
        self.pending_clear_captures
            .insert(0, (item.clone(), revision));
        self.pending_clear_captures
            .truncate(UI_HISTORY_MEMORY_CAPACITY);
    }

    /// 清空当前搜索接缝并恢复完整内存历史；每次新一轮面板打开都从此状态开始。
    fn reset_search_state(&mut self) {
        self.search.cancel();
        self.search_text.clear();
        self.search_filter = SearchFilter::All;
        self.search_status = SearchStatus::Idle;
        self.search_generation = None;
        self.pending_history_request = None;
        self.reset_history_scroll_dataset();
        self.snapshot.selected_index = None;
        self.pin_error_visible = false;
        self.delete_error_visible = false;
        self.clear_unpinned_confirmation_visible = false;
        self.clear_unpinned_error_visible = false;
        self.clear_all_confirmation_visible = false;
        self.clear_all_confirmation_text.clear();
        self.clear_all_error_visible = false;
        self.rebuild_visible_snapshot(None);
    }

    /// 推进 SQLite 数据集；可见时立即生成当前筛选的首页请求。
    fn begin_history_dataset(&mut self, request_now: bool) {
        self.reset_history_scroll_dataset();
        match self.history_pages.begin_dataset() {
            Ok(_) if request_now => {
                self.search_status = SearchStatus::Loading;
                match self
                    .history_pages
                    .request_first_page(self.build_search_query())
                {
                    Ok(request) => self.pending_history_request = Some(request),
                    Err(_) => self.mark_history_identity_error(),
                }
            }
            Ok(_) => {
                self.pending_history_request = None;
            }
            Err(HistoryPageCoordinatorError::GenerationExhausted) => {
                self.mark_history_identity_error()
            }
            Err(
                HistoryPageCoordinatorError::TokenExhausted
                | HistoryPageCoordinatorError::NoActiveDataset
                | HistoryPageCoordinatorError::RequestAlreadyActive
                | HistoryPageCoordinatorError::DatasetExhausted
                | HistoryPageCoordinatorError::RetryRequired,
            ) => self.mark_history_identity_error(),
        }
    }

    /// 身份耗尽只显示固定错误并保留当前卡片。
    fn mark_history_identity_error(&mut self) {
        self.pending_history_request = None;
        self.reset_history_scroll_dataset();
        self.search_status = SearchStatus::Error;
    }

    /// 清除当前数据集的滚动续页门禁；新数据集总是从 outside 初态开始。
    fn reset_history_scroll_dataset(&mut self) {
        self.history_was_near_bottom = false;
        self.history_next_page_loading = false;
        self.append_binding_gate = AppendBindingGate::Idle;
        // 新数据集/新面板不复用旧 Published 快照，迟到卡片和视口事件必须立即失效。
        self.published_window = None;
        self.pending_origin_token = None;
    }

    /// 根据滞回阈值更新底部状态；中间区保持调用前状态。
    fn near_bottom_after_distance(previous: bool, distance: i32) -> bool {
        if distance <= HISTORY_BOTTOM_ENTER_THRESHOLD {
            true
        } else if distance > HISTORY_BOTTOM_EXIT_THRESHOLD {
            false
        } else {
            previous
        }
    }

    /// 计算真实 Flickable 底部距离；未完成布局的几何不参与分页。
    fn history_bottom_distance(
        viewport_y: i32,
        visible_height: i32,
        content_height: i32,
    ) -> Option<i32> {
        if visible_height <= 0 || content_height <= 0 {
            return None;
        }
        let offset = viewport_y.saturating_neg().max(0);
        Some(
            content_height
                .saturating_sub(visible_height)
                .saturating_sub(offset)
                .max(0),
        )
    }

    /// 根据真实 Flickable 几何检测底部边沿；绑定后探针等待期间旧回调不得触发分页。
    fn handle_history_viewport(
        &mut self,
        viewport_y: i32,
        visible_height: i32,
        content_height: i32,
    ) {
        // 先保存原始负向坐标和可见高度，下一次 WindowCommit 会统一 clamp 并生成新窗口。
        self.history_viewport_y = i64::from(viewport_y);
        self.history_visible_height = i64::from(visible_height.max(0));
        if !self.panel_visible || self.append_binding_gate != AppendBindingGate::Idle {
            return;
        }
        let Some(distance) =
            Self::history_bottom_distance(viewport_y, visible_height, content_height)
        else {
            return;
        };
        let near_bottom = Self::near_bottom_after_distance(self.history_was_near_bottom, distance);
        let entered_bottom = near_bottom && !self.history_was_near_bottom;
        self.history_was_near_bottom = near_bottom;
        if !entered_bottom {
            return;
        }
        if self.history_pages.retry_required() {
            // 离开后重新进入等同一次明确重试，仍沿用成功页保存的数据库游标。
            self.history_pages.allow_retry();
        }
        self.request_next_history_page();
    }

    /// 消费模型绑定后的唯一追加探针；匹配失败的旧事件不能解除当前门禁。
    fn handle_post_append_probe(
        &mut self,
        append_revision: u64,
        viewport_y: i32,
        visible_height: i32,
        content_height: i32,
    ) {
        if self.append_binding_gate != AppendBindingGate::ProbePending(append_revision) {
            return;
        }
        // 先消费再判断，确保重复探针即使在 inside 也不能生成第二个请求。
        self.append_binding_gate = AppendBindingGate::Idle;
        if !self.panel_visible {
            return;
        }
        let Some(distance) =
            Self::history_bottom_distance(viewport_y, visible_height, content_height)
        else {
            return;
        };
        let near_bottom = Self::near_bottom_after_distance(self.history_was_near_bottom, distance);
        self.history_was_near_bottom = near_bottom;
        if near_bottom {
            self.request_next_history_page();
        }
    }

    /// 精确取消尚未调度成功的追加探针；旧失败不得清除后来一页的 pending。
    fn cancel_post_append_probe(&mut self, append_revision: u64) {
        if self.append_binding_gate == AppendBindingGate::ProbePending(append_revision) {
            self.append_binding_gate = AppendBindingGate::Idle;
        }
    }

    /// 为一次成功 Append 分配不回绕的 UI 绑定修订；耗尽时关闭本次自动探针。
    fn reserve_append_revision(&mut self) -> Option<u64> {
        let Some(revision) = self.next_append_revision.checked_add(1) else {
            self.append_binding_gate = AppendBindingGate::RevisionExhausted;
            return None;
        };
        self.next_append_revision = revision;
        self.append_binding_gate = AppendBindingGate::ProbePending(revision);
        Some(revision)
    }

    /// 修订耗尽的 Append 完成模型绑定后只解除门禁，不自动探测或请求续页。
    fn finish_exhausted_append_binding(&mut self) {
        if self.append_binding_gate == AppendBindingGate::RevisionExhausted {
            self.append_binding_gate = AppendBindingGate::Idle;
        }
    }

    /// 按当前筛选、数据库游标和剩余容量生成唯一续页请求。
    fn request_next_history_page(&mut self) {
        if self.history_pages.has_active_request() || self.snapshot.items.len() >= MAX_LOADED_ITEMS
        {
            return;
        }
        match self
            .history_pages
            .request_next_page(self.build_search_query())
        {
            Ok(request) => {
                self.pending_history_request = Some(request);
                self.history_next_page_loading = true;
            }
            Err(HistoryPageCoordinatorError::RequestAlreadyActive)
            | Err(HistoryPageCoordinatorError::DatasetExhausted)
            | Err(HistoryPageCoordinatorError::RetryRequired) => {}
            Err(_) => self.mark_history_identity_error(),
        }
    }

    /// 提交一次关键词或标签变化；保留旧可见结果并等待当前代次到期，避免内容跳闪。
    fn begin_search(&mut self, text: String, filter: SearchFilter, now: Instant) -> UiAction {
        if !self.panel_visible {
            return UiAction::None;
        }

        self.search_text = text;
        self.search_filter = filter;
        // 防抖期间保留上一批结果，让“搜索中”提示不会造成内容跳闪；当前代次完成后
        // `apply_search_if_current` 才会以新筛选集合替换卡片。
        self.snapshot.selected_index = None;
        self.search_status = SearchStatus::Loading;
        // 输入事件发生时立即推进数据集；旧 SQLite 结果不能等到 120ms 到期才失效。
        if self.history_pages.begin_dataset().is_err() {
            self.mark_history_identity_error();
            return UiAction::None;
        }
        self.reset_history_scroll_dataset();

        match self.search.submit(self.build_search_query(), now) {
            Ok(generation) => {
                self.search_generation = Some(generation.as_u64());
                UiAction::ScheduleSearch {
                    generation: generation.as_u64(),
                }
            }
            Err(SearchCoordinatorError::GenerationExhausted) => {
                // 代次耗尽是唯一可观察的协调器错误；不把内部错误对象或查询正文透传给 UI。
                self.search_generation = None;
                self.search_status = SearchStatus::Error;
                UiAction::None
            }
        }
    }

    /// 仅应用仍属于当前搜索代次的防抖事件；旧计时器不得 poll 新请求。
    fn apply_search_if_current(&mut self, generation: u64, now: Instant) -> UiAction {
        if !self.panel_visible
            || self.search.latest_generation().map(|value| value.as_u64()) != Some(generation)
        {
            return UiAction::None;
        }

        let Some(search_request) = self.search.poll(now) else {
            return UiAction::None;
        };
        if search_request.generation.as_u64() != generation {
            return UiAction::None;
        }
        match self.history_pages.request_first_page(search_request.query) {
            Ok(request) => self.pending_history_request = Some(request),
            Err(_) => self.mark_history_identity_error(),
        }
        UiAction::None
    }

    /// 将当前输入和标签转换成 ATOM-23 可接受的拥有型查询；本原子不访问存储线程。
    fn build_search_query(&self) -> HistoryQuery {
        let keyword = self.search_text.trim();
        HistoryQuery {
            keyword: (!keyword.is_empty()).then(|| keyword.to_owned()),
            // 类型标签只映射为两个固定数据库值；“全部”和“收藏”允许文本/图片混合。
            item_type: match self.search_filter {
                SearchFilter::Text => Some("text".to_owned()),
                SearchFilter::Image => Some("image".to_owned()),
                SearchFilter::All | SearchFilter::Pinned => None,
            },
            is_pinned: match self.search_filter {
                SearchFilter::Pinned => Some(true),
                SearchFilter::All | SearchFilter::Text | SearchFilter::Image => None,
            },
            limit: UI_FIRST_BATCH_SIZE as u32,
            ..HistoryQuery::default()
        }
    }

    /// 应用从 latest 结果槽提取的首页或续页；只接受精确三元身份。
    fn apply_history_page_result(&mut self, result: HistoryPageResult) -> HistoryModelRefresh {
        let selected_identity = self
            .snapshot
            .selected_index
            .and_then(|index| self.snapshot.items.get(index))
            .map(|item| (item.id, item.content_hash));
        let Some(application) =
            self.history_pages
                .accept_page(self.panel_visible, result, &self.snapshot.items)
        else {
            return HistoryModelRefresh::None;
        };
        self.history_next_page_loading = false;
        match application {
            HistoryPageApplication::Replace(items) => {
                self.append_binding_gate = AppendBindingGate::Idle;
                self.history_was_near_bottom = false;
                self.history.replace(items.clone());
                self.snapshot.items = items;
                self.refresh_history_geometry();
                self.snapshot.selected_index = selected_identity.and_then(|(id, hash)| {
                    self.snapshot
                        .items
                        .iter()
                        .position(|item| item.id == id && item.content_hash == hash)
                });
                self.select_first_if_needed();
                self.prune_capture_revisions();
                self.search_status = if self.snapshot.items.is_empty() {
                    SearchStatus::Empty
                } else {
                    SearchStatus::Results
                };
                HistoryModelRefresh::Replace
            }
            HistoryPageApplication::Append(items) => {
                self.snapshot.items.extend(items);
                self.history.replace(self.snapshot.items.clone());
                self.refresh_history_geometry();
                self.prune_capture_revisions();
                self.search_status = if self.snapshot.items.is_empty() {
                    SearchStatus::Empty
                } else {
                    SearchStatus::Results
                };
                HistoryModelRefresh::AppendPreservingViewport {
                    append_revision: self.reserve_append_revision(),
                }
            }
            HistoryPageApplication::FirstPageFailed => {
                self.search_status = SearchStatus::Error;
                HistoryModelRefresh::None
            }
            HistoryPageApplication::NextPageFailed => HistoryModelRefresh::None,
        }
    }

    /// 查询请求未能进入 worker 时按原请求身份收口；续页进入固定重试态，首页显示固定错误。
    fn mark_history_submission_failed(&mut self, request: &HistoryPageRequest) {
        match self
            .history_pages
            .fail_submission(self.panel_visible, request)
        {
            Some(HistoryPageApplication::FirstPageFailed) => {
                self.search_status = SearchStatus::Error
            }
            Some(HistoryPageApplication::NextPageFailed) => {
                self.history_next_page_loading = false;
            }
            Some(HistoryPageApplication::Replace(_))
            | Some(HistoryPageApplication::Append(_))
            | None => {}
        }
    }

    /// 取出本次事件生成的后台请求；调用方必须在释放 UiState 借用后提交。
    fn take_pending_history_request(&mut self) -> Option<HistoryPageRequest> {
        self.pending_history_request.take()
    }

    /// 按当前关键词和标签在有界内存历史中重建可见列表，避免改变完整缓存顺序。
    fn rebuild_visible_snapshot(&mut self, selected_hash: Option<[u8; 32]>) {
        let query = self.search_text.trim().to_lowercase();
        self.snapshot.items = self
            .history
            .items()
            .iter()
            .filter(|item| {
                if self.search_filter == SearchFilter::Pinned && !item.is_pinned {
                    return false;
                }
                if query.is_empty() {
                    return true;
                }
                item.preview.to_lowercase().contains(&query)
                    || item.source.to_lowercase().contains(&query)
            })
            .cloned()
            .collect();
        restore_selected_index(&mut self.snapshot, selected_hash);
        self.refresh_history_geometry();
    }

    /// 为当前完整 UI 快照构造精确混合高度 prefix-sum；失败时保留旧快照而不伪造高度。
    fn refresh_history_geometry(&mut self) {
        // 数据集身份一旦变化，旧 WindowCommit 即使 checksum 仍有效也不能再解析新快照的
        // local index；先清空发布闩锁，确保几何构造失败时安全回退 legacy 而不是接受迟到事件。
        self.published_window = None;
        self.pending_origin_token = None;
        let Some(next_dataset_revision) = self.dataset_revision.checked_add(1) else {
            self.history_geometry = None;
            return;
        };
        let items = self
            .snapshot
            .items
            .iter()
            .map(|item| HistoryGeometryItem {
                id: item.id,
                content_hash: item.content_hash,
                height: match item.kind {
                    crate::command::UiClipboardItemKind::Text => TEXT_HISTORY_ROW_HEIGHT as i64,
                    crate::command::UiClipboardItemKind::Image(_) => {
                        IMAGE_HISTORY_ROW_HEIGHT as i64
                    }
                },
            })
            .collect();
        match HistoryGeometry::new(items) {
            Ok(geometry) => {
                self.dataset_revision = next_dataset_revision;
                self.history_geometry = Some(geometry);
            }
            Err(_) => {
                // 任何高度非法/溢出都关闭显式模式，legacy 路径仍可显示旧摘要。
                self.history_geometry = None;
            }
        }
    }

    /// 从当前 prefix-sum 快照构造最多 100 行的唯一 WindowCommit。
    fn build_window_commit(&mut self) -> Option<WindowCommit> {
        let geometry = self.history_geometry.as_ref()?;
        let revision = self.window_revision;
        let next_revision = revision.checked_add(1)?;
        let window = geometry
            .window_for(
                self.history_viewport_y,
                self.history_visible_height,
                THUMBNAIL_ITEM_BUFFER,
            )
            .ok()?;
        let origin_token = if window.viewport_y != self.history_viewport_y {
            let token = self.next_origin_token.checked_add(1)?;
            self.next_origin_token = token;
            Some(token)
        } else {
            None
        };
        let cards = window
            .items
            .iter()
            .filter_map(|entry| self.snapshot.items.get(entry.absolute_index).cloned())
            .collect::<Vec<_>>();
        if cards.len() != window.items.len() {
            return None;
        }
        let offsets = window
            .items
            .iter()
            .map(|entry| WindowOffset {
                absolute_index: entry.absolute_index as u64,
                id: entry.id,
                content_hash: entry.content_hash,
                top: entry.top,
                height: entry.height,
            })
            .collect::<Vec<_>>();
        let mut builder =
            WindowCommitBuilder::new(self.session_nonce?, self.dataset_revision, revision)?;
        if !builder.set_window(WindowCommitPayload {
            start: window.start as u64,
            total_count: window.total_count as u64,
            total_height: window.total_height,
            visible_height: window.visible_height,
            clamped_viewport_y: window.viewport_y,
            origin_token,
            cards,
            offsets,
        }) || !builder.ready()
        {
            return None;
        }
        let commit = builder.publish_commit_stamp()?;
        self.published_window = Some(commit.clone());
        self.pending_origin_token = commit.origin_token;
        self.window_revision = next_revision;
        self.history_viewport_y = commit.clamped_viewport_y;
        Some(commit)
    }

    /// 面板首次显示时把选择置于当前已加载列表第一项；空列表保持无选中项。
    fn select_first_if_needed(&mut self) {
        let limit = selection_limit(&self.snapshot);
        if limit == 0 {
            self.snapshot.selected_index = None;
            return;
        }

        self.snapshot.selected_index = Some(
            self.snapshot
                .selected_index
                .unwrap_or(0)
                .min(limit.saturating_sub(1)),
        );
    }

    /// 返回当前打开代次，回调据此生成不会误伤新面板的关闭事件。
    fn panel_generation(&self) -> u64 {
        self.panel_generation
    }

    /// 复制出不可变观测结果，避免把内部可变引用暴露给调用方。
    fn snapshot(&self) -> UiStateSnapshot {
        UiStateSnapshot {
            snapshot: self.snapshot.clone(),
            search_text: self.search_text.clone(),
            search_filter: self.search_filter,
            search_status: self.search_status,
            startup_status: self.startup_status.clone(),
            search_generation: self.search_generation,
            history_performance: self.history_pages.performance_snapshot(),
            panel_visible: self.panel_visible,
            panel_generation: self.panel_generation,
            quitting: self.quitting,
            pending_pin_mutation: self.pending_pin_mutation,
            pin_error_visible: self.pin_error_visible,
            pending_delete_mutation: self.pending_delete_mutation,
            delete_error_visible: self.delete_error_visible,
            clear_unpinned_confirmation_visible: self.clear_unpinned_confirmation_visible,
            pending_clear_history_mutation: self.pending_clear_history_mutation,
            clear_unpinned_error_visible: self.clear_unpinned_error_visible,
            clear_all_confirmation_visible: self.clear_all_confirmation_visible,
            clear_all_confirmation_text: self.clear_all_confirmation_text.clone(),
            clear_all_error_visible: self.clear_all_error_visible,
            active_clear_revision: self.active_clear_revision,
            applied_event_count: self.applied_event_count,
            applied_on_thread: self.applied_on_thread,
        }
    }
}

thread_local! {
    /// 每个线程各自持有状态；只有运行 Slint 事件循环的线程会收到后台提交的事件。
    static UI_STATE: RefCell<UiState> = RefCell::new(UiState::default());
    /// 缩略图请求端只在 UI 线程使用，后台线程无法取得 Slint 图片对象。
    static UI_THUMBNAIL_LOADER: RefCell<Option<ThumbnailLoaderSender>> = const { RefCell::new(None) };
    /// 已提交身份用于抑制滚动重绘产生的重复请求。
    static UI_THUMBNAIL_REQUESTED: RefCell<HashSet<(u64, u64, [u8; 32])>> = RefCell::new(HashSet::new());
    /// 解码像素只能在 UI 线程转换为 Slint Image，并按稳定记录身份缓存。
    static UI_THUMBNAIL_CACHE: RefCell<HashMap<(u64, [u8; 32]), Image>> = RefCell::new(HashMap::new());
    /// 失败身份显示稳定占位，避免布局变化重复读取同一坏文件。
    static UI_THUMBNAIL_FAILED: RefCell<HashSet<(u64, [u8; 32])>> = RefCell::new(HashSet::new());
    /// 最近一次视口重算得到的图片保留身份；范围外结果不得进入纹理缓存。
    static UI_THUMBNAIL_VISIBLE: RefCell<HashSet<(u64, [u8; 32])>> = RefCell::new(HashSet::new());
    /// 成功和失败身份共享的 LRU 顺序；队尾是最近访问项，队首优先淘汰。
    static UI_THUMBNAIL_CACHE_ORDER: RefCell<VecDeque<(u64, [u8; 32])>> = const { RefCell::new(VecDeque::new()) };
    /// 最近一次真实列表视口；模型增删和分页后必须沿用它重新计算可见图片。
    static UI_THUMBNAIL_VIEWPORT: RefCell<(i32, i32)> = const { RefCell::new((0, 500)) };
    /// UI 线程持有的弱窗口引用，避免事件入口形成窗口强引用环。
    static UI_WINDOW: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
    #[cfg(windows)]
    /// UI 线程只保留复制请求桥的 Clone，不持有存储执行器或剪贴板正文。
    static UI_COPY_INBOX: RefCell<Option<ClipboardCaptureInbox>> = const { RefCell::new(None) };
    /// UI 线程只持有 latest-wins 查询发送端，提交不会等待 SQLite。
    static UI_HISTORY_REQUESTS: RefCell<Option<HistoryRequestSender>> = const { RefCell::new(None) };
    /// UI wake 到达后从该槽提取最新轻量结果，并在同一锁内清除 wake_pending。
    static UI_HISTORY_RESULTS: RefCell<Option<HistoryResultReceiver>> = const { RefCell::new(None) };
    /// UI 线程只持有收藏请求发送端；SQLite 和 worker 生命周期仍由主线程拥有。
    static UI_PIN_MUTATIONS: RefCell<Option<PinMutationSender>> = const { RefCell::new(None) };
    /// UI 线程只持有删除请求发送端；DEL-03 会通过该单槽执行非阻塞提交。
    static UI_DELETE_MUTATIONS: RefCell<Option<DeleteMutationSender>> = const { RefCell::new(None) };
    /// UI 线程只持有双范围清空请求发送端；SQLite 和 worker 生命周期仍由主线程拥有。
    static UI_CLEAR_HISTORY_MUTATIONS: RefCell<Option<ClearHistoryMutationSender>> = const { RefCell::new(None) };
}

/// 可安全跨线程读取的 UI 状态快照，不包含任何 UI 引用或 Slint 对象。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiStateSnapshot {
    /// 当前历史展示快照。
    pub snapshot: UiSnapshot,
    /// 当前搜索框输入；只保存用户主动输入的查询词，不保存剪贴板正文。
    pub search_text: String,
    /// 当前搜索标签。
    pub search_filter: SearchFilter,
    /// 当前搜索结果状态。
    pub search_status: SearchStatus,
    /// 当前启动设置的稳定反馈文案；空字符串表示尚未收到反馈。
    pub startup_status: String,
    /// 当前搜索代次；旧计时器测试使用它证明结果没有闪回。
    pub search_generation: Option<u64>,
    /// 当前数据集的纯数值分页性能快照；不包含游标、重试状态或活动请求。
    pub history_performance: HistoryPerformanceSnapshot,
    /// 当前看板可见性。
    pub panel_visible: bool,
    /// 当前面板打开代次；只用于验证关闭事件是否仍属于当前实例。
    pub panel_generation: u64,
    /// 是否已经接受退出请求；用于验证退出闩锁拒绝迟到事件。
    pub quitting: bool,
    /// 当前唯一在途收藏请求；测试只使用稳定身份，不包含正文。
    pub pending_pin_mutation: Option<PinMutationRequest>,
    /// 固定收藏失败提示当前是否可见。
    pub pin_error_visible: bool,
    /// 当前唯一在途删除请求；测试只使用稳定身份，不包含正文。
    pub pending_delete_mutation: Option<DeleteMutationRequest>,
    /// 固定删除失败提示当前是否可见。
    pub delete_error_visible: bool,
    /// 清空未收藏确认区当前是否可见。
    pub clear_unpinned_confirmation_visible: bool,
    /// 当前唯一在途清空请求；只含 token 和面板代次。
    pub pending_clear_history_mutation: Option<ClearHistoryMutationRequest>,
    /// 固定清空失败提示当前是否可见。
    pub clear_unpinned_error_visible: bool,
    /// 清空全部强确认区当前是否可见。
    pub clear_all_confirmation_visible: bool,
    /// 强确认输入框当前原始文本。
    pub clear_all_confirmation_text: String,
    /// 固定清空全部失败提示当前是否可见。
    pub clear_all_error_visible: bool,
    /// 已消费的最大清空修订号；用于测试水位只增不减。
    pub active_clear_revision: u64,
    /// 已由 UI reducer 应用的事件数量。
    pub applied_event_count: u64,
    /// 最后一次 reducer 执行所在的线程，用于测试线程所有权。
    pub applied_on_thread: Option<ThreadId>,
}

/// 在 UI 线程登记主窗口弱引用，并把关闭、键盘、鼠标与搜索回调接入状态协议。
pub fn bind_app_window(window: &AppWindow) {
    UI_WINDOW.with(|target| {
        *target.borrow_mut() = Some(window.as_weak());
    });

    window.on_panel_dismiss_requested(|| {
        let generation = current_panel_generation();
        if let Err(error) = post_ui_event(UiEvent::HidePanel { generation }) {
            eprintln!("面板关闭事件无法进入 UI 事件队列：{error}");
        }
    });

    // 标题栏关闭和 Alt+F4 都只拒绝本次原生关闭请求；Esc 仍通过显式事件隐藏到托盘。
    window
        .window()
        .on_close_requested(|| CloseRequestResponse::KeepWindowShown);

    window.on_card_selection_requested(|index| {
        // Slint 回调与 UI reducer 位于同一线程；在事件排队前立刻把易变索引解析成稳定身份。
        let Some(event) = resolve_card_selection(index) else {
            return;
        };
        if let Err(error) = post_ui_event(event) {
            eprintln!("鼠标选择事件无法进入 UI 事件队列：{error}");
        }
    });

    window.on_copy_item_requested(|index| {
        // 按钮点击在 UI 线程同步冻结稳定身份，之后才进入异步 reducer。
        let Some(event) = resolve_copy_item(index) else {
            return;
        };
        if let Err(error) = post_ui_event(event) {
            eprintln!("显式复制事件无法进入 UI 事件队列：{error}");
        }
    });

    window.on_pin_item_requested(|index| {
        // 点击瞬间冻结稳定身份和相反的明确状态；后台禁止使用 toggle SQL。
        let Some(event) = resolve_pin_item(index) else {
            return;
        };
        if let Err(error) = post_ui_event(event) {
            eprintln!("收藏事件无法进入 UI 事件队列：{error}");
        }
    });

    window.on_delete_item_requested(|index| {
        // 删除按钮只传可见索引；UI 线程同步冻结 ID 与哈希后才允许进入后台。
        let Some(event) = resolve_delete_item(index) else {
            return;
        };
        if let Err(error) = post_ui_event(event) {
            eprintln!("删除事件无法进入 UI 事件队列：{error}");
        }
    });

    window.on_clear_unpinned_requested(|| {
        if let Err(error) = post_ui_event(UiEvent::ClearUnpinnedRequested) {
            eprintln!("打开清空确认事件无法进入 UI 事件队列：{error}");
        }
    });

    window.on_clear_unpinned_cancelled(|| {
        if let Err(error) = post_ui_event(UiEvent::ClearUnpinnedCancelled) {
            eprintln!("取消清空事件无法进入 UI 事件队列：{error}");
        }
    });

    window.on_clear_unpinned_confirmed(|| {
        let panel_generation = current_panel_generation();
        if let Err(error) = post_ui_event(UiEvent::ClearUnpinnedConfirmed { panel_generation }) {
            eprintln!("确认清空事件无法进入 UI 事件队列：{error}");
        }
    });

    window.on_clear_all_requested(|| {
        if let Err(error) = post_ui_event(UiEvent::ClearAllRequested) {
            eprintln!("打开清空全部确认事件无法进入 UI 事件队列：{error}");
        }
    });

    window.on_clear_all_confirmation_text_changed(|text| {
        if let Err(error) =
            post_ui_event(UiEvent::ClearAllConfirmationTextChanged(text.to_string()))
        {
            eprintln!("清空全部确认文字事件无法进入 UI 事件队列：{error}");
        }
    });

    window.on_clear_all_cancelled(|| {
        if let Err(error) = post_ui_event(UiEvent::ClearAllCancelled) {
            eprintln!("取消清空全部事件无法进入 UI 事件队列：{error}");
        }
    });

    window.on_clear_all_confirmed(|text| {
        let panel_generation = current_panel_generation();
        if let Err(error) = post_ui_event(UiEvent::ClearAllConfirmed {
            panel_generation,
            confirmation_text: text.to_string(),
        }) {
            eprintln!("确认清空全部事件无法进入 UI 事件队列：{error}");
        }
    });

    window.on_search_text_changed(|text| {
        if let Err(error) = post_ui_event(UiEvent::SearchTextChanged(text.to_string())) {
            eprintln!("搜索文本事件无法进入 UI 事件队列：{error}");
        }
    });

    window.on_search_filter_requested(|filter| {
        if let Err(error) = post_ui_event(UiEvent::SearchFilterChanged(SearchFilter::from_index(
            filter,
        ))) {
            eprintln!("搜索筛选事件无法进入 UI 事件队列：{error}");
        }
    });

    window.on_history_viewport_changed(
        |viewport_y, visible_height, content_height, origin_token| {
            let geometry_identity = UI_STATE.with(|state| {
                let state = state.borrow();
                state
                    .history_geometry
                    .as_ref()
                    .zip(state.published_window.as_ref())
                    .map(|(_, commit)| WindowEventIdentity {
                        session_nonce: commit.session_nonce,
                        dataset_revision: commit.dataset_revision,
                        window_revision: commit.window_revision,
                        commit_revision: commit.commit_revision,
                        commit_checksum: commit.commit_checksum,
                    })
            });
            let event = if let Some(identity) = geometry_identity {
                let Some(viewport_y) = quantize_slint_length(viewport_y) else {
                    return;
                };
                let Some(visible_height) = quantize_slint_length(visible_height) else {
                    return;
                };
                let origin_token = if origin_token.is_empty() {
                    None
                } else {
                    match origin_token.parse::<u64>() {
                        Ok(token) => Some(token),
                        Err(_) => return,
                    }
                };
                UiEvent::HistoryWindowViewportChanged {
                    identity,
                    viewport_y,
                    visible_height,
                    origin_token,
                }
            } else {
                let append_binding_gate = UI_STATE.with(|state| state.borrow().append_binding_gate);
                if append_binding_gate != AppendBindingGate::Idle {
                    UiEvent::HistoryViewportChangedDuringAppend {
                        append_revision: match append_binding_gate {
                            AppendBindingGate::ProbePending(revision) => Some(revision),
                            AppendBindingGate::RevisionExhausted => None,
                            AppendBindingGate::Idle => unreachable!("空闲状态已由外层分支排除"),
                        },
                        viewport_y: viewport_y.round() as i32,
                        visible_height: visible_height.round() as i32,
                        content_height: content_height.round() as i32,
                    }
                } else {
                    UiEvent::HistoryViewportChanged {
                        viewport_y: viewport_y.round() as i32,
                        visible_height: visible_height.round() as i32,
                        content_height: content_height.round() as i32,
                    }
                }
            };
            if let Err(error) = post_ui_event(event) {
                eprintln!("历史视口事件无法进入 UI 事件队列：{error}");
            }
        },
    );

    window.on_retry_history_page_requested(|| {
        if let Err(error) = post_ui_event(UiEvent::RetryHistoryPage) {
            eprintln!("历史续页重试事件无法进入 UI 事件队列：{error}");
        }
    });
}

#[cfg(windows)]
/// 登记由消息线程创建的剪贴板写回请求桥；正文读取和系统写回仍在历史结果泵线程完成。
pub fn bind_copy_request_inbox(inbox: ClipboardCaptureInbox) {
    UI_COPY_INBOX.with(|slot| {
        *slot.borrow_mut() = Some(inbox);
    });
}

/// 在 UI 线程绑定 SQLite 查询的非阻塞请求端和 latest 结果端。
pub fn bind_history_query_bridge(requests: HistoryRequestSender, results: HistoryResultReceiver) {
    UI_HISTORY_REQUESTS.with(|slot| {
        *slot.borrow_mut() = Some(requests);
    });
    UI_HISTORY_RESULTS.with(|slot| {
        *slot.borrow_mut() = Some(results);
    });
}

/// 在 UI 线程绑定收藏变更的非阻塞单槽发送端。
pub fn bind_pin_mutation_sender(sender: PinMutationSender) {
    UI_PIN_MUTATIONS.with(|slot| {
        *slot.borrow_mut() = Some(sender);
    });
}

/// 在 UI 线程绑定单条删除的非阻塞单槽发送端。
pub fn bind_delete_mutation_sender(sender: DeleteMutationSender) {
    UI_DELETE_MUTATIONS.with(|slot| {
        *slot.borrow_mut() = Some(sender);
    });
}

/// 在 UI 线程绑定显式双范围清空历史的非阻塞单槽发送端。
pub fn bind_clear_history_mutation_sender(sender: ClearHistoryMutationSender) {
    UI_CLEAR_HISTORY_MUTATIONS.with(|slot| {
        *slot.borrow_mut() = Some(sender);
    });
}

/// 在 UI 线程绑定缩略图后台加载器；请求端有界且提交过程不阻塞滚动。
pub fn bind_thumbnail_loader_sender(sender: ThumbnailLoaderSender) {
    UI_THUMBNAIL_LOADER.with(|slot| {
        *slot.borrow_mut() = Some(sender);
    });
}

/// 关闭查询双向桥；Quit 调用后 worker 不再接受排队请求，迟到结果也不再唤醒 UI。
fn close_history_query_bridge() {
    UI_HISTORY_REQUESTS.with(|slot| {
        if let Some(sender) = slot.borrow().as_ref() {
            sender.close();
        }
    });
    UI_HISTORY_RESULTS.with(|slot| {
        if let Some(receiver) = slot.borrow().as_ref() {
            receiver.close();
        }
    });
}

/// 关闭收藏请求入口；已经进入单槽的请求由 worker 排空后退出。
fn close_pin_mutation_bridge() {
    UI_PIN_MUTATIONS.with(|slot| {
        if let Some(sender) = slot.borrow().as_ref() {
            sender.close();
        }
    });
}

/// 关闭删除请求入口；已经进入单槽的请求由 worker 排空后退出。
fn close_delete_mutation_bridge() {
    UI_DELETE_MUTATIONS.with(|slot| {
        if let Some(sender) = slot.borrow().as_ref() {
            sender.close();
        }
    });
}

/// 关闭双范围清空请求入口；已经进入单槽的请求由 worker 排空后退出。
fn close_clear_history_mutation_bridge() {
    UI_CLEAR_HISTORY_MUTATIONS.with(|slot| {
        if let Some(sender) = slot.borrow().as_ref() {
            sender.close();
        }
    });
}

/// 将后台结果排入 Slint 事件循环；项目中所有后台到 UI 的路径都必须调用此函数。
///
/// 该函数只接受拥有型 `UiEvent`，不会同步执行 reducer。返回 `Ok(())` 只代表事件
/// 已成功进入队列，实际状态更新要等事件循环运行到该闭包后才发生。
pub fn post_ui_event(event: UiEvent) -> Result<(), slint::EventLoopError> {
    slint::invoke_from_event_loop(move || {
        let thumbnail_result = match &event {
            UiEvent::ThumbnailLoaded(result) => Some(result.clone()),
            _ => None,
        };
        let viewport = match &event {
            UiEvent::HistoryViewportChanged {
                viewport_y,
                visible_height,
                ..
            }
            | UiEvent::HistoryViewportChangedDuringAppend {
                viewport_y,
                visible_height,
                ..
            } => Some((*viewport_y, *visible_height)),
            UiEvent::HistoryWindowViewportChanged {
                viewport_y,
                visible_height,
                ..
            } => match (i32::try_from(*viewport_y), i32::try_from(*visible_height)) {
                (Ok(viewport_y), Ok(visible_height)) => Some((viewport_y, visible_height)),
                _ => None,
            },
            _ => None,
        };
        if let Some(viewport) = viewport {
            UI_THUMBNAIL_VIEWPORT.with(|current| {
                *current.borrow_mut() = viewport;
            });
        }
        let history_result = if matches!(&event, UiEvent::HistoryQueryWake) {
            UI_HISTORY_RESULTS.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .and_then(HistoryResultReceiver::take_latest)
            })
        } else {
            None
        };
        // 鼠标选择事件只改变 reducer 索引，不能重建 VecModel；否则每次点击都会让
        // ListView 重新创建卡片，破坏滚动连续性并把模型生命周期混入选择逻辑。
        let may_refresh_model = event_may_refresh_model(&event);
        let history_result_event = matches!(&event, UiEvent::HistoryQueryWake);
        let (
            action,
            mut snapshot,
            search_text,
            search_filter,
            mut search_status,
            mut startup_status,
            mut history_next_page_loading,
            mut history_retry_required,
            mut pending_pin_mutation,
            mut pin_error_visible,
            mut pending_delete_mutation,
            mut delete_error_visible,
            clear_unpinned_confirmation_visible,
            mut pending_clear_history_mutation,
            mut clear_unpinned_error_visible,
            clear_all_confirmation_visible,
            clear_all_confirmation_text,
            mut clear_all_error_visible,
            request,
            history_model_refresh,
            geometry_commit,
        ) = UI_STATE.with(|state| {
            let mut state = state.borrow_mut();
            let action = state.apply(event);
            let history_model_refresh = history_result
                .map(|result| state.apply_history_page_result(result))
                .unwrap_or(HistoryModelRefresh::None);
            let geometry_commit = if state.history_geometry.is_some()
                && (may_refresh_model
                    || history_result_event
                    || viewport.is_some()
                    || thumbnail_result.is_some())
            {
                state.build_window_commit()
            } else {
                None
            };
            (
                action,
                state.snapshot.clone(),
                state.search_text.clone(),
                state.search_filter,
                state.search_status,
                state.startup_status.clone(),
                state.history_next_page_loading,
                state.history_pages.retry_required(),
                state.pending_pin_mutation,
                state.pin_error_visible,
                state.pending_delete_mutation,
                state.delete_error_visible,
                state.clear_unpinned_confirmation_visible,
                state.pending_clear_history_mutation,
                state.clear_unpinned_error_visible,
                state.clear_all_confirmation_visible,
                state.clear_all_confirmation_text.clone(),
                state.clear_all_error_visible,
                state.take_pending_history_request(),
                history_model_refresh,
                geometry_commit,
            )
        });
        if let Some(request) = request {
            let failed_request = request.clone();
            let submitted = UI_HISTORY_REQUESTS.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .is_some_and(|sender| sender.submit(request).is_ok())
            });
            if !submitted {
                UI_STATE.with(|state| {
                    let mut state = state.borrow_mut();
                    state.mark_history_submission_failed(&failed_request);
                    snapshot = state.snapshot.clone();
                    search_status = state.search_status;
                    startup_status = state.startup_status.clone();
                    history_next_page_loading = state.history_next_page_loading;
                    history_retry_required = state.history_pages.retry_required();
                });
            }
        }
        if let UiAction::QueuePin(pin_request) = action {
            let submitted = UI_PIN_MUTATIONS.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .is_some_and(|sender| sender.try_submit(pin_request).is_ok())
            });
            if !submitted {
                UI_STATE.with(|state| {
                    let mut state = state.borrow_mut();
                    state.mark_pin_submission_failed(&pin_request);
                    snapshot = state.snapshot.clone();
                    pending_pin_mutation = state.pending_pin_mutation;
                    pin_error_visible = state.pin_error_visible;
                });
            }
        }
        if let UiAction::QueueDelete(delete_request) = action {
            let submitted = UI_DELETE_MUTATIONS.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .is_some_and(|sender| sender.try_submit(delete_request).is_ok())
            });
            if !submitted {
                UI_STATE.with(|state| {
                    let mut state = state.borrow_mut();
                    state.mark_delete_submission_failed(&delete_request);
                    snapshot = state.snapshot.clone();
                    pending_delete_mutation = state.pending_delete_mutation;
                    delete_error_visible = state.delete_error_visible;
                });
            }
        }
        if let UiAction::QueueClearHistory(clear_request) = action {
            let submitted = UI_CLEAR_HISTORY_MUTATIONS.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .is_some_and(|sender| sender.try_submit(clear_request).is_ok())
            });
            if !submitted {
                UI_STATE.with(|state| {
                    let mut state = state.borrow_mut();
                    state.mark_clear_history_submission_failed(&clear_request);
                    pending_clear_history_mutation = state.pending_clear_history_mutation;
                    clear_unpinned_error_visible = state.clear_unpinned_error_visible;
                    clear_all_error_visible = state.clear_all_error_visible;
                });
            }
        }
        // 后台结果必须在 UI 线程重新核对当前面板代次和稳定身份，旧会话结果直接丢弃。
        let thumbnail_applied = thumbnail_result
            .as_ref()
            .is_some_and(|result| apply_thumbnail_result(result, &snapshot));
        // 先根据本次真实视口整理缩略图保留范围，再绑定卡片模型；这样模型中的 Image
        // 克隆会在范围变化时一起被替换为空图片，范围外纹理不会继续被 Slint 持有。
        let thumbnail_range_changed = if action != UiAction::Quit
            && panel_visible_after_event()
            && (may_refresh_model || viewport.is_some())
        {
            let (viewport_y, visible_height) =
                UI_THUMBNAIL_VIEWPORT.with(|current| *current.borrow());
            schedule_thumbnail_requests(
                &snapshot,
                current_panel_generation(),
                viewport_y,
                visible_height,
            )
        } else if action == UiAction::Hide {
            clear_thumbnail_runtime_cache();
            false
        } else {
            false
        };
        let preserve_thumbnail_viewport = thumbnail_range_changed && viewport.is_some();
        // Reassert 只重新显示和激活原窗口，不能重建 ListView 模型或扰动滚动状态。
        let refresh_model = ((may_refresh_model && !history_result_event)
            || history_model_refresh != HistoryModelRefresh::None
            || thumbnail_applied
            || thumbnail_range_changed
            || geometry_commit.is_some())
            && action != UiAction::Reassert;

        if action == UiAction::Quit {
            #[cfg(windows)]
            close_copy_request_gate();
            close_history_query_bridge();
            close_pin_mutation_bridge();
            close_delete_mutation_bridge();
            close_clear_history_mutation_bridge();
            clear_thumbnail_runtime_cache();
            UI_THUMBNAIL_LOADER.with(|slot| {
                slot.borrow_mut().take();
            });
            // 退出调用必须在 Slint 事件线程执行，后台 Win32 回调只负责投递事件。
            if let Err(error) = slint::quit_event_loop() {
                eprintln!("退出 Slint 事件循环失败：{error}");
            }
            return;
        }

        if let UiAction::ScheduleSearch { generation } = action {
            schedule_search_debounce(generation);
        }

        let append_revision = match history_model_refresh {
            HistoryModelRefresh::AppendPreservingViewport { append_revision } => append_revision,
            HistoryModelRefresh::None | HistoryModelRefresh::Replace => None,
        };
        let preserve_append_viewport = matches!(
            history_model_refresh,
            HistoryModelRefresh::AppendPreservingViewport { .. }
        );
        let mut append_probe_window = None;
        let mut exhausted_append_window = None;
        UI_WINDOW.with(|target| {
            let weak_window = target.borrow().clone();
            let Some(window) = weak_window.and_then(|weak| weak.upgrade()) else {
                if let Some(revision) = append_revision {
                    cancel_pending_post_append_probe(revision);
                } else if preserve_append_viewport {
                    finish_exhausted_append_binding();
                }
                return;
            };

            set_window_search_state(&window, &search_text, search_filter, search_status);
            window.set_startup_status(SharedString::from(startup_status.clone()));
            window.set_history_next_page_loading(history_next_page_loading);
            window.set_history_retry_required(history_retry_required);
            window.set_pin_error_visible(pin_error_visible);
            window.set_delete_error_visible(delete_error_visible);
            window.set_clear_unpinned_confirmation_visible(clear_unpinned_confirmation_visible);
            window.set_clear_unpinned_pending(
                pending_clear_history_mutation
                    .as_ref()
                    .is_some_and(|request| request.scope == ClearHistoryScope::UnpinnedText),
            );
            window.set_clear_unpinned_error_visible(clear_unpinned_error_visible);
            window.set_clear_all_confirmation_visible(clear_all_confirmation_visible);
            window.set_clear_all_confirmation_text(clear_all_confirmation_text.into());
            window.set_clear_all_pending(
                pending_clear_history_mutation
                    .as_ref()
                    .is_some_and(|request| request.scope == ClearHistoryScope::All),
            );
            window.set_clear_all_error_visible(clear_all_error_visible);
            window.set_history_mutation_pending(
                pending_pin_mutation.is_some()
                    || pending_delete_mutation.is_some()
                    || pending_clear_history_mutation.is_some(),
            );

            // 只有快照、捕获或显示事件才刷新轻量卡片模型；选择事件复用现有模型。
            if refresh_model {
                let retained_viewport_y =
                    (thumbnail_applied || preserve_append_viewport || preserve_thumbnail_viewport)
                        .then(|| window.get_history_viewport_y());
                let geometry_applied = geometry_commit.as_ref().is_some_and(|commit| {
                    apply_window_commit(
                        &window,
                        commit,
                        pending_pin_mutation.as_ref(),
                        pending_delete_mutation.as_ref(),
                    )
                });
                if !geometry_applied {
                    // 显式提交的量化或身份校验失败时保留可用 legacy 画面，不能留下空模型。
                    set_window_snapshot(
                        &window,
                        &snapshot,
                        pending_pin_mutation.as_ref(),
                        pending_delete_mutation.as_ref(),
                        false,
                    );
                }
                if let Some(viewport_y) = retained_viewport_y {
                    window.set_history_viewport_y(viewport_y);
                }
                if preserve_append_viewport && append_revision.is_none() {
                    // Setter 返回不代表延迟布局回调结束；门禁必须保留到下一 UI 闭包。
                    exhausted_append_window = Some(window.as_weak());
                }
            }
            // 选中视觉和右栏投影都由完整快照的单一索引驱动；Slint 不另存选择事实。
            window.set_selected_index(
                snapshot
                    .selected_index
                    .and_then(|index| i32::try_from(index).ok())
                    .unwrap_or(-1),
            );
            set_selected_card_projection(
                &window,
                &snapshot,
                pending_pin_mutation.as_ref(),
                pending_delete_mutation.as_ref(),
            );
            if refresh_model
                && !thumbnail_applied
                && !preserve_append_viewport
                && !preserve_thumbnail_viewport
            {
                ensure_selection_visible(&window, &snapshot);
            }
            if append_revision.is_some() {
                append_probe_window = Some(window.as_weak());
            }

            if let UiAction::QueueCopy { id, content_hash } = action {
                #[cfg(windows)]
                request_copy_item(id, content_hash);
            }

            match action {
                UiAction::Show => {
                    let show_generation = current_panel_generation();
                    let show_result = perform_show_action(
                        || window.show(),
                        || {
                            ensure_selection_visible(&window, &snapshot);
                            #[cfg(windows)]
                            {
                                let positioned = position_panel(&window);
                                let activated = activate_panel();
                                if !positioned || !activated {
                                    schedule_panel_activation(
                                        &window,
                                        current_panel_generation(),
                                        3,
                                        true,
                                    );
                                }
                            }
                        },
                    );
                    if let Err(error) = show_result {
                        // 失败时按代次回滚可见会话；下一次热键必须重新执行 Show。
                        UI_STATE.with(|state| {
                            state.borrow_mut().mark_panel_show_failed(show_generation);
                        });
                        eprintln!("无法显示剪贴板看板：{error}");
                    }
                }
                UiAction::Reassert => {
                    let show_result = perform_show_action(
                        || window.show(),
                        || {
                            #[cfg(windows)]
                            {
                                let positioned = reassert_panel_topmost();
                                let activated = activate_panel();
                                if !positioned || !activated {
                                    schedule_panel_activation(
                                        &window,
                                        current_panel_generation(),
                                        3,
                                        false,
                                    );
                                }
                            }
                        },
                    );
                    if let Err(error) = show_result {
                        // Reassert 失败只等待下次热键重试，不能隐藏或重建当前面板会话。
                        eprintln!("无法重新激活剪贴板看板：{error}");
                    }
                }
                UiAction::Hide => {
                    if let Err(error) = window.hide() {
                        eprintln!("无法隐藏剪贴板看板：{error}");
                    }
                }
                UiAction::SelectItem => {}
                UiAction::QueueCopy { .. } => {}
                UiAction::QueuePin(_) => {}
                UiAction::QueueDelete(_) => {}
                UiAction::QueueClearHistory(_) => {}
                UiAction::ScheduleSearch { .. } => {}
                UiAction::None => {}
                // Quit 已在上方提前返回；此分支仅用于让枚举匹配保持显式完整。
                UiAction::Quit => unreachable!("退出动作必须在窗口副作用前处理"),
            }
        });

        if let (Some(revision), Some(weak_window)) = (append_revision, append_probe_window) {
            schedule_post_append_probe(weak_window, revision);
        } else if let Some(weak_window) = exhausted_append_window {
            schedule_exhausted_append_binding_completion(weak_window);
        }
    })
}

/// 按修订收口一次 probe 调度结果；失败只解除门禁，不回滚已接受的历史页。
fn settle_post_append_probe_dispatch<E, C>(
    append_revision: u64,
    result: Result<(), E>,
    cancel: C,
) -> Result<(), E>
where
    C: FnOnce(u64),
{
    if result.is_err() {
        cancel(append_revision);
    }
    result
}

/// 在全局 UI 状态中精确取消一个尚未送达的绑定后探针。
fn cancel_pending_post_append_probe(append_revision: u64) {
    UI_STATE.with(|state| {
        state.borrow_mut().cancel_post_append_probe(append_revision);
    });
}

/// 在全局 UI 状态中完成一次没有可投递修订的 Append 绑定。
fn finish_exhausted_append_binding() {
    UI_STATE.with(|state| {
        state.borrow_mut().finish_exhausted_append_binding();
    });
}

/// 在下一 UI 闭包只解除修订耗尽门禁；不得读取几何或自动签发续页。
fn schedule_exhausted_append_binding_completion(window: slint::Weak<AppWindow>) {
    let scheduled = slint::invoke_from_event_loop(move || {
        // 窗口在等待期间消失也必须解除门禁；弱引用只证明该任务来自真实绑定路径。
        let _window_still_exists = window.upgrade().is_some();
        finish_exhausted_append_binding();
    });
    if let Err(error) = scheduled {
        // 调度失败不能留下永久门禁；此时没有下一闭包可等待，只能立即安全收口。
        finish_exhausted_append_binding();
        eprintln!("历史追加耗尽门禁无法安排到 UI 事件循环：{error}");
    }
}

/// 把绑定完成后的几何读取安排到下一 UI 闭包，再投递唯一带修订探针。
fn schedule_post_append_probe(window: slint::Weak<AppWindow>, append_revision: u64) {
    let scheduled = slint::invoke_from_event_loop(move || {
        let Some(window) = window.upgrade() else {
            cancel_pending_post_append_probe(append_revision);
            return;
        };
        let probe = UiEvent::HistoryPostAppendProbe {
            append_revision,
            viewport_y: window.get_history_viewport_y().round() as i32,
            visible_height: window.get_history_visible_height().round() as i32,
            content_height: window.get_history_viewport_height().round() as i32,
        };
        let delivered = post_ui_event(probe);
        if let Err(error) = settle_post_append_probe_dispatch(
            append_revision,
            delivered,
            cancel_pending_post_append_probe,
        ) {
            eprintln!("历史追加探针无法进入 UI 事件队列：{error}");
        }
    });
    if let Err(error) = settle_post_append_probe_dispatch(
        append_revision,
        scheduled,
        cancel_pending_post_append_probe,
    ) {
        eprintln!("历史追加探针无法安排到 UI 事件循环：{error}");
    }
}

/// 判断事件是否可能改变卡片模型；纯确认状态变化复用现有列表模型。
fn event_may_refresh_model(event: &UiEvent) -> bool {
    !matches!(
        event,
        UiEvent::ThumbnailLoaded(_)
            | UiEvent::SelectItem { .. }
            | UiEvent::CopyItem { .. }
            | UiEvent::HistoryViewportChanged { .. }
            | UiEvent::HistoryViewportChangedDuringAppend { .. }
            | UiEvent::HistoryPostAppendProbe { .. }
            | UiEvent::HistoryWindowViewportChanged { .. }
            | UiEvent::RetryHistoryPage
            | UiEvent::ClearUnpinnedRequested
            | UiEvent::ClearUnpinnedCancelled
            | UiEvent::ClearUnpinnedConfirmed { .. }
            | UiEvent::ClearAllRequested
            | UiEvent::ClearAllConfirmationTextChanged(_)
            | UiEvent::ClearAllCancelled
            | UiEvent::ClearAllConfirmed { .. }
    )
}

/// 接受当前会话仍存在的缩略图结果，并在 UI 线程构造 Slint 图片。
fn apply_thumbnail_result(result: &ThumbnailLoadResult, snapshot: &UiSnapshot) -> bool {
    // 每个结果都结束对应在途身份；即使已滚出视口，滚回后也必须允许重新加载。
    UI_THUMBNAIL_REQUESTED.with(|requested| {
        requested
            .borrow_mut()
            .remove(&(result.panel_generation, result.id, result.content_hash));
    });
    let current = UI_STATE.with(|state| {
        let state = state.borrow();
        state.panel_visible
            && state.panel_generation == result.panel_generation
            && UI_THUMBNAIL_VISIBLE
                .with(|visible| visible.borrow().contains(&(result.id, result.content_hash)))
            && snapshot
                .items
                .iter()
                .any(|item| item.id == result.id && item.content_hash == result.content_hash)
    });
    if !current {
        return false;
    }
    let identity = (result.id, result.content_hash);
    let Ok(pixels) = &result.outcome else {
        reserve_thumbnail_cache_slot(identity);
        UI_THUMBNAIL_FAILED.with(|failed| {
            failed.borrow_mut().insert(identity);
        });
        return true;
    };
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(pixels.width, pixels.height);
    if buffer.make_mut_bytes().len() != pixels.rgba.len() {
        return false;
    }
    buffer.make_mut_bytes().copy_from_slice(&pixels.rgba);
    let image = Image::from_rgba8(buffer);
    reserve_thumbnail_cache_slot(identity);
    UI_THUMBNAIL_CACHE.with(|cache| {
        cache.borrow_mut().insert(identity, image);
    });
    UI_THUMBNAIL_FAILED.with(|failed| {
        failed.borrow_mut().remove(&identity);
    });
    true
}

/// 释放保留范围外的 UI 图片、失败状态和 LRU 顺序。
///
/// `ClipboardCard` 会克隆 `slint::Image`，所以缓存清理必须和模型重建配套执行；
/// 调用方在返回 `true` 时应保存视口并重新绑定卡片模型，切断范围外的最后一个引用。
fn trim_thumbnail_cache_to_active(active: &HashSet<(u64, [u8; 32])>) -> bool {
    let mut changed = false;
    UI_THUMBNAIL_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let before = cache.len();
        cache.retain(|identity, _| active.contains(identity));
        changed |= cache.len() != before;
    });
    UI_THUMBNAIL_FAILED.with(|failed| {
        let mut failed = failed.borrow_mut();
        let before = failed.len();
        failed.retain(|identity| active.contains(identity));
        changed |= failed.len() != before;
    });
    UI_THUMBNAIL_CACHE_ORDER.with(|order| {
        let mut order = order.borrow_mut();
        let before = order.len();
        order.retain(|identity| active.contains(identity));
        changed |= order.len() != before;
    });
    changed
}

/// 清空当前面板的所有缩略图运行时状态，隐藏或退出后不保留 UI 纹理。
fn clear_thumbnail_runtime_cache() {
    UI_THUMBNAIL_CACHE.with(|cache| cache.borrow_mut().clear());
    UI_THUMBNAIL_FAILED.with(|failed| failed.borrow_mut().clear());
    UI_THUMBNAIL_CACHE_ORDER.with(|order| order.borrow_mut().clear());
    UI_THUMBNAIL_VISIBLE.with(|visible| visible.borrow_mut().clear());
    UI_THUMBNAIL_REQUESTED.with(|requested| requested.borrow_mut().clear());
}

/// 将缓存命中身份移动到队尾，形成最近最少使用顺序。
fn touch_thumbnail_cache(identity: (u64, [u8; 32])) {
    UI_THUMBNAIL_CACHE_ORDER.with(|order| {
        let mut order = order.borrow_mut();
        order.retain(|candidate| *candidate != identity);
        order.push_back(identity);
    });
}

/// 为新结果预留一个有界槽位；满载时从 LRU 队首逐项淘汰。
fn reserve_thumbnail_cache_slot(identity: (u64, [u8; 32])) {
    UI_THUMBNAIL_CACHE_ORDER.with(|order| {
        let mut order = order.borrow_mut();
        order.retain(|candidate| *candidate != identity);
        if order.len() >= THUMBNAIL_CACHE_CAPACITY {
            if let Some(evicted) = order.pop_front() {
                UI_THUMBNAIL_CACHE.with(|cache| {
                    cache.borrow_mut().remove(&evicted);
                });
                UI_THUMBNAIL_FAILED.with(|failed| {
                    failed.borrow_mut().remove(&evicted);
                });
            }
        }
        order.push_back(identity);
    });
}

/// 按混合卡片固定高度计算当前可视区及其前后缓冲条目。
fn thumbnail_retained_range(
    snapshot: &UiSnapshot,
    viewport_y: i32,
    visible_height: i32,
) -> std::ops::Range<usize> {
    let items = visible_snapshot_items(snapshot).collect::<Vec<_>>();
    if items.is_empty() || visible_height <= 0 {
        return 0..0;
    }
    let top = viewport_y.saturating_neg().max(0);
    let bottom = top.saturating_add(visible_height);
    let mut item_top = 0_i32;
    let mut first_visible = None;
    let mut last_visible = None;
    for (index, item) in items.iter().enumerate() {
        let item_height = match item.kind {
            crate::command::UiClipboardItemKind::Text => TEXT_HISTORY_ROW_HEIGHT,
            crate::command::UiClipboardItemKind::Image(_) => IMAGE_HISTORY_ROW_HEIGHT,
        };
        let item_bottom = item_top.saturating_add(item_height);
        if item_bottom > top && item_top < bottom {
            first_visible.get_or_insert(index);
            last_visible = Some(index);
        }
        if item_top >= bottom {
            break;
        }
        item_top = item_bottom;
    }
    let Some(first_visible) = first_visible else {
        return 0..0;
    };
    let last_visible = last_visible.unwrap_or(first_visible);
    let start = first_visible.saturating_sub(THUMBNAIL_ITEM_BUFFER);
    let end = last_visible
        .saturating_add(THUMBNAIL_ITEM_BUFFER + 1)
        .min(items.len());
    start..end
}

/// 按当前保留范围提交缩略图请求，并返回是否需要重建卡片模型。
fn schedule_thumbnail_requests(
    snapshot: &UiSnapshot,
    panel_generation: u64,
    viewport_y: i32,
    visible_height: i32,
) -> bool {
    let panel_visible = UI_STATE.with(|state| state.borrow().panel_visible);
    if !panel_visible || panel_generation == 0 || visible_height <= 0 {
        let changed = UI_THUMBNAIL_VISIBLE.with(|visible| {
            let mut visible = visible.borrow_mut();
            let changed = !visible.is_empty();
            visible.clear();
            changed
        });
        let had_cache = UI_THUMBNAIL_CACHE.with(|cache| !cache.borrow().is_empty())
            || UI_THUMBNAIL_FAILED.with(|failed| !failed.borrow().is_empty())
            || UI_THUMBNAIL_CACHE_ORDER.with(|order| !order.borrow().is_empty());
        clear_thumbnail_runtime_cache();
        return changed || had_cache;
    }
    let items = visible_snapshot_items(snapshot).collect::<Vec<_>>();
    let retained_range = thumbnail_retained_range(snapshot, viewport_y, visible_height);
    let active = items
        .iter()
        .enumerate()
        .filter(|(index, item)| {
            retained_range.contains(index)
                && matches!(item.kind, crate::command::UiClipboardItemKind::Image(_))
        })
        .map(|(_, item)| (item.id, item.content_hash))
        .collect::<HashSet<_>>();
    let range_changed = UI_THUMBNAIL_VISIBLE.with(|visible| {
        let mut visible = visible.borrow_mut();
        let changed = *visible != active;
        *visible = active.clone();
        changed
    });
    let cache_pruned = trim_thumbnail_cache_to_active(&active);
    UI_THUMBNAIL_REQUESTED.with(|requested| {
        requested
            .borrow_mut()
            .retain(|(generation, _, _)| *generation == panel_generation);
    });
    for (index, item) in items.into_iter().enumerate() {
        if index < retained_range.start {
            continue;
        }
        if index >= retained_range.end {
            break;
        }
        let Some(path) = (match &item.kind {
            crate::command::UiClipboardItemKind::Text => None,
            crate::command::UiClipboardItemKind::Image(image) => Some(image.thumbnail_path.clone()),
        }) else {
            continue;
        };
        let identity = (item.id, item.content_hash);
        let cache_hit = UI_THUMBNAIL_CACHE.with(|cache| cache.borrow().contains_key(&identity));
        let failed_hit = UI_THUMBNAIL_FAILED.with(|failed| failed.borrow().contains(&identity));
        if cache_hit || failed_hit {
            touch_thumbnail_cache(identity);
            continue;
        }
        let request_key = (panel_generation, item.id, item.content_hash);
        let already_requested =
            UI_THUMBNAIL_REQUESTED.with(|requested| requested.borrow().contains(&request_key));
        if !already_requested {
            let request = ThumbnailLoadRequest {
                panel_generation,
                id: item.id,
                content_hash: item.content_hash,
                path,
            };
            let submitted = UI_THUMBNAIL_LOADER.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .is_some_and(|sender| sender.try_submit(request).is_ok())
            });
            if submitted {
                UI_THUMBNAIL_REQUESTED.with(|requested| {
                    requested.borrow_mut().insert(request_key);
                });
            }
        }
    }
    range_changed || cache_pruned
}

#[cfg(windows)]
/// 将 UI reducer 产生的 ID/哈希命令投递给有界复制工作桥；不在 UI 线程读取正文或调用 Win32。
fn request_copy_item(id: u64, content_hash: [u8; 32]) {
    UI_COPY_INBOX.with(|slot| {
        let binding = slot.borrow();
        let Some(inbox) = binding.as_ref() else {
            eprintln!("仅复制请求桥尚未就绪");
            return;
        };
        if let Err(error) = inbox.request_copy(ClipboardCopyRequest::new(id, content_hash)) {
            eprintln!("仅复制请求无法进入工作桥：{error:?}");
        }
    });
}

#[cfg(windows)]
/// UI 接受 Quit 时先线性化关闭复制入口；操作只含原子交换和非阻塞唤醒。
fn close_copy_request_gate() {
    UI_COPY_INBOX.with(|slot| {
        if let Some(inbox) = slot.borrow().as_ref() {
            inbox.close_copy_requests();
        }
    });
}

/// 按内容哈希重定位选中项；去重或容量裁剪后仍优先保持原条目身份。
fn restore_selected_index(snapshot: &mut UiSnapshot, selected_hash: Option<[u8; 32]>) {
    let limit = selection_limit(snapshot);
    if limit == 0 {
        snapshot.selected_index = None;
        return;
    }

    snapshot.selected_index = selected_hash
        .and_then(|hash| {
            snapshot
                .items
                .iter()
                .take(limit)
                .position(|item| item.content_hash == hash)
        })
        .or_else(|| selected_hash.map(|_| limit.saturating_sub(1)));
}

/// 计算当前快照可被窗口选择和构造的条目数量。
fn selection_limit(snapshot: &UiSnapshot) -> usize {
    snapshot.items.len().min(MAX_LOADED_ITEMS)
}

/// 将显式窗口中的局部卡片索引解析为完整提交身份；旧窗口或身份不匹配时直接拒绝。
fn resolve_geometry_card_event(
    state: &UiState,
    index: i32,
    action: WindowCardAction,
) -> Option<UiEvent> {
    if !state.panel_visible {
        return None;
    }
    let local_index = usize::try_from(index).ok()?;
    let window = state.published_window.as_ref()?;
    if !window.validate() || local_index >= window.offsets.len() {
        return None;
    }
    let offset = window.offsets.get(local_index)?;
    let item = state.snapshot.items.get(offset.absolute_index as usize)?;
    if item.id != offset.id || item.content_hash != offset.content_hash {
        return None;
    }
    if matches!(action, WindowCardAction::Copy) && !item.copy_enabled() {
        return None;
    }
    Some(UiEvent::HistoryWindowCardRequested {
        identity: WindowEventIdentity {
            session_nonce: window.session_nonce,
            dataset_revision: window.dataset_revision,
            window_revision: window.window_revision,
            commit_revision: window.commit_revision,
            commit_checksum: window.commit_checksum,
        },
        absolute_index: offset.absolute_index,
        id: offset.id,
        content_hash: offset.content_hash,
        action,
    })
}

/// 只有会话 nonce、几何元数据和已发布窗口齐全时才启用显式卡片协议；否则回退 legacy。
fn explicit_window_ready(state: &UiState) -> bool {
    state.session_nonce.is_some()
        && state.history_geometry.is_some()
        && state
            .published_window
            .as_ref()
            .is_some_and(WindowCommit::validate)
}

/// 将当前可见卡片索引同步解析为代次绑定的稳定身份；空白区和越界索引直接忽略。
fn resolve_card_selection(index: i32) -> Option<UiEvent> {
    let index = usize::try_from(index).ok()?;
    UI_STATE.with(|state| {
        let state = state.borrow();
        if !state.panel_visible || index >= selection_limit(&state.snapshot) {
            return None;
        }
        if explicit_window_ready(&state) {
            return resolve_geometry_card_event(&state, index as i32, WindowCardAction::Select);
        }
        let item = state.snapshot.items.get(index)?;
        Some(UiEvent::SelectItem {
            panel_generation: state.panel_generation,
            id: item.id,
            content_hash: item.content_hash,
        })
    })
}

/// 将复制按钮索引同步解析为代次绑定的稳定身份；越界或隐藏面板不会产生后台命令。
fn resolve_copy_item(index: i32) -> Option<UiEvent> {
    let index = usize::try_from(index).ok()?;
    UI_STATE.with(|state| {
        let state = state.borrow();
        if !state.panel_visible || index >= selection_limit(&state.snapshot) {
            return None;
        }
        if explicit_window_ready(&state) {
            return resolve_geometry_card_event(&state, index as i32, WindowCardAction::Copy);
        }
        let item = state.snapshot.items.get(index)?;
        if !item.copy_enabled() {
            return None;
        }
        Some(UiEvent::CopyItem {
            panel_generation: state.panel_generation,
            id: item.id,
            content_hash: item.content_hash,
        })
    })
}

/// 将收藏按钮索引同步解析为稳定身份和明确期望状态；已有请求时不产生第二个事件。
fn resolve_pin_item(index: i32) -> Option<UiEvent> {
    let index = usize::try_from(index).ok()?;
    UI_STATE.with(|state| {
        let state = state.borrow();
        if !state.panel_visible
            || state.pending_pin_mutation.is_some()
            || state.pending_delete_mutation.is_some()
            || state.pending_clear_history_mutation.is_some()
            || index >= selection_limit(&state.snapshot)
        {
            return None;
        }
        if explicit_window_ready(&state) {
            let item = state.snapshot.items.get(index)?;
            return resolve_geometry_card_event(
                &state,
                index as i32,
                WindowCardAction::Pin {
                    is_pinned: !item.is_pinned,
                },
            );
        }
        let item = state.snapshot.items.get(index)?;
        Some(UiEvent::PinItem {
            panel_generation: state.panel_generation,
            id: item.id,
            content_hash: item.content_hash,
            is_pinned: !item.is_pinned,
        })
    })
}

/// 将删除按钮索引同步解析为稳定身份；任一历史 mutation 在途时拒绝新请求。
fn resolve_delete_item(index: i32) -> Option<UiEvent> {
    let index = usize::try_from(index).ok()?;
    UI_STATE.with(|state| {
        let state = state.borrow();
        if !state.panel_visible
            || state.pending_pin_mutation.is_some()
            || state.pending_delete_mutation.is_some()
            || state.pending_clear_history_mutation.is_some()
            || index >= selection_limit(&state.snapshot)
        {
            return None;
        }
        if explicit_window_ready(&state) {
            return resolve_geometry_card_event(&state, index as i32, WindowCardAction::Delete);
        }
        let item = state.snapshot.items.get(index)?;
        Some(UiEvent::DeleteItem {
            panel_generation: state.panel_generation,
            id: item.id,
            content_hash: item.content_hash,
        })
    })
}

/// 将领域无关的 UI 快照转换为 Slint 卡片模型；完整正文不会进入此转换层。
fn to_slint_card(
    item: &UiClipboardItem,
    pending_pin: Option<&PinMutationRequest>,
    pending_delete: Option<&DeleteMutationRequest>,
) -> crate::ClipboardCard {
    crate::ClipboardCard {
        preview: SharedString::from(item.preview.as_str()),
        source: SharedString::from(item.source.as_str()),
        relative_time: SharedString::from(item.relative_time.as_str()),
        is_pinned: item.is_pinned,
        pin_pending: pending_pin.is_some_and(|pending| {
            pending.id == item.id && pending.content_hash == item.content_hash
        }),
        delete_pending: pending_delete.is_some_and(|pending| {
            pending.id == item.id && pending.content_hash == item.content_hash
        }),
        is_image: matches!(item.kind, crate::command::UiClipboardItemKind::Image(_)),
        copy_enabled: item.copy_enabled(),
        image_width: match &item.kind {
            crate::command::UiClipboardItemKind::Image(image) => {
                i32::try_from(image.width).unwrap_or(i32::MAX)
            }
            crate::command::UiClipboardItemKind::Text => 0,
        },
        image_height: match &item.kind {
            crate::command::UiClipboardItemKind::Image(image) => {
                i32::try_from(image.height).unwrap_or(i32::MAX)
            }
            crate::command::UiClipboardItemKind::Text => 0,
        },
        thumbnail: UI_THUMBNAIL_CACHE.with(|cache| {
            cache
                .borrow()
                .get(&(item.id, item.content_hash))
                .cloned()
                .unwrap_or_default()
        }),
        thumbnail_loaded: UI_THUMBNAIL_CACHE
            .with(|cache| cache.borrow().contains_key(&(item.id, item.content_hash))),
        thumbnail_failed: UI_THUMBNAIL_FAILED
            .with(|failed| failed.borrow().contains(&(item.id, item.content_hash))),
    }
}

/// 返回完整快照中的当前选中项；不读取 WindowCommit 的局部索引或 Slint 展示字段。
fn selected_item_from_snapshot(snapshot: &UiSnapshot) -> Option<&UiClipboardItem> {
    let selected_index = snapshot.selected_index?;
    if selected_index >= selection_limit(snapshot) {
        return None;
    }
    snapshot.items.get(selected_index)
}

/// 构造无选择时的安全投影；has-selected-card=false 时右栏不得读取其中任何字段。
fn empty_slint_card() -> crate::ClipboardCard {
    crate::ClipboardCard {
        preview: SharedString::default(),
        source: SharedString::default(),
        relative_time: SharedString::default(),
        is_pinned: false,
        pin_pending: false,
        delete_pending: false,
        is_image: false,
        copy_enabled: false,
        image_width: 0,
        image_height: 0,
        thumbnail: slint::Image::default(),
        thumbnail_loaded: false,
        thumbnail_failed: false,
    }
}

/// 从完整快照同步选中投影；先关闭门禁再替换 DTO，避免异步刷新期间暴露旧身份。
fn set_selected_card_projection(
    window: &AppWindow,
    snapshot: &UiSnapshot,
    pending_pin: Option<&PinMutationRequest>,
    pending_delete: Option<&DeleteMutationRequest>,
) {
    let selected_card = selected_item_from_snapshot(snapshot)
        .map(|item| to_slint_card(item, pending_pin, pending_delete));
    let has_selected_card = selected_card.is_some();

    window.set_has_selected_card(false);
    window.set_selected_card(selected_card.unwrap_or_else(empty_slint_card));
    window.set_has_selected_card(has_selected_card);
}

/// 将 Slint length 量化为有限整数像素；NaN/Infinity/越界输入直接丢弃。
fn quantize_slint_length(value: f32) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    let rounded = if value == 0.0 { 0.0 } else { value.round() };
    if rounded < -(MAX_EXACT_SLINT_INTEGER as f32) || rounded > MAX_EXACT_SLINT_INTEGER as f32 {
        return None;
    }
    Some(rounded as i64)
}

/// 将非负整数像素安全转换为 Slint length；无法由 f32 精确表示时拒绝提交。
fn checked_slint_length(value: i64) -> Option<f32> {
    if !(0..=MAX_EXACT_SLINT_INTEGER).contains(&value) {
        return None;
    }
    let converted = value as f32;
    if !converted.is_finite() || converted as i64 != value {
        return None;
    }
    Some(converted)
}

/// 将有符号整数坐标安全转换为 Slint length；显式归一化负零避免回调抖动。
fn checked_slint_coordinate(value: i64) -> Option<f32> {
    if !(-MAX_EXACT_SLINT_INTEGER..=MAX_EXACT_SLINT_INTEGER).contains(&value) {
        return None;
    }
    let converted = value as f32;
    if !converted.is_finite() || converted as i64 != value {
        return None;
    }
    Some(if value == 0 { 0.0 } else { converted })
}

/// legacy set_cards 路径只绑定完整逻辑摘要，混合几何模式随后由 WindowCommit 替换为窗口模型。
fn set_window_snapshot(
    window: &AppWindow,
    snapshot: &UiSnapshot,
    pending_pin: Option<&PinMutationRequest>,
    pending_delete: Option<&DeleteMutationRequest>,
    geometry_mode: bool,
) {
    let cards = visible_snapshot_items(snapshot)
        .map(|item| to_slint_card(item, pending_pin, pending_delete))
        .collect::<Vec<_>>();
    if geometry_mode {
        // 显式模式只允许 bounded WindowCommit 进入 repeater，避免短暂绑定完整模型。
        window.set_cards(ModelRc::new(VecModel::from(
            Vec::<crate::ClipboardCard>::new(),
        )));
    } else {
        window.set_geometry_mode(false);
        window.set_cards(ModelRc::new(VecModel::from(cards)));
    }
    window.set_selected_index(
        snapshot
            .selected_index
            .and_then(|index| i32::try_from(index).ok())
            .unwrap_or(-1),
    );
}

/// 将已验证 WindowCommit 的有界 cards/offsets 按顺序写入精确 Flickable 画布。
fn apply_window_commit(
    window: &AppWindow,
    commit: &WindowCommit,
    pending_pin: Option<&PinMutationRequest>,
    pending_delete: Option<&DeleteMutationRequest>,
) -> bool {
    if !commit.validate() {
        return false;
    }
    let cards = commit
        .cards
        .iter()
        .map(|item| to_slint_card(item, pending_pin, pending_delete))
        .collect::<Vec<_>>();
    let Some(offsets) = commit
        .offsets
        .iter()
        .map(|offset| checked_slint_length(offset.top))
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    let Some(content_height) = checked_slint_length(commit.total_height) else {
        return false;
    };
    let Some(viewport_y) = checked_slint_coordinate(commit.clamped_viewport_y) else {
        return false;
    };
    let Some(window_start) = i32::try_from(commit.start).ok() else {
        return false;
    };
    let Some(window_length) = i32::try_from(commit.length).ok() else {
        return false;
    };
    let Some(logical_count) = i32::try_from(commit.total_count).ok() else {
        return false;
    };
    if !commit.total_height.is_positive() && commit.total_count != 0 {
        return false;
    }
    // 先抑制中间布局回调，再原子替换 cards/offsets/尺寸；最终视口设置后才允许回调携带 token。
    window.set_geometry_events_suppressed(true);
    // 先发布与卡片一一对应的偏移模型，再发布卡片模型；避免 repeater 在偏移模型为空时
    // 创建 delegate，导致首次进入显式 Flickable 时绑定被永久判定为无效。
    window.set_window_offsets(ModelRc::new(VecModel::from(offsets)));
    window.set_window_cards(ModelRc::new(VecModel::from(cards)));
    window.set_window_start(window_start);
    window.set_window_length(window_length);
    window.set_history_logical_count(logical_count);
    window.set_geometry_content_height(content_height);
    window.set_geometry_mode(true);
    // 先把一次性来源 token 写入 UI，再设置视口；由 Slint 回调携带该 token 完成确认。
    if let Some(origin_token) = commit.origin_token {
        window.set_geometry_origin_token(SharedString::from(origin_token.to_string()));
    } else {
        window.set_geometry_origin_token(SharedString::default());
    }
    // 显式模式不再让完整 cards 模型进入 ListView，legacy 属性仍保留给旧调用者；此操作也受抑制保护。
    window.set_cards(ModelRc::new(VecModel::from(
        Vec::<crate::ClipboardCard>::new(),
    )));
    window.set_geometry_events_suppressed(false);
    window.set_geometry_viewport_y(viewport_y);
    true
}

/// 为窄测试和后续调用方登记拥有型几何元数据；不创建任何 Slint 行模型。
pub fn set_history_geometry_metadata(window: &AppWindow, items: Vec<HistoryGeometryItem>) -> bool {
    let Ok(geometry) = HistoryGeometry::new(items) else {
        return false;
    };
    let Some(content_height) = checked_slint_length(geometry.total_height()) else {
        return false;
    };
    let Some(logical_count) = i32::try_from(geometry.len()).ok() else {
        return false;
    };
    window.set_geometry_content_height(content_height);
    window.set_history_logical_count(logical_count);
    window.set_geometry_mode(true);
    true
}

/// 安装已经由 WindowCommitBuilder 校验的 bounded 窗口提交；失败时保留旧窗口。
pub fn set_window_commit(window: &AppWindow, commit: WindowCommit) -> bool {
    if !commit.validate() {
        return false;
    }
    apply_window_commit(window, &commit, None, None)
}

/// 将搜索框、标签和结果状态同步到 Slint；状态文案不携带查询正文之外的内部错误。
fn set_window_search_state(
    window: &AppWindow,
    search_text: &str,
    search_filter: SearchFilter,
    search_status: SearchStatus,
) {
    window.set_search_text(SharedString::from(search_text));
    window.set_search_filter(search_filter.as_index());
    window.set_search_status(SharedString::from(search_status.as_str()));
}

/// 返回窗口模型允许使用的全部已加载摘要；ListView 只实例化视口附近的 delegate。
fn visible_snapshot_items(snapshot: &UiSnapshot) -> impl Iterator<Item = &UiClipboardItem> {
    snapshot.items.iter().take(MAX_LOADED_ITEMS)
}

/// 根据已计算的混合卡片边界求保持选中项可见所需的负向视口偏移。
fn selection_viewport_y(
    item_top: f32,
    item_bottom: f32,
    current_viewport_y: f32,
    visible_height: f32,
    content_height: f32,
) -> f32 {
    if visible_height <= 0.0 || content_height <= visible_height {
        return 0.0;
    }

    let max_offset = (content_height - visible_height).max(0.0);
    let current_offset = (-current_viewport_y).clamp(0.0, max_offset);
    let target_offset = if item_top < current_offset {
        item_top
    } else if item_bottom > current_offset + visible_height {
        item_bottom - visible_height
    } else {
        current_offset
    };

    -target_offset.clamp(0.0, max_offset)
}

/// 累加文本和图片的已知固定高度，返回指定索引的精确上下边界。
fn selection_item_bounds(snapshot: &UiSnapshot, selected_index: usize) -> Option<(f32, f32)> {
    let mut top = 0_f32;
    for (index, item) in visible_snapshot_items(snapshot).enumerate() {
        let height = match item.kind {
            crate::command::UiClipboardItemKind::Text => TEXT_HISTORY_ROW_HEIGHT as f32,
            crate::command::UiClipboardItemKind::Image(_) => IMAGE_HISTORY_ROW_HEIGHT as f32,
        };
        if index == selected_index {
            return Some((top, top + height));
        }
        top += height;
    }
    None
}

/// 在 UI 线程把选中卡片滚入视口；空列表或尚未完成布局时安全回到顶部。
fn ensure_selection_visible(window: &AppWindow, snapshot: &UiSnapshot) {
    let Some(selected_index) = snapshot.selected_index else {
        window.set_history_viewport_y(0.0);
        return;
    };
    let Some((item_top, item_bottom)) = selection_item_bounds(snapshot, selected_index) else {
        return;
    };

    let target = selection_viewport_y(
        item_top,
        item_bottom,
        window.get_history_viewport_y(),
        window.get_history_visible_height(),
        window.get_history_viewport_height(),
    );
    window.set_history_viewport_y(target);
}

/// 为当前搜索代次安排一次 120 ms 防抖事件；计时器回调只投递事件，不直接触碰 reducer。
fn schedule_search_debounce(generation: u64) {
    slint::Timer::single_shot(crate::search::DEFAULT_SEARCH_DEBOUNCE, move || {
        if let Err(error) = post_ui_event(UiEvent::SearchDebounceElapsed { generation }) {
            eprintln!("搜索防抖事件无法进入 UI 事件队列：{error}");
        }
    });
}

/// 读取当前面板代次；Slint 回调在 UI 线程运行，因此不需要跨线程锁。
pub fn current_panel_generation() -> u64 {
    UI_STATE.with(|state| state.borrow().panel_generation())
}

/// 读取本次事件应用后的面板可见状态；缓存调度只能在可见会话中运行。
fn panel_visible_after_event() -> bool {
    UI_STATE.with(|state| state.borrow().panel_visible)
}

#[cfg(windows)]
/// 在窗口真正显示后设置物理坐标，避免部分 Windows 后端用默认位置覆盖预定位结果。
fn position_panel(window: &AppWindow) -> bool {
    // Slint 窗口的 HWND 在首次 show 后才稳定；每次定位顺便重试标题栏图标，
    // 这样异步创建窗口时也能在后续激活重试中完成设置。
    let _ = apply_panel_icon();
    let slint_size = window.window().size();
    let (width, height) = panel_size().unwrap_or((slint_size.width, slint_size.height));
    if let Some(area) = cursor_work_area() {
        let position = center_position(area, width, height);
        // Winit 的首次显示可能异步创建 HWND；找到原生窗口后只保留一个物理位置来源。
        let moved = move_panel(position);
        if moved {
            return true;
        }
        // HWND 尚未创建时先写入 Slint 属性，定时器下一轮会用 Win32 位置覆盖它。
        window
            .window()
            .set_position(PhysicalPosition::new(position.x, position.y));
    }
    false
}

#[cfg(windows)]
/// 在面板 HWND 真正创建后重试物理定位和激活，并用代次拒绝旧会话定时器。
fn schedule_panel_activation(
    window: &AppWindow,
    generation: u64,
    remaining_attempts: u8,
    reposition: bool,
) {
    let weak_window = window.as_weak();
    slint::Timer::single_shot(Duration::from_millis(16), move || {
        let is_current = UI_STATE.with(|state| {
            let state = state.borrow();
            state.panel_visible && state.panel_generation() == generation
        });
        if !is_current {
            return;
        }

        let Some(window) = weak_window.upgrade() else {
            return;
        };
        match activation_attempt(
            || {
                if reposition {
                    position_panel(&window)
                } else {
                    reassert_panel_topmost()
                }
            },
            activate_panel,
            remaining_attempts,
        ) {
            ActivationAttempt::Done => {}
            ActivationAttempt::Retry => {
                schedule_panel_activation(&window, generation, remaining_attempts - 1, reposition);
            }
            ActivationAttempt::TopmostRejected => {
                // SetWindowPos 失败不能误报成功；保留可见状态并等待下一次热键重新断言。
                eprintln!("Windows 暂未允许剪贴板看板保持置顶");
            }
            ActivationAttempt::ActivationRejected => {
                // Windows 前台锁拒绝激活时只记录固定诊断；面板仍保持可见并等待用户点击。
                eprintln!("Windows 暂未允许激活剪贴板看板");
            }
        }
    });
}

/// 读取当前线程的 UI 状态快照；生产调用方应只在 UI 事件闭包内使用此函数。
pub fn ui_state_snapshot() -> UiStateSnapshot {
    UI_STATE.with(|state| state.borrow().snapshot())
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证面板代次协议，确保旧的关闭事件不会误关闭新面板。

    #[cfg(windows)]
    use super::{activation_attempt, ActivationAttempt};
    use super::{
        apply_thumbnail_result, bind_clear_history_mutation_sender, checked_slint_coordinate,
        checked_slint_length, close_clear_history_mutation_bridge, event_may_refresh_model,
        explicit_window_ready, perform_show_action, quantize_slint_length,
        reserve_thumbnail_cache_slot, resolve_geometry_card_event, schedule_thumbnail_requests,
        selection_item_bounds, selection_viewport_y, settle_post_append_probe_dispatch,
        thumbnail_retained_range, touch_thumbnail_cache, visible_snapshot_items, AppendBindingGate,
        HistoryModelRefresh, UiAction, UiState, CLEAR_ALL_CONFIRMATION_PHRASE,
        HISTORY_BOTTOM_ENTER_THRESHOLD, HISTORY_BOTTOM_EXIT_THRESHOLD, MAX_EXACT_SLINT_INTEGER,
        THUMBNAIL_CACHE_CAPACITY, THUMBNAIL_ITEM_BUFFER, UI_FIRST_BATCH_SIZE,
        UI_HISTORY_MEMORY_CAPACITY,
    };
    use crate::command::{
        SearchFilter, SearchStatus, UiClipboardItem, UiClipboardItemKind, UiEvent, UiImageSummary,
        UiSnapshot, WindowCardAction, WindowEventIdentity,
    };
    use crate::history_mutation::{
        clear_history_mutation_channel, ClearHistoryMutationFailure, ClearHistoryMutationRequest,
        ClearHistoryMutationResult, ClearHistoryMutationSubmitError, ClearHistoryMutationSuccess,
        ClearHistoryScope, DeleteMutationFailure, DeleteMutationResult, PinMutationFailure,
        PinMutationResult,
    };
    use crate::history_query::{HistoryPageResult, HistoryQueryFailure, UiHistoryPage};
    use crate::thumbnail_loader::{ThumbnailLoadFailure, ThumbnailLoadResult};
    use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
    use std::time::{Duration, Instant};

    /// 构造具有稳定哈希的测试卡片，便于验证首批边界而不混入重复去重行为。
    fn test_item(index: usize) -> UiClipboardItem {
        UiClipboardItem {
            id: index as u64 + 1,
            preview: format!("条目-{index}"),
            source: "测试来源".to_owned(),
            relative_time: "刚刚".to_owned(),
            content_hash: [index as u8; 32],
            copy_count: 1,
            is_pinned: false,
            kind: Default::default(),
        }
    }

    /// 开机启动反馈只把稳定文案写入 UI 快照，不携带路径或底层错误详情。
    #[cfg(windows)]
    #[test]
    fn startup_status_feedback_is_visible_and_sanitized() {
        let mut state = UiState::default();
        state.apply(UiEvent::StartupStatus {
            transaction_id: std::num::NonZeroU64::new(1).unwrap(),
            generation: 1,
            kind: crate::platform::windows::startup::StartupResultKind::Status(
                crate::platform::windows::startup::EffectiveStartupState::Enabled,
            ),
        });
        assert_eq!(state.snapshot().startup_status, "开机启动：已启用");

        // 旧事务即使晚到，也不能覆盖较新的 Busy/Retry 反馈。
        state.apply(UiEvent::StartupStatus {
            transaction_id: std::num::NonZeroU64::new(2).unwrap(),
            generation: 1,
            kind: crate::platform::windows::startup::StartupResultKind::Busy,
        });
        state.apply(UiEvent::StartupStatus {
            transaction_id: std::num::NonZeroU64::new(1).unwrap(),
            generation: 1,
            kind: crate::platform::windows::startup::StartupResultKind::Applied,
        });
        assert_eq!(state.snapshot().startup_status, "开机启动处理中");
    }

    /// 构造带缩略图路径的图片摘要，测试不读取真实图片文件。
    fn test_image_item(index: usize) -> UiClipboardItem {
        let mut item = test_item(index);
        item.kind = UiClipboardItemKind::Image(UiImageSummary {
            thumbnail_path: std::path::PathBuf::from(format!("C:/thumb-{index}.webp")),
            width: 320,
            height: 200,
        });
        item
    }

    /// 选中投影必须按完整快照的绝对索引解析，并在无选择或越界时安全关闭。
    #[test]
    fn 选中投影只来自完整快照() {
        let snapshot = UiSnapshot {
            items: (0..85).map(test_item).collect(),
            selected_index: Some(84),
        };

        let selected =
            super::selected_item_from_snapshot(&snapshot).expect("完整快照中的选中项应存在");
        assert_eq!(selected.id, 85);
        assert_eq!(selected.preview, "条目-84");

        let mut no_selection = snapshot.clone();
        no_selection.selected_index = None;
        assert!(super::selected_item_from_snapshot(&no_selection).is_none());

        let mut out_of_range = snapshot;
        out_of_range.selected_index = Some(85);
        assert!(super::selected_item_from_snapshot(&out_of_range).is_none());
    }

    /// 将测试摘要包装成带存储修订号的持久化捕获事件。
    fn captured(item: UiClipboardItem, mutation_revision: u64) -> UiEvent {
        UiEvent::ClipboardCaptured {
            item,
            mutation_revision,
        }
    }

    /// 取出当前请求并模拟 worker 返回成功首页。
    fn apply_success_page(state: &mut UiState, items: Vec<UiClipboardItem>) {
        let request = state
            .take_pending_history_request()
            .expect("当前事件应生成 SQLite 首页请求");
        state.apply_history_page_result(HistoryPageResult {
            generation: request.generation,
            token: request.token,
            requested_cursor: request.query.cursor,
            outcome: Ok(UiHistoryPage {
                items,
                next_cursor: None,
            }),
        });
    }

    /// 把指定请求包装成可注入 reducer 的首页结果。
    fn page_result(
        request: &crate::history_query::HistoryPageRequest,
        outcome: Result<UiHistoryPage, HistoryQueryFailure>,
    ) -> HistoryPageResult {
        HistoryPageResult {
            generation: request.generation,
            token: request.token,
            requested_cursor: request.query.cursor,
            outcome,
        }
    }

    /// 构造已经成功追加且等待绑定后探针的状态，返回唯一追加修订。
    fn state_with_pending_append_probe() -> (UiState, u64) {
        let mut state = UiState::default();
        state.apply(UiEvent::OpenPanel);
        let first = state.take_pending_history_request().unwrap();
        state.apply_history_page_result(page_result(
            &first,
            Ok(UiHistoryPage {
                items: (0..30).map(test_item).collect(),
                next_cursor: Some(crate::storage::HistoryCursor {
                    copied_at: 70,
                    id: 30,
                }),
            }),
        ));
        state.apply(UiEvent::HistoryViewportChanged {
            viewport_y: -2_800,
            visible_height: 212,
            content_height: 3_180,
        });
        let next = state.take_pending_history_request().unwrap();
        let refresh = state.apply_history_page_result(page_result(
            &next,
            Ok(UiHistoryPage {
                items: vec![test_item(30)],
                next_cursor: Some(crate::storage::HistoryCursor {
                    copied_at: 60,
                    id: 31,
                }),
            }),
        ));
        let HistoryModelRefresh::AppendPreservingViewport {
            append_revision: Some(revision),
        } = refresh
        else {
            panic!("成功续页必须登记绑定后探针");
        };
        (state, revision)
    }

    /// 读取已发布窗口的完整身份；测试事件不能只携带 local index。
    fn window_identity(window: &crate::command::WindowCommit) -> WindowEventIdentity {
        WindowEventIdentity {
            session_nonce: window.session_nonce,
            dataset_revision: window.dataset_revision,
            window_revision: window.window_revision,
            commit_revision: window.commit_revision,
            commit_checksum: window.commit_checksum,
        }
    }

    /// 显式窗口回调只接受当前身份和一次性 origin token，旧提交/重复 token 必须丢弃。
    #[test]
    fn 显式视口事件使用一次性来源令牌并隔离迟到回调() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: (0..8).map(test_item).collect(),
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        state.history_visible_height = 50;
        state.history_viewport_y = -10_000;
        let first = state.build_window_commit().expect("应发布首个显式窗口");
        let first_identity = window_identity(&first);
        let token = first.origin_token.expect("越界视口必须产生来源令牌");
        state.apply(UiEvent::HistoryWindowViewportChanged {
            identity: first_identity,
            viewport_y: first.clamped_viewport_y,
            visible_height: first.visible_height,
            origin_token: Some(token),
        });
        assert_eq!(state.pending_origin_token, None);
        let accepted_viewport = state.history_viewport_y;

        // 同一提交的重复来源回调已经没有 pending token，必须被拒绝。
        state.apply(UiEvent::HistoryWindowViewportChanged {
            identity: first_identity,
            viewport_y: 0,
            visible_height: first.visible_height,
            origin_token: Some(token),
        });
        assert_eq!(state.history_viewport_y, accepted_viewport);

        // 用户滚动后发布新窗口，旧身份事件不得改变新窗口视口。
        state.apply(UiEvent::HistoryWindowViewportChanged {
            identity: first_identity,
            viewport_y: -106,
            visible_height: first.visible_height,
            origin_token: None,
        });
        let second = state.build_window_commit().expect("用户滚动后应发布新窗口");
        assert_ne!(first.commit_checksum, second.commit_checksum);
        state.apply(UiEvent::HistoryWindowViewportChanged {
            identity: first_identity,
            viewport_y: 0,
            visible_height: first.visible_height,
            origin_token: None,
        });
        assert_eq!(state.history_viewport_y, second.clamped_viewport_y);
    }

    /// 显式卡片事件验证绝对索引、ID/哈希和提交身份后再执行操作。
    #[test]
    fn 显式卡片事件使用窗口身份而非局部索引() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: (0..3).map(test_item).collect(),
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);
        let commit = state.build_window_commit().expect("应发布显式窗口");
        let offset = commit.offsets[1].clone();
        let identity = window_identity(&commit);

        assert_eq!(
            state.apply(UiEvent::HistoryWindowCardRequested {
                identity,
                absolute_index: offset.absolute_index,
                id: offset.id,
                content_hash: offset.content_hash,
                action: WindowCardAction::Select,
            }),
            UiAction::SelectItem
        );
        assert_eq!(state.snapshot.selected_index, Some(1));

        let mut stale = identity;
        stale.commit_checksum[0] ^= 1;
        assert_eq!(
            state.apply(UiEvent::HistoryWindowCardRequested {
                identity: stale,
                absolute_index: offset.absolute_index,
                id: offset.id,
                content_hash: offset.content_hash,
                action: WindowCardAction::Select,
            }),
            UiAction::None
        );
    }

    /// 数据集替换必须先失效旧 WindowCommit，避免几何构造失败或 UI 更新间隙接受迟到卡片。
    #[test]
    fn 数据集变更先隔离旧窗口提交() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: (0..3).map(test_item).collect(),
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);
        let _ = state.build_window_commit().expect("首个窗口应发布");
        assert!(state.published_window.is_some());
        state.snapshot.items.push(test_item(99));
        state.refresh_history_geometry();
        assert!(state.published_window.is_none());
        assert!(state.pending_origin_token.is_none());
        assert!(!explicit_window_ready(&state));
    }

    /// Slint f32 转换必须在连续整数精确范围内；极值、非有限值和负零不能绕过门禁。
    #[test]
    fn slint_几何量化在极值和非有限输入时关闭() {
        assert_eq!(quantize_slint_length(f32::NAN), None);
        assert_eq!(quantize_slint_length(f32::INFINITY), None);
        assert_eq!(quantize_slint_length(f32::NEG_INFINITY), None);
        assert_eq!(quantize_slint_length(-0.0), Some(0));
        assert_eq!(
            checked_slint_length(MAX_EXACT_SLINT_INTEGER),
            Some(16_777_216.0)
        );
        assert_eq!(checked_slint_length(MAX_EXACT_SLINT_INTEGER + 1), None);
        assert_eq!(checked_slint_length(i64::MAX), None);
        assert_eq!(
            checked_slint_coordinate(-MAX_EXACT_SLINT_INTEGER),
            Some(-16_777_216.0)
        );
        assert_eq!(checked_slint_coordinate(MAX_EXACT_SLINT_INTEGER + 1), None);
        assert_eq!(checked_slint_coordinate(i64::MIN), None);
    }

    /// 窗口滚动到非零起点后，四类卡片操作都必须从 local index 映射到同一绝对身份。
    #[test]
    fn 显式卡片回调在非零窗口起点保持四操作身份() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: (0..20).map(test_item).collect(),
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);
        state.history_visible_height = 50;
        state.history_viewport_y = -1_800;
        let commit = state.build_window_commit().expect("应发布中部窗口");
        assert!(commit.start > 0, "测试必须覆盖非零窗口起点");
        let expected = commit.offsets[0].clone();
        for action in [
            WindowCardAction::Select,
            WindowCardAction::Copy,
            WindowCardAction::Pin { is_pinned: true },
            WindowCardAction::Delete,
        ] {
            let event = resolve_geometry_card_event(&state, 0, action)
                .expect("当前 bounded local index 必须解析");
            let UiEvent::HistoryWindowCardRequested {
                absolute_index,
                id,
                content_hash,
                action: actual_action,
                ..
            } = event
            else {
                panic!("显式窗口必须生成带身份事件");
            };
            assert_eq!(absolute_index, expected.absolute_index);
            assert_eq!(id, expected.id);
            assert_eq!(content_hash, expected.content_hash);
            assert_eq!(actual_action, action);
        }
    }

    /// 用固定内容与可见高度构造指定底部距离的真实几何。
    fn viewport_for_distance(distance: i32) -> UiEvent {
        UiEvent::HistoryViewportChanged {
            viewport_y: -(900 - distance),
            visible_height: 100,
            content_height: 1_000,
        }
    }

    /// 在已打开面板中完成“打开确认→确认”，返回 reducer 生成的后台请求。
    fn begin_clear(state: &mut UiState) -> ClearHistoryMutationRequest {
        assert_eq!(state.apply(UiEvent::ClearUnpinnedRequested), UiAction::None);
        assert!(state.clear_unpinned_confirmation_visible);
        let action = state.apply(UiEvent::ClearUnpinnedConfirmed {
            panel_generation: state.panel_generation,
        });
        let UiAction::QueueClearHistory(request) = action else {
            panic!("二次确认必须生成清空请求");
        };
        assert_eq!(request.scope, ClearHistoryScope::UnpinnedText);
        request
    }

    /// 在已打开面板中完成文字强确认，并返回显式 All 范围请求。
    fn begin_clear_all(state: &mut UiState) -> ClearHistoryMutationRequest {
        assert_eq!(state.apply(UiEvent::ClearAllRequested), UiAction::None);
        assert!(state.clear_all_confirmation_visible);
        state.apply(UiEvent::ClearAllConfirmationTextChanged(
            CLEAR_ALL_CONFIRMATION_PHRASE.to_owned(),
        ));
        let action = state.apply(UiEvent::ClearAllConfirmed {
            panel_generation: state.panel_generation,
            confirmation_text: CLEAR_ALL_CONFIRMATION_PHRASE.to_owned(),
        });
        let UiAction::QueueClearHistory(request) = action else {
            panic!("精确文字强确认必须生成清空全部请求");
        };
        assert_eq!(request.scope, ClearHistoryScope::All);
        request
    }

    /// 生成与指定请求严格匹配的清空成功事件。
    fn clear_succeeded(request: ClearHistoryMutationRequest, clear_revision: u64) -> UiEvent {
        UiEvent::ClearHistoryMutationCompleted(ClearHistoryMutationResult {
            mutation_token: request.mutation_token,
            panel_generation: request.panel_generation,
            scope: request.scope,
            outcome: Ok(ClearHistoryMutationSuccess {
                deleted_count: 1,
                clear_revision,
            }),
        })
    }

    /// 打开确认后取消不得创建请求或改变卡片。
    #[test]
    fn 取消清空确认不改变历史() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0), test_item(1)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let before = state.snapshot.clone();
        state.apply(UiEvent::ClearUnpinnedRequested);
        assert!(state.clear_unpinned_confirmation_visible);
        assert_eq!(state.apply(UiEvent::ClearUnpinnedCancelled), UiAction::None);
        assert!(!state.clear_unpinned_confirmation_visible);
        assert!(state.pending_clear_history_mutation.is_none());
        assert_eq!(state.snapshot, before);
    }

    /// 清空全部必须同时校验 reducer 状态和点击事件文字，错误或带空格输入均不可提交。
    #[test]
    fn 清空全部只接受精确文字强确认() {
        let mut state = UiState::default();
        state.apply(UiEvent::OpenPanel);
        state.apply(UiEvent::ClearAllRequested);
        state.apply(UiEvent::ClearAllConfirmationTextChanged(
            "清空全部 ".to_owned(),
        ));
        assert_eq!(
            state.apply(UiEvent::ClearAllConfirmed {
                panel_generation: state.panel_generation,
                confirmation_text: "清空全部 ".to_owned(),
            }),
            UiAction::None
        );
        assert!(state.pending_clear_history_mutation.is_none());

        state.apply(UiEvent::ClearAllConfirmationTextChanged(
            CLEAR_ALL_CONFIRMATION_PHRASE.to_owned(),
        ));
        assert_eq!(
            state.apply(UiEvent::ClearAllConfirmed {
                panel_generation: state.panel_generation,
                confirmation_text: "错误文字".to_owned(),
            }),
            UiAction::None
        );
        assert!(state.pending_clear_history_mutation.is_none());

        let UiAction::QueueClearHistory(request) = state.apply(UiEvent::ClearAllConfirmed {
            panel_generation: state.panel_generation,
            confirmation_text: CLEAR_ALL_CONFIRMATION_PHRASE.to_owned(),
        }) else {
            panic!("精确文字必须生成后台请求");
        };
        assert_eq!(request.scope, ClearHistoryScope::All);
        assert!(state.clear_all_confirmation_text.is_empty());
    }

    /// 取消清空全部只关闭确认和输入，不修改历史或建立请求。
    #[test]
    fn 取消清空全部不改变历史() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0), test_item(1)],
            selected_index: Some(1),
        }));
        state.apply(UiEvent::OpenPanel);
        let before = state.snapshot.clone();
        state.apply(UiEvent::ClearAllRequested);
        state.apply(UiEvent::ClearAllConfirmationTextChanged(
            CLEAR_ALL_CONFIRMATION_PHRASE.to_owned(),
        ));
        state.apply(UiEvent::ClearAllCancelled);

        assert!(!state.clear_all_confirmation_visible);
        assert!(state.clear_all_confirmation_text.is_empty());
        assert!(state.pending_clear_history_mutation.is_none());
        assert_eq!(state.snapshot, before);
    }

    /// 全量清空成功移除收藏和普通旧项，保留事务后捕获并清理已删除选择与旧页。
    #[test]
    fn 清空全部成功收口派生状态并保留事务后捕获() {
        let mut pinned = test_item(1);
        pinned.is_pinned = true;
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0), pinned],
            selected_index: Some(1),
        }));
        state.apply(UiEvent::OpenPanel);
        let stale_page = state
            .take_pending_history_request()
            .expect("打开后缺少旧首页请求");
        let request = begin_clear_all(&mut state);
        let post_clear = test_item(2);
        state.apply(captured(post_clear.clone(), 4));
        let capture_refresh = state
            .take_pending_history_request()
            .expect("清空后捕获缺少刷新请求");
        state.apply_history_page_result(page_result(
            &capture_refresh,
            Ok(UiHistoryPage {
                items: vec![post_clear.clone()],
                next_cursor: None,
            }),
        ));

        state.apply(clear_succeeded(request, 3));

        assert_eq!(state.snapshot.items, vec![post_clear.clone()]);
        assert_eq!(state.history.items(), &[post_clear]);
        assert_eq!(state.snapshot.selected_index, Some(0));
        assert_eq!(state.active_clear_revision, 3);
        assert!(!state
            .capture_revisions
            .values()
            .any(|revision| *revision < 3));
        state.apply_history_page_result(page_result(
            &stale_page,
            Ok(UiHistoryPage {
                items: vec![test_item(0)],
                next_cursor: None,
            }),
        ));
        assert_eq!(state.snapshot.items.len(), 1);
    }

    /// 清空后捕获只进入内存且刷新被作废时，提交失败也不能把新记录藏到下次重开。
    #[test]
    fn 清空全部在刷新失败时仍显示事务后捕获() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let _old_page = state
            .take_pending_history_request()
            .expect("打开后缺少旧首页请求");
        let request = begin_clear_all(&mut state);
        let post_clear = test_item(2);
        state.apply(captured(post_clear.clone(), 4));
        let _capture_page = state
            .take_pending_history_request()
            .expect("捕获后缺少即将作废的首页请求");

        state.apply(clear_succeeded(request, 3));
        assert_eq!(state.snapshot.items, vec![post_clear.clone()]);
        let clear_refresh = state
            .take_pending_history_request()
            .expect("清空成功后缺少新首页请求");
        state.mark_history_submission_failed(&clear_refresh);
        assert_eq!(state.snapshot.items, vec![post_clear]);
    }

    /// 当前筛选首页不含事务后捕获时，独立账本必须跨查询替换保留修订号和内存记录。
    #[test]
    fn 清空全部账本抵抗筛选首页抹除事务后捕获() {
        let mut state = UiState::default();
        state.search_filter = SearchFilter::Pinned;
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        // OpenPanel 会重置筛选；测试在请求生成前恢复收藏筛选，模拟用户当前数据集。
        state.search_filter = SearchFilter::Pinned;
        let _old_page = state
            .take_pending_history_request()
            .expect("打开后缺少旧首页请求");
        let request = begin_clear_all(&mut state);
        let post_clear = test_item(2);
        state.apply(captured(post_clear.clone(), 4));
        let capture_page = state
            .take_pending_history_request()
            .expect("捕获后缺少当前筛选首页请求");
        state.apply_history_page_result(page_result(
            &capture_page,
            Ok(UiHistoryPage {
                // 未收藏的新捕获不属于收藏筛选，查询页会合法地不返回它。
                items: Vec::new(),
                next_cursor: None,
            }),
        ));
        assert!(state.history.items().is_empty());
        assert!(state.capture_revisions.is_empty());
        assert_eq!(state.pending_clear_captures.len(), 1);

        state.apply(clear_succeeded(request, 3));

        assert_eq!(state.history.items(), &[post_clear.clone()]);
        assert_eq!(
            state
                .capture_revisions
                .get(&(post_clear.id, post_clear.content_hash)),
            Some(&4)
        );
        assert!(state.snapshot.items.is_empty());
    }

    /// 全量清空存储或提交失败不得乐观删除，并只显示全量路径固定错误。
    #[test]
    fn 清空全部失败保留历史并隔离错误状态() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let request = begin_clear_all(&mut state);
        let before = state.snapshot.clone();
        state.apply(UiEvent::ClearHistoryMutationCompleted(
            ClearHistoryMutationResult {
                mutation_token: request.mutation_token,
                panel_generation: request.panel_generation,
                scope: request.scope,
                outcome: Err(ClearHistoryMutationFailure::StorageUnavailable),
            },
        ));
        assert_eq!(state.snapshot, before);
        assert!(state.clear_all_error_visible);
        assert!(!state.clear_unpinned_error_visible);

        let second = begin_clear_all(&mut state);
        state.mark_clear_history_submission_failed(&second);
        assert_eq!(state.snapshot, before);
        assert!(state.clear_all_error_visible);
    }

    /// 全量清空在隐藏期间完成仍须删除收藏项，重开后旧历史不能恢复。
    #[test]
    fn 清空全部在隐藏期间完成并保持结果() {
        let mut pinned = test_item(0);
        pinned.is_pinned = true;
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![pinned],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let request = begin_clear_all(&mut state);
        let generation = state.panel_generation;
        state.apply(UiEvent::HidePanel { generation });
        assert!(!state.clear_all_confirmation_visible);
        assert!(state.clear_all_confirmation_text.is_empty());

        state.apply(clear_succeeded(request, 2));
        assert!(state.snapshot.items.is_empty());
        assert!(state.history.items().is_empty());
        assert!(state.pending_clear_history_mutation.is_none());
        state.apply(UiEvent::OpenPanel);
        assert!(state.snapshot.items.is_empty());
    }

    /// 全量确认和普通确认必须互斥，且清空在途时收藏与删除都不能建立请求。
    #[test]
    fn 清空全部与其他历史变更双向互斥() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        state.apply(UiEvent::ClearUnpinnedRequested);
        assert!(state.clear_unpinned_confirmation_visible);
        state.apply(UiEvent::ClearAllRequested);
        assert!(state.clear_all_confirmation_visible);
        assert!(!state.clear_unpinned_confirmation_visible);
        let request = begin_clear_all(&mut state);
        let item = state.snapshot.items[0].clone();
        assert_eq!(
            state.begin_pin_mutation(state.panel_generation, item.id, item.content_hash, true),
            UiAction::None
        );
        assert_eq!(
            state.begin_delete_mutation(state.panel_generation, item.id, item.content_hash),
            UiAction::None
        );
        assert_eq!(state.apply(UiEvent::ClearUnpinnedRequested), UiAction::None);
        assert!(!state.clear_unpinned_confirmation_visible);
        state.mark_clear_history_submission_failed(&request);
    }

    /// 成功前不乐观移除；成功后只移除清空前未收藏项并保留收藏与清空后捕获。
    #[test]
    fn 清空成功按存储修订号保留收藏和新捕获() {
        let mut state = UiState::default();
        let mut pinned = test_item(1);
        pinned.is_pinned = true;
        let old = test_item(0);
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![old.clone(), pinned.clone()],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let first = state
            .take_pending_history_request()
            .expect("打开后缺少首页请求");
        state.apply_history_page_result(page_result(
            &first,
            Ok(UiHistoryPage {
                items: vec![old.clone(), pinned.clone()],
                next_cursor: None,
            }),
        ));
        let request = begin_clear(&mut state);
        assert_eq!(state.snapshot.items, vec![old, pinned.clone()]);

        let post_clear = test_item(2);
        state.apply(captured(post_clear.clone(), 3));
        let capture_page = state
            .take_pending_history_request()
            .expect("捕获后缺少首页请求");
        state.apply_history_page_result(page_result(
            &capture_page,
            Ok(UiHistoryPage {
                items: vec![post_clear.clone(), pinned.clone()],
                next_cursor: None,
            }),
        ));
        state.apply(clear_succeeded(request, 2));

        assert_eq!(
            state
                .snapshot
                .items
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![post_clear.id, pinned.id]
        );
        assert_eq!(state.active_clear_revision, 2);
        assert!(event_may_refresh_model(&clear_succeeded(request, 2)));
        let refresh = state
            .take_pending_history_request()
            .expect("清空成功后缺少刷新请求");
        state.mark_history_submission_failed(&refresh);
        assert_eq!(state.snapshot.items.len(), 2);
    }

    /// 失败结果和提交失败都必须保留卡片，并显示同一固定失败状态。
    #[test]
    fn 清空失败保留历史并显示固定错误() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let request = begin_clear(&mut state);
        let before = state.snapshot.clone();
        state.apply(UiEvent::ClearHistoryMutationCompleted(
            ClearHistoryMutationResult {
                mutation_token: request.mutation_token,
                panel_generation: request.panel_generation,
                scope: request.scope,
                outcome: Err(ClearHistoryMutationFailure::StorageUnavailable),
            },
        ));
        assert_eq!(state.snapshot, before);
        assert!(state.clear_unpinned_error_visible);

        state.apply(UiEvent::ClearUnpinnedRequested);
        let second = begin_clear(&mut state);
        state.mark_clear_history_submission_failed(&second);
        assert_eq!(state.snapshot, before);
        assert!(state.clear_unpinned_error_visible);
    }

    /// 成功水位只增不减，幂等第二次清空后第一次清空前的迟到捕获仍被拒绝。
    #[test]
    fn 清空水位单调并拒绝迟到捕获() {
        let mut state = UiState::default();
        state.apply(UiEvent::OpenPanel);
        let first = begin_clear(&mut state);
        state.apply(clear_succeeded(first, 5));
        let second = begin_clear(&mut state);
        state.apply(clear_succeeded(second, 3));
        assert_eq!(state.active_clear_revision, 5);

        state.apply(captured(test_item(8), 4));
        assert!(state
            .history
            .items()
            .iter()
            .all(|item| item.id != test_item(8).id));
        let reused_old_id = test_item(0);
        state.apply(captured(reused_old_id.clone(), 6));
        assert!(state
            .history
            .items()
            .iter()
            .any(|item| item.id == reused_old_id.id));
    }

    /// 清空前返回的旧收藏快照不能在随后取消收藏并清空后迟到复活。
    #[test]
    fn 清空拒绝收藏状态已经过期的迟到捕获() {
        let mut state = UiState::default();
        state.apply(UiEvent::OpenPanel);
        let request = begin_clear(&mut state);
        state.apply(clear_succeeded(request, 2));

        let mut stale_pinned_capture = test_item(4);
        stale_pinned_capture.is_pinned = true;
        state.apply(captured(stale_pinned_capture.clone(), 1));

        assert!(state
            .history
            .items()
            .iter()
            .all(|item| item.id != stale_pinned_capture.id));
        assert!(state
            .snapshot
            .items
            .iter()
            .all(|item| item.id != stale_pinned_capture.id));
    }

    /// 清空与收藏、单条删除必须双向共享一个 UI mutation 互斥边界。
    #[test]
    fn 清空与收藏删除双向互斥() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let clear_request = begin_clear(&mut state);
        let item = state.snapshot.items[0].clone();
        assert_eq!(
            state.begin_pin_mutation(state.panel_generation, item.id, item.content_hash, true),
            UiAction::None
        );
        assert_eq!(
            state.begin_delete_mutation(state.panel_generation, item.id, item.content_hash),
            UiAction::None
        );
        state.mark_clear_history_submission_failed(&clear_request);

        let UiAction::QueuePin(_) =
            state.begin_pin_mutation(state.panel_generation, item.id, item.content_hash, true)
        else {
            panic!("清空结束后收藏请求应可建立");
        };
        assert_eq!(state.apply(UiEvent::ClearUnpinnedRequested), UiAction::None);
        assert!(!state.clear_unpinned_confirmation_visible);
    }

    /// 深分页快照成功清空后只保留收藏，清空前旧首页不得把已删记录带回。
    #[test]
    fn 清空深分页快照并拒绝旧首页复活() {
        let mut items = (0..120).map(test_item).collect::<Vec<_>>();
        items[110].is_pinned = true;
        let pinned_identity = (items[110].id, items[110].content_hash);
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: items.clone(),
            selected_index: Some(110),
        }));
        state.apply(UiEvent::OpenPanel);
        let stale_page = state
            .take_pending_history_request()
            .expect("打开后缺少旧首页请求");
        let request = begin_clear(&mut state);
        state.apply(clear_succeeded(request, 10));

        assert_eq!(state.snapshot.items.len(), 1);
        assert_eq!(
            (
                state.snapshot.items[0].id,
                state.snapshot.items[0].content_hash
            ),
            pinned_identity
        );
        assert_eq!(state.snapshot.selected_index, Some(0));
        state.apply_history_page_result(page_result(
            &stale_page,
            Ok(UiHistoryPage {
                items,
                next_cursor: None,
            }),
        ));
        assert_eq!(state.snapshot.items.len(), 1);
        assert_eq!(state.snapshot.items[0].id, pinned_identity.0);
    }

    /// 已接受清空在面板隐藏后仍须应用，重新打开不能吞掉成功结果或降低水位。
    #[test]
    fn 清空在隐藏期间完成并在重开后保持() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let generation = state.panel_generation;
        let request = begin_clear(&mut state);
        state.apply(UiEvent::HidePanel { generation });
        state.apply(clear_succeeded(request, 2));

        assert!(state.snapshot.items.is_empty());
        assert_eq!(state.active_clear_revision, 2);
        assert!(state.pending_clear_history_mutation.is_none());
        state.apply(UiEvent::ShowPanel);
        assert!(state.snapshot.items.is_empty());
        assert_eq!(state.active_clear_revision, 2);
    }

    /// Quit 使用的关闭辅助函数必须关闭已绑定 clear sender，并立即拒绝后续请求。
    #[test]
    fn 退出关闭已绑定的清空请求入口() {
        let (sender, _receiver) = clear_history_mutation_channel();
        bind_clear_history_mutation_sender(sender.clone());
        close_clear_history_mutation_bridge();

        assert_eq!(
            sender.try_submit(ClearHistoryMutationRequest {
                mutation_token: 1,
                panel_generation: 1,
                scope: ClearHistoryScope::UnpinnedText,
            }),
            Err(ClearHistoryMutationSubmitError::Closed)
        );
    }

    /// 首次打开立即请求 30 条 SQLite 首页，托盘重复显示不得重查。
    #[test]
    fn 首次打开立即查询首页且托盘重复显示不重查() {
        let mut state = UiState::default();
        assert_eq!(state.apply(UiEvent::OpenPanel), UiAction::Show);
        let request = state
            .take_pending_history_request()
            .expect("首次打开必须生成首页请求");
        assert_eq!(request.query.limit, UI_FIRST_BATCH_SIZE as u32);
        assert!(request.query.cursor.is_none());

        assert_eq!(state.apply(UiEvent::ShowPanel), UiAction::Reassert);
        assert!(state.take_pending_history_request().is_none());
    }

    /// 混合列表进入与离开阈值必须精确覆盖 183/184/185/275/276/277 六个边界。
    #[test]
    fn 混合卡片底部阈值提前加载() {
        assert_eq!(HISTORY_BOTTOM_ENTER_THRESHOLD, 184);
        assert_eq!(HISTORY_BOTTOM_EXIT_THRESHOLD, 276);
        assert!(UiState::near_bottom_after_distance(false, 183));
        assert!(UiState::near_bottom_after_distance(false, 184));
        assert!(!UiState::near_bottom_after_distance(false, 185));
        assert!(UiState::near_bottom_after_distance(true, 185));
        assert!(UiState::near_bottom_after_distance(true, 276));
        assert!(!UiState::near_bottom_after_distance(true, 277));
    }

    /// 三个布局属性乱序通知和滞回区抖动只能生成一个活动续页请求。
    #[test]
    fn 几何抖动只签发一次续页() {
        let mut state = UiState::default();
        state.apply(UiEvent::OpenPanel);
        let first = state.take_pending_history_request().unwrap();
        state.apply_history_page_result(page_result(
            &first,
            Ok(UiHistoryPage {
                items: (0..30).map(test_item).collect(),
                next_cursor: Some(crate::storage::HistoryCursor {
                    copied_at: 70,
                    id: 30,
                }),
            }),
        ));

        state.apply(viewport_for_distance(277));
        state.apply(viewport_for_distance(184));
        let request = state.take_pending_history_request().unwrap();
        for distance in [183, 185, 275, 184, 277, 276, 185] {
            state.apply(viewport_for_distance(distance));
            assert!(state.take_pending_history_request().is_none());
        }
        assert!(state.history_pages.has_active_request());
        assert_eq!(request.query.limit, 50);
    }

    /// Append 等待期间旧普通回调无效，匹配探针只消费一次并最多继续一页。
    #[test]
    fn 追加后绑定探针按新几何继续补页() {
        let (mut state, revision) = state_with_pending_append_probe();
        let selected_identity = state
            .snapshot
            .selected_index
            .and_then(|index| state.snapshot.items.get(index))
            .map(|item| (item.id, item.content_hash));
        assert_eq!(selected_identity, Some((1, [0; 32])));
        assert_eq!(
            state.append_binding_gate,
            AppendBindingGate::ProbePending(revision)
        );
        state.apply(viewport_for_distance(277));
        state.apply(viewport_for_distance(183));
        assert!(state.take_pending_history_request().is_none());

        let probe = UiEvent::HistoryPostAppendProbe {
            append_revision: revision,
            viewport_y: -(900 - 184),
            visible_height: 100,
            content_height: 1_000,
        };
        state.apply(probe.clone());
        assert_eq!(state.append_binding_gate, AppendBindingGate::Idle);
        let request = state.take_pending_history_request().unwrap();
        state.apply(probe);
        assert!(state.take_pending_history_request().is_none());
        assert_eq!(request.query.limit, 50);
        assert_eq!(
            state.apply(UiEvent::CopyItem {
                panel_generation: state.panel_generation,
                id: 1,
                content_hash: [0; 32],
            }),
            UiAction::QueueCopy {
                id: 1,
                content_hash: [0; 32],
            }
        );
    }

    /// pending 期间冻结的普通回调即使晚于 outside 探针送达也不能重新进入底部。
    #[test]
    fn 旧普通回调和重复探针不得误触发() {
        let (mut state, revision) = state_with_pending_append_probe();
        let stale = UiEvent::HistoryViewportChangedDuringAppend {
            append_revision: Some(revision),
            viewport_y: -(900 - 184),
            visible_height: 100,
            content_height: 1_000,
        };
        state.apply(UiEvent::HistoryPostAppendProbe {
            append_revision: revision,
            viewport_y: -(900 - 277),
            visible_height: 100,
            content_height: 1_000,
        });
        assert!(!state.history_was_near_bottom);
        assert!(state.take_pending_history_request().is_none());

        state.apply(stale);
        state.apply(UiEvent::HistoryPostAppendProbe {
            append_revision: revision,
            viewport_y: -(900 - 184),
            visible_height: 100,
            content_height: 1_000,
        });
        assert!(!state.history_was_near_bottom);
        assert!(state.take_pending_history_request().is_none());
    }

    /// 搜索等数据集失效路径必须拒绝旧追加探针并恢复 outside 初态。
    #[test]
    fn 数据集失效清除旧追加探针() {
        let invalidators: Vec<Box<dyn Fn(&mut UiState)>> = vec![
            Box::new(|state| {
                state.apply_at(
                    UiEvent::SearchTextChanged("新查询".to_owned()),
                    Instant::now(),
                );
            }),
            Box::new(|state| {
                state.apply_at(
                    UiEvent::SearchFilterChanged(SearchFilter::Image),
                    Instant::now(),
                );
            }),
            Box::new(|state| {
                state.apply(captured(test_item(99), 1));
            }),
            Box::new(|state| {
                let generation = state.panel_generation;
                state.apply(UiEvent::HidePanel { generation });
                state.apply(UiEvent::ShowPanel);
            }),
        ];

        for invalidate in invalidators {
            let (mut state, revision) = state_with_pending_append_probe();
            invalidate(&mut state);
            assert_eq!(state.append_binding_gate, AppendBindingGate::Idle);
            assert!(!state.history_was_near_bottom);
            state.pending_history_request = None;
            state.apply(UiEvent::HistoryPostAppendProbe {
                append_revision: revision,
                viewport_y: -(900 - 184),
                visible_height: 100,
                content_height: 1_000,
            });
            assert!(state.take_pending_history_request().is_none());
        }

        let mut state = UiState::default();
        state.apply(UiEvent::OpenPanel);
        let first = state.take_pending_history_request().unwrap();
        let old_revision = 77;
        state.append_binding_gate = AppendBindingGate::ProbePending(old_revision);
        state.history_was_near_bottom = true;
        state.apply_history_page_result(page_result(
            &first,
            Ok(UiHistoryPage {
                items: vec![test_item(100)],
                next_cursor: None,
            }),
        ));
        state.apply(UiEvent::HistoryPostAppendProbe {
            append_revision: old_revision,
            viewport_y: 0,
            visible_height: 100,
            content_height: 100,
        });
        assert!(state.take_pending_history_request().is_none());
        assert!(!state.history_was_near_bottom);
    }

    /// 调度失败只取消匹配 pending，随后真实 outside→inside 仍可恢复普通续页。
    #[test]
    fn 探针投递失败解除门禁并可恢复() {
        let (mut state, revision) = state_with_pending_append_probe();
        let result: Result<(), &'static str> = settle_post_append_probe_dispatch(
            revision,
            Err("注入调度失败"),
            |failed_revision| state.cancel_post_append_probe(failed_revision),
        );
        assert!(result.is_err());
        assert_eq!(state.append_binding_gate, AppendBindingGate::Idle);
        assert_eq!(state.snapshot.items.len(), 31);

        state.apply(viewport_for_distance(277));
        state.apply(viewport_for_distance(184));
        assert!(state.take_pending_history_request().is_some());
    }

    /// 窗口弱引用缺失与调度失败共用精确取消语义，不能永久封锁普通边沿。
    #[test]
    fn 窗口缺失取消探针后可恢复() {
        let (mut state, revision) = state_with_pending_append_probe();
        // 生产窗口 weak upgrade 失败时调用同一取消接缝；这里不创建或显示真实窗口。
        state.cancel_post_append_probe(revision);
        assert_eq!(state.append_binding_gate, AppendBindingGate::Idle);
        assert_eq!(state.snapshot.items.len(), 31);
        state.apply(viewport_for_distance(277));
        state.apply(viewport_for_distance(184));
        assert!(state.take_pending_history_request().is_some());
    }

    /// 修订耗尽的成功 Append 仍冻结绑定回调，完成后由真实 outside→inside 恢复。
    #[test]
    fn 追加修订耗尽不回绕() {
        let mut state = UiState::default();
        state.apply(UiEvent::OpenPanel);
        let first = state.take_pending_history_request().unwrap();
        state.apply_history_page_result(page_result(
            &first,
            Ok(UiHistoryPage {
                items: (0..30).map(test_item).collect(),
                next_cursor: Some(crate::storage::HistoryCursor {
                    copied_at: 70,
                    id: 30,
                }),
            }),
        ));
        state.next_append_revision = u64::MAX;
        state.apply(viewport_for_distance(184));
        let next = state.take_pending_history_request().unwrap();
        let refresh = state.apply_history_page_result(page_result(
            &next,
            Ok(UiHistoryPage {
                items: vec![test_item(30)],
                next_cursor: Some(crate::storage::HistoryCursor {
                    copied_at: 60,
                    id: 31,
                }),
            }),
        ));
        assert_eq!(
            refresh,
            HistoryModelRefresh::AppendPreservingViewport {
                append_revision: None
            }
        );
        assert_eq!(state.next_append_revision, u64::MAX);
        assert_eq!(
            state.append_binding_gate,
            AppendBindingGate::RevisionExhausted
        );
        assert_eq!(state.snapshot.items.len(), 31);

        // 模拟模型与视口 setter 同步及延迟产生的乱序回调；下一 UI 闭包前门禁仍有效。
        for distance in [277, 183, 275, 184] {
            state.apply(UiEvent::HistoryViewportChangedDuringAppend {
                append_revision: None,
                viewport_y: -(900 - distance),
                visible_height: 100,
                content_height: 1_000,
            });
            assert!(state.take_pending_history_request().is_none());
        }
        // 生产路径由下一 UI 闭包执行同一收口方法，且不读取几何、不发送 probe。
        state.finish_exhausted_append_binding();
        assert_eq!(state.append_binding_gate, AppendBindingGate::Idle);

        // 绑定前冻结的迟到事件仍无效；同区 inside 也不会自动重武装本次 Append。
        state.apply(UiEvent::HistoryViewportChangedDuringAppend {
            append_revision: None,
            viewport_y: -(900 - 184),
            visible_height: 100,
            content_height: 1_000,
        });
        state.apply(viewport_for_distance(184));
        assert!(state.take_pending_history_request().is_none());

        state.apply(viewport_for_distance(277));
        state.apply(viewport_for_distance(184));
        assert!(state.take_pending_history_request().is_some());
    }

    /// 续页加载态只覆盖续页在途时间，成功、失败和数据集切换都会收口。
    #[test]
    fn 续页加载态在全部收口路径关闭() {
        let (mut state, revision) = state_with_pending_append_probe();
        assert!(!state.history_next_page_loading);
        state.apply(UiEvent::HistoryPostAppendProbe {
            append_revision: revision,
            viewport_y: -(900 - 184),
            visible_height: 100,
            content_height: 1_000,
        });
        assert!(state.history_next_page_loading);
        let request = state.take_pending_history_request().unwrap();
        state.apply_history_page_result(page_result(
            &request,
            Err(HistoryQueryFailure::StorageUnavailable),
        ));
        assert!(!state.history_next_page_loading);

        state.begin_history_dataset(true);
        assert!(!state.history_next_page_loading);
        state.hide_current_panel();
        assert!(!state.history_next_page_loading);
    }

    /// 85 条数据必须严格按 30、50、5 三批追加，保持数据库顺序且不重复。
    #[test]
    fn 滚动续页按三十加五十加五加载八十五条() {
        let mut state = UiState::default();
        state.apply(UiEvent::OpenPanel);
        let first = state.take_pending_history_request().unwrap();
        state.apply_history_page_result(page_result(
            &first,
            Ok(UiHistoryPage {
                items: (0..30).map(test_item).collect(),
                next_cursor: Some(crate::storage::HistoryCursor {
                    copied_at: 70,
                    id: 30,
                }),
            }),
        ));

        state.apply(UiEvent::HistoryViewportChanged {
            viewport_y: 0,
            visible_height: 212,
            content_height: 3_180,
        });
        state.apply(UiEvent::HistoryViewportChanged {
            viewport_y: -2_800,
            visible_height: 212,
            content_height: 3_180,
        });
        let second = state.take_pending_history_request().unwrap();
        assert_eq!(second.query.limit, 50);
        let refresh = state.apply_history_page_result(page_result(
            &second,
            Ok(UiHistoryPage {
                items: (30..80).map(test_item).collect(),
                next_cursor: Some(crate::storage::HistoryCursor {
                    copied_at: 20,
                    id: 80,
                }),
            }),
        ));
        let HistoryModelRefresh::AppendPreservingViewport {
            append_revision: Some(revision),
        } = refresh
        else {
            panic!("第二页成功后必须登记绑定探针");
        };
        state.apply(UiEvent::HistoryPostAppendProbe {
            append_revision: revision,
            viewport_y: -8_200,
            visible_height: 212,
            content_height: 8_480,
        });
        let third = state.take_pending_history_request().unwrap();
        assert_eq!(third.query.limit, 50);
        state.apply_history_page_result(page_result(
            &third,
            Ok(UiHistoryPage {
                items: (80..85).map(test_item).collect(),
                next_cursor: None,
            }),
        ));

        assert_eq!(state.snapshot.items.len(), 85);
        assert_eq!(
            state
                .snapshot
                .items
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            (1..=85).collect::<Vec<_>>()
        );
        assert!(state.history_pages.next_cursor().is_none());
    }

    /// 同一底部区域重复通知不得重发；失败后只有点击或离开再进入可生成新 token。
    #[test]
    fn 续页失败保持游标并要求明确重试() {
        let mut state = UiState::default();
        state.apply(UiEvent::OpenPanel);
        let first = state.take_pending_history_request().unwrap();
        let cursor = crate::storage::HistoryCursor {
            copied_at: 70,
            id: 30,
        };
        state.apply_history_page_result(page_result(
            &first,
            Ok(UiHistoryPage {
                items: (0..30).map(test_item).collect(),
                next_cursor: Some(cursor),
            }),
        ));
        let near_bottom = UiEvent::HistoryViewportChanged {
            viewport_y: -2_800,
            visible_height: 212,
            content_height: 3_180,
        };
        state.apply(near_bottom.clone());
        let failed = state.take_pending_history_request().unwrap();
        state.apply_history_page_result(page_result(
            &failed,
            Err(HistoryQueryFailure::StorageUnavailable),
        ));
        assert!(state.history_pages.retry_required());
        assert_eq!(state.history_pages.next_cursor(), Some(cursor));
        assert_eq!(state.snapshot.items.len(), 30);

        state.apply(near_bottom);
        assert!(state.take_pending_history_request().is_none());
        state.apply(UiEvent::RetryHistoryPage);
        let retry = state.take_pending_history_request().unwrap();
        assert_ne!(retry.token, failed.token);
        assert_eq!(retry.query.cursor, failed.query.cursor);
        state.apply_history_page_result(page_result(
            &retry,
            Err(HistoryQueryFailure::StorageUnavailable),
        ));
        state.apply(UiEvent::HistoryViewportChanged {
            viewport_y: 0,
            visible_height: 212,
            content_height: 3_180,
        });
        state.apply(UiEvent::HistoryViewportChanged {
            viewport_y: -2_800,
            visible_height: 212,
            content_height: 3_180,
        });
        let edge_retry = state.take_pending_history_request().unwrap();
        assert_ne!(edge_retry.token, retry.token);
        assert_eq!(edge_retry.query.cursor, retry.query.cursor);
    }

    /// 续页按 ID 保留首见顺序去重，但后续游标必须采用数据库成功页返回值。
    #[test]
    fn 续页去重不从_ui_末项重算游标() {
        let mut state = UiState::default();
        state.apply(UiEvent::OpenPanel);
        let first = state.take_pending_history_request().unwrap();
        state.apply_history_page_result(page_result(
            &first,
            Ok(UiHistoryPage {
                items: (0..30).map(test_item).collect(),
                next_cursor: Some(crate::storage::HistoryCursor {
                    copied_at: 70,
                    id: 30,
                }),
            }),
        ));
        state.apply(UiEvent::HistoryViewportChanged {
            viewport_y: -2_800,
            visible_height: 212,
            content_height: 3_180,
        });
        let request = state.take_pending_history_request().unwrap();
        let database_cursor = crate::storage::HistoryCursor {
            copied_at: 60,
            id: 40,
        };
        state.apply_history_page_result(page_result(
            &request,
            Ok(UiHistoryPage {
                items: vec![test_item(29), test_item(30), test_item(30)],
                next_cursor: Some(database_cursor),
            }),
        ));

        assert_eq!(state.snapshot.items.len(), 31);
        assert_eq!(state.snapshot.items[30].id, 31);
        assert_eq!(state.history_pages.next_cursor(), Some(database_cursor));
    }

    /// 真实 reducer 接受混合页后，公开状态快照只能读取纯数值性能观测。
    #[test]
    fn 混合页性能快照通过公开状态读取() {
        let mut state = UiState::default();
        state.apply(UiEvent::OpenPanel);
        let request = state
            .take_pending_history_request()
            .expect("打开面板后应签发首页请求");
        let text = test_item(0);
        let mut image = test_item(1);
        image.kind = UiClipboardItemKind::Image(UiImageSummary {
            thumbnail_path: std::path::PathBuf::from("thumbnail.webp"),
            width: 320,
            height: 200,
        });
        state.apply_history_page_result(page_result(
            &request,
            Ok(UiHistoryPage {
                items: vec![text, image.clone(), image],
                next_cursor: None,
            }),
        ));

        let public_snapshot = state.snapshot();
        assert_eq!(public_snapshot.history_performance.accepted_pages, 1);
        assert_eq!(public_snapshot.history_performance.loaded_items, 2);
        assert_eq!(public_snapshot.history_performance.text_items, 1);
        assert_eq!(public_snapshot.history_performance.image_items, 1);
        assert_eq!(public_snapshot.history_performance.duplicate_items, 1);
        assert_eq!(
            public_snapshot.history_performance.text_items
                + public_snapshot.history_performance.image_items,
            public_snapshot.history_performance.loaded_items
        );
    }

    /// 第 31 条以后的卡片仍可由稳定身份直接选择并生成复制动作。
    #[test]
    fn 跨页卡片保持完整选择和复制能力() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: (0..85).map(test_item).collect(),
            selected_index: Some(84),
        }));
        state.apply(UiEvent::OpenPanel);
        let generation = state.panel_generation();
        assert_eq!(state.snapshot.selected_index, Some(84));
        assert_eq!(
            state.apply(UiEvent::CopyItem {
                panel_generation: generation,
                id: 85,
                content_hash: [84; 32],
            }),
            UiAction::QueueCopy {
                id: 85,
                content_hash: [84; 32],
            }
        );
    }

    /// 查询失败只显示固定错误，不能清空仍可交互的旧卡片。
    #[test]
    fn 首页查询失败保留旧卡片() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let request = state.take_pending_history_request().unwrap();
        state.apply_history_page_result(page_result(
            &request,
            Err(HistoryQueryFailure::StorageUnavailable),
        ));
        assert_eq!(state.search_status, SearchStatus::Error);
        assert_eq!(state.snapshot.items, vec![test_item(0)]);
    }

    /// 捕获提交会先推进数据集，使已经生成但尚未应用的旧首页失效。
    #[test]
    fn 捕获刷新拒绝旧首页结果() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);
        let old_request = state.take_pending_history_request().unwrap();
        state.apply(captured(test_item(1), 1));
        let current_request = state.take_pending_history_request().unwrap();

        state.apply_history_page_result(page_result(
            &old_request,
            Ok(UiHistoryPage {
                items: vec![test_item(99)],
                next_cursor: None,
            }),
        ));
        assert_ne!(state.snapshot.items, vec![test_item(99)]);

        state.apply_history_page_result(page_result(
            &current_request,
            Ok(UiHistoryPage {
                items: vec![test_item(1), test_item(0)],
                next_cursor: None,
            }),
        ));
        assert_eq!(state.snapshot.items[0], test_item(1));
    }

    /// 输入事件本身就必须使旧首页失效，不能等防抖到期后才建立隔离。
    #[test]
    fn 搜索输入立即拒绝旧首页结果() {
        let start = Instant::now();
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);
        let old_request = state.take_pending_history_request().unwrap();
        state.apply_at(UiEvent::SearchTextChanged("新查询".to_owned()), start);

        state.apply_history_page_result(page_result(
            &old_request,
            Ok(UiHistoryPage {
                items: vec![test_item(99)],
                next_cursor: None,
            }),
        ));
        assert_eq!(state.snapshot.items, vec![test_item(0)]);
        assert_eq!(state.search_status, SearchStatus::Loading);
    }

    /// 打开两轮面板后，第一轮的关闭事件必须被 reducer 拒绝。
    #[test]
    fn 过期关闭事件不会关闭新代次() {
        let mut state = UiState::default();

        assert_eq!(state.apply(UiEvent::OpenPanel), UiAction::Show);
        let first_generation = state.panel_generation();
        assert_eq!(
            state.apply(UiEvent::HidePanel {
                generation: first_generation
            }),
            UiAction::Hide
        );
        assert_eq!(state.apply(UiEvent::OpenPanel), UiAction::Show);
        let second_generation = state.panel_generation();
        assert!(second_generation > first_generation);

        assert_eq!(
            state.apply(UiEvent::HidePanel {
                generation: first_generation,
            }),
            UiAction::None
        );
        assert!(state.panel_visible);
        assert_eq!(
            state.apply(UiEvent::HidePanel {
                generation: second_generation,
            }),
            UiAction::Hide
        );
        assert!(!state.panel_visible);
    }

    /// 托盘打开必须在已显示时重新断言窗口，而不创建新的面板会话。
    #[test]
    fn 托盘打开幂等显示面板() {
        let mut state = UiState::default();

        assert_eq!(state.apply(UiEvent::ShowPanel), UiAction::Show);
        let generation = state.panel_generation();
        assert_eq!(state.apply(UiEvent::ShowPanel), UiAction::Reassert);
        assert!(state.panel_visible);
        assert_eq!(state.panel_generation(), generation);
    }

    /// 重复热键必须隐藏当前面板，形成与 Esc 一致的显式开关契约。
    #[test]
    fn 重复热键隐藏当前面板() {
        let start = Instant::now();
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0), test_item(1)],
            selected_index: Some(1),
        }));
        state.apply(UiEvent::OpenPanel);
        let _ = state.take_pending_history_request();
        state.apply_at(UiEvent::SearchTextChanged("条目".to_owned()), start);
        let before = state.snapshot();

        assert_eq!(state.apply(UiEvent::OpenPanel), UiAction::Hide);
        let after = state.snapshot();
        assert_eq!(after.snapshot, before.snapshot);
        assert_eq!(after.panel_generation, before.panel_generation);
        assert!(!after.panel_visible);
    }

    /// 首次显示失败必须恢复隐藏状态，使下一次热键重新执行 Show，而不是误走关闭。
    #[test]
    fn 显示失败后下一次热键重新打开() {
        let mut state = UiState::default();

        assert_eq!(state.apply(UiEvent::OpenPanel), UiAction::Show);
        let failed_generation = state.panel_generation();
        state.mark_panel_show_failed(failed_generation);
        assert!(!state.panel_visible);

        assert_eq!(state.apply(UiEvent::OpenPanel), UiAction::Show);
        assert!(state.panel_visible);
        assert!(state.panel_generation() > failed_generation);
    }

    /// 重新激活时的 show 失败保持既有可见会话，同时不能安排后续原生激活。
    #[test]
    fn 窗口重新激活失败保持当前会话() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0), test_item(1)],
            selected_index: Some(1),
        }));
        state.apply(UiEvent::OpenPanel);
        state.search_text = "保留查询".to_owned();
        assert_eq!(state.apply(UiEvent::ShowPanel), UiAction::Reassert);
        let before = state.snapshot();
        let activation_scheduled = std::cell::Cell::new(false);

        let result = perform_show_action(
            || Err::<(), _>("注入重新显示失败"),
            || activation_scheduled.set(true),
        );

        assert_eq!(result, Err("注入重新显示失败"));
        assert!(!activation_scheduled.get());
        assert_eq!(state.snapshot(), before);
    }

    /// 激活被 Windows 拒绝时只消费重试预算，耗尽后返回固定记录结果。
    #[cfg(windows)]
    #[test]
    fn 激活失败只重试或记录() {
        assert_eq!(
            activation_attempt(|| true, || false, 2),
            ActivationAttempt::Retry
        );
        assert_eq!(
            activation_attempt(|| true, || false, 0),
            ActivationAttempt::ActivationRejected
        );
        assert_eq!(
            activation_attempt(|| false, || true, 0),
            ActivationAttempt::TopmostRejected
        );
        assert_eq!(
            activation_attempt(|| true, || true, 2),
            ActivationAttempt::Done
        );
    }

    /// 第一次退出后，迟到的打开和关闭事件都必须被 reducer 拒绝。
    #[test]
    fn 退出闩锁拒绝后续事件() {
        let mut state = UiState::default();

        assert_eq!(state.apply(UiEvent::Quit), UiAction::Quit);
        assert!(state.quitting);
        assert!(!state.panel_visible);
        assert_eq!(state.apply(UiEvent::ShowPanel), UiAction::None);
        assert_eq!(state.apply(UiEvent::Quit), UiAction::None);
        assert_eq!(
            state.apply(UiEvent::HidePanel { generation: 1 }),
            UiAction::None
        );
        assert!(!state.panel_visible);
    }

    /// UI 接受退出时必须先关闭复制入口，关闭后新按钮事件不能进入后台。
    #[cfg(windows)]
    #[test]
    fn 退出关闭复制请求门禁() {
        let inbox = crate::clipboard::ClipboardCaptureInbox::new();
        super::bind_copy_request_inbox(inbox.clone());

        super::close_copy_request_gate();

        assert_eq!(
            inbox.request_copy(crate::clipboard::ClipboardCopyRequest::new(1, [1; 32])),
            Err(crate::clipboard::ClipboardWorkerError::Disconnected)
        );
    }

    /// 新捕获事件必须插入列表顶部，并且不能把完整正文带入 UI 状态。
    #[test]
    fn 捕获事件置顶摘要卡片() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(crate::command::UiSnapshot {
            items: vec![UiClipboardItem {
                id: 1,
                preview: "旧内容".to_owned(),
                source: "旧来源".to_owned(),
                relative_time: "1分钟前".to_owned(),
                content_hash: [1; 32],
                copy_count: 1,
                is_pinned: false,
                kind: Default::default(),
            }],
            selected_index: Some(0),
        }));

        assert_eq!(
            state.apply(captured(
                UiClipboardItem {
                    id: 2,
                    preview: "新摘要".to_owned(),
                    source: "新来源".to_owned(),
                    relative_time: "刚刚".to_owned(),
                    content_hash: [2; 32],
                    copy_count: 1,
                    is_pinned: false,
                    kind: Default::default(),
                },
                1,
            )),
            UiAction::None
        );
        assert_eq!(state.snapshot.items[0].preview, "新摘要");
        assert_eq!(state.snapshot.items[0].source, "新来源");
        assert_eq!(state.snapshot.items[0].relative_time, "刚刚");
        assert_eq!(state.snapshot.selected_index, Some(1));
    }

    /// 相同哈希再次捕获时，UI 只显示一张卡片并保留收藏和原始摘要。
    #[test]
    fn 捕获重复文本合并并保留收藏() {
        let mut state = UiState::default();
        state.apply(captured(
            UiClipboardItem {
                id: 1,
                preview: "收藏正文".to_owned(),
                source: "旧来源".to_owned(),
                relative_time: "之前".to_owned(),
                content_hash: [9; 32],
                copy_count: 1,
                is_pinned: true,
                kind: Default::default(),
            },
            1,
        ));
        state.apply(captured(
            UiClipboardItem {
                id: 1,
                preview: "收藏正文".to_owned(),
                source: "旧来源".to_owned(),
                relative_time: "刚刚".to_owned(),
                content_hash: [9; 32],
                copy_count: u64::MAX,
                is_pinned: true,
                kind: Default::default(),
            },
            2,
        ));

        assert_eq!(state.snapshot.items.len(), 1);
        assert_eq!(state.snapshot.items[0].id, 1);
        assert_eq!(state.snapshot.items[0].preview, "收藏正文");
        assert_eq!(state.snapshot.items[0].source, "旧来源");
        assert_eq!(state.snapshot.items[0].relative_time, "刚刚");
        assert_eq!(state.snapshot.items[0].copy_count, u64::MAX);
        assert!(state.snapshot.items[0].is_pinned);
    }

    /// 恢复快照去重后，原选中条目按哈希重定位而不是仅按旧索引截断。
    #[test]
    fn 恢复快照去重后保持选中条目身份() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(crate::command::UiSnapshot {
            items: vec![
                UiClipboardItem {
                    id: 1,
                    preview: "第一条".to_owned(),
                    source: "来源".to_owned(),
                    relative_time: "一".to_owned(),
                    content_hash: [1; 32],
                    copy_count: 1,
                    is_pinned: false,
                    kind: Default::default(),
                },
                UiClipboardItem {
                    id: 2,
                    preview: "重复条目".to_owned(),
                    source: "来源".to_owned(),
                    relative_time: "二".to_owned(),
                    content_hash: [1; 32],
                    copy_count: 1,
                    is_pinned: false,
                    kind: Default::default(),
                },
                UiClipboardItem {
                    id: 3,
                    preview: "第三条".to_owned(),
                    source: "来源".to_owned(),
                    relative_time: "三".to_owned(),
                    content_hash: [3; 32],
                    copy_count: 1,
                    is_pinned: false,
                    kind: Default::default(),
                },
            ],
            selected_index: Some(1),
        }));

        assert_eq!(state.snapshot.items.len(), 2);
        assert_eq!(state.snapshot.items[0].content_hash, [1; 32]);
        assert_eq!(state.snapshot.selected_index, Some(0));
    }

    /// UI 状态必须最多保留 2,000 条摘要，避免分页或持续捕获无限增长。
    #[test]
    fn ui_内存历史容量固定为两千条() {
        let mut state = UiState::default();
        for index in 0..(UI_HISTORY_MEMORY_CAPACITY + 20) {
            let mut content_hash = [0_u8; 32];
            content_hash[..8].copy_from_slice(&(index as u64).to_le_bytes());
            state.apply(captured(
                UiClipboardItem {
                    id: index as u64 + 1,
                    preview: format!("条目-{index}"),
                    source: "测试来源".to_owned(),
                    relative_time: "刚刚".to_owned(),
                    content_hash,
                    copy_count: 1,
                    is_pinned: false,
                    kind: Default::default(),
                },
                index as u64 + 1,
            ));
        }

        assert_eq!(state.snapshot.items.len(), UI_HISTORY_MEMORY_CAPACITY);
    }

    /// 窗口模型接收全部已加载摘要，由 ListView 负责只实例化视口附近卡片。
    #[test]
    fn 窗口模型包含全部已加载摘要() {
        let snapshot = UiSnapshot {
            items: (0..(UI_FIRST_BATCH_SIZE + 20)).map(test_item).collect(),
            selected_index: None,
        };

        assert_eq!(
            visible_snapshot_items(&snapshot).count(),
            UI_FIRST_BATCH_SIZE + 20
        );
    }

    /// 面板打开且存在首批记录时，选择应从第一张卡片开始。
    #[test]
    fn 打开面板默认选择第一项() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0), test_item(1)],
            selected_index: None,
        }));

        assert_eq!(state.apply(UiEvent::OpenPanel), UiAction::Show);
        assert_eq!(state.snapshot.selected_index, Some(0));
    }

    /// 鼠标点击携带的稳定身份必须选中对应卡片，并保持在快照的选中索引中。
    #[test]
    fn 鼠标按稳定身份选择卡片() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0), test_item(1), test_item(2)],
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);
        let generation = state.panel_generation();

        assert_eq!(
            state.apply(UiEvent::SelectItem {
                panel_generation: generation,
                id: 3,
                content_hash: [2; 32],
            }),
            UiAction::SelectItem
        );
        assert_eq!(state.snapshot.selected_index, Some(2));

        assert_eq!(state.snapshot.selected_index, Some(2));
    }

    /// 点击索引必须在排队前解析为当前面板代次、记录 ID 和内容哈希三元身份。
    #[test]
    fn 点击索引同步解析为稳定身份() {
        super::UI_STATE.with(|slot| {
            let mut state = slot.borrow_mut();
            *state = UiState::default();
            state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
                items: vec![test_item(0), test_item(1), test_item(2)],
                selected_index: None,
            }));
            state.apply(UiEvent::OpenPanel);
        });

        assert_eq!(
            super::resolve_card_selection(1),
            Some(UiEvent::SelectItem {
                panel_generation: 1,
                id: 2,
                content_hash: [1; 32],
            })
        );
        assert_eq!(super::resolve_card_selection(-1), None);
        assert_eq!(super::resolve_card_selection(3), None);
    }

    /// 面板关闭并重新打开后，旧代次的点击事件不能改变新会话选择。
    #[test]
    fn 旧面板代次点击被忽略() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0), test_item(1)],
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);
        let old_generation = state.panel_generation();
        state.apply(UiEvent::HidePanel {
            generation: old_generation,
        });
        state.apply(UiEvent::OpenPanel);

        assert_eq!(
            state.apply(UiEvent::SelectItem {
                panel_generation: old_generation,
                id: 2,
                content_hash: [1; 32],
            }),
            UiAction::None
        );
        assert_eq!(state.snapshot.selected_index, Some(0));
    }

    /// 点击事件排队后若列表已重排且原身份不在当前首批，不能误选相同索引的新条目。
    #[test]
    fn 迟到点击不会误选重排后的同索引条目() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0), test_item(1)],
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);
        let generation = state.panel_generation();

        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(2), test_item(0)],
            selected_index: Some(0),
        }));
        assert_eq!(
            state.apply(UiEvent::SelectItem {
                panel_generation: generation,
                id: 2,
                content_hash: [1; 32],
            }),
            UiAction::None
        );
        assert_eq!(state.snapshot.items[0].id, 3);
        assert_eq!(state.snapshot.selected_index, Some(0));
    }

    /// 稳定身份要求 ID 与哈希同时匹配，任一字段不同都不能选择记录。
    #[test]
    fn 鼠标选择同时校验记录_id_和哈希() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0), test_item(1)],
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);
        let generation = state.panel_generation();

        for (id, content_hash) in [(2, [0; 32]), (1, [1; 32])] {
            assert_eq!(
                state.apply(UiEvent::SelectItem {
                    panel_generation: generation,
                    id,
                    content_hash,
                }),
                UiAction::None
            );
            assert_eq!(state.snapshot.selected_index, Some(0));
        }
    }

    /// 关键词必须在防抖截止后应用，并把结果状态从 loading 转为 results。
    #[test]
    fn 搜索关键词在防抖后只保留匹配卡片() {
        let start = Instant::now();
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![
                UiClipboardItem {
                    preview: "Rust 代码".to_owned(),
                    ..test_item(0)
                },
                UiClipboardItem {
                    preview: "Slint 界面".to_owned(),
                    ..test_item(1)
                },
            ],
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);

        assert!(matches!(
            state.apply_at(UiEvent::SearchTextChanged("rust".to_owned()), start),
            UiAction::ScheduleSearch { generation: 1 }
        ));
        assert_eq!(state.search_status, SearchStatus::Loading);
        assert_eq!(state.snapshot.items.len(), 2);

        state.apply_at(
            UiEvent::SearchDebounceElapsed { generation: 1 },
            start + Duration::from_millis(120),
        );
        apply_success_page(
            &mut state,
            vec![UiClipboardItem {
                preview: "Rust 代码".to_owned(),
                ..test_item(0)
            }],
        );
        assert_eq!(state.search_status, SearchStatus::Results);
        assert_eq!(state.snapshot.items.len(), 1);
        assert_eq!(state.snapshot.items[0].preview, "Rust 代码");
        assert_eq!(state.snapshot.selected_index, Some(0));
    }

    /// 旧计时器到期时不能 poll 后续代次，只有最新输入的计时器可以恢复结果。
    #[test]
    fn 搜索旧计时器不会闪回新代次结果() {
        let start = Instant::now();
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![
                UiClipboardItem {
                    preview: "旧关键词".to_owned(),
                    ..test_item(0)
                },
                UiClipboardItem {
                    preview: "新关键词".to_owned(),
                    ..test_item(1)
                },
            ],
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);
        let _ = state.take_pending_history_request();

        state.apply_at(UiEvent::SearchTextChanged("旧".to_owned()), start);
        state.apply_at(
            UiEvent::SearchTextChanged("新".to_owned()),
            start + Duration::from_millis(30),
        );
        assert_eq!(
            state.apply_at(
                UiEvent::SearchDebounceElapsed { generation: 1 },
                start + Duration::from_millis(120),
            ),
            UiAction::None
        );
        assert_eq!(state.search_status, SearchStatus::Loading);
        assert_eq!(state.snapshot.items.len(), 2);

        state.apply_at(
            UiEvent::SearchDebounceElapsed { generation: 2 },
            start + Duration::from_millis(150),
        );
        apply_success_page(
            &mut state,
            vec![UiClipboardItem {
                preview: "新关键词".to_owned(),
                ..test_item(1)
            }],
        );
        assert_eq!(state.search_status, SearchStatus::Results);
        assert_eq!(state.snapshot.items[0].preview, "新关键词");
    }

    /// 收藏标签必须清空旧选择，并以 SQLite 返回的收藏首页替换当前有界缓存。
    #[test]
    fn 收藏筛选重置选择并只保留收藏记录() {
        let start = Instant::now();
        let mut state = UiState::default();
        let mut pinned = test_item(0);
        pinned.is_pinned = true;
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![pinned, test_item(1)],
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);
        let _ = state.take_pending_history_request();

        assert!(matches!(
            state.apply_at(UiEvent::SearchFilterChanged(SearchFilter::Pinned), start,),
            UiAction::ScheduleSearch { generation: 1 }
        ));
        assert_eq!(state.snapshot.selected_index, None);
        state.apply_at(
            UiEvent::SearchDebounceElapsed { generation: 1 },
            start + Duration::from_millis(120),
        );
        let mut pinned_result = test_item(0);
        pinned_result.is_pinned = true;
        apply_success_page(&mut state, vec![pinned_result]);
        assert_eq!(state.search_filter, SearchFilter::Pinned);
        assert_eq!(state.snapshot.items.len(), 1);
        assert!(state.snapshot.items[0].is_pinned);
        assert_eq!(state.snapshot.selected_index, Some(0));
        assert_eq!(state.history.items().len(), 1);
    }

    /// 全部和收藏允许混合类型，文本与图片标签在 SQLite 输入边界映射为固定类型。
    #[test]
    fn 全部文本图片与收藏查询映射正确() {
        let mut state = UiState::default();
        for (filter, expected_type, expected_pinned) in [
            (SearchFilter::All, None, None),
            (SearchFilter::Text, Some("text"), None),
            (SearchFilter::Image, Some("image"), None),
            (SearchFilter::Pinned, None, Some(true)),
        ] {
            state.search_filter = filter;
            let query = state.build_search_query();
            assert_eq!(query.item_type.as_deref(), expected_type);
            assert_eq!(query.is_pinned, expected_pinned);
        }
    }

    /// 面板关闭后到达的旧搜索事件不能重新显示结果或改变已取消的查询状态。
    #[test]
    fn 面板关闭后迟到搜索事件被丢弃() {
        let start = Instant::now();
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);
        state.apply_at(UiEvent::SearchTextChanged("条目".to_owned()), start);
        let generation = state.panel_generation();
        state.apply(UiEvent::HidePanel { generation });
        let before = state.snapshot.clone();
        state.apply_at(
            UiEvent::SearchDebounceElapsed { generation: 1 },
            start + Duration::from_millis(120),
        );
        assert_eq!(state.snapshot, before);
        assert!(!state.panel_visible);
    }

    /// 显式复制按钮只产生已校验的 ID/哈希动作，并同步选择对应卡片且保持面板可见。
    #[test]
    fn 显式复制按钮使用点击项稳定身份() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0), test_item(1)],
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);
        let generation = state.panel_generation();

        assert_eq!(
            state.apply(UiEvent::CopyItem {
                panel_generation: generation,
                id: 2,
                content_hash: [1; 32],
            }),
            UiAction::QueueCopy {
                id: 2,
                content_hash: [1; 32],
            }
        );
        assert_eq!(state.snapshot.selected_index, Some(1));
        assert!(state.panel_visible);
    }

    /// 图片复制必须通过 resolver 和 reducer 生成同一稳定 ID/哈希命令。
    #[test]
    fn 图片卡片进入复制队列() {
        let mut image = test_item(0);
        image.kind = UiClipboardItemKind::Image(UiImageSummary {
            thumbnail_path: std::path::PathBuf::from("thumbnail.webp"),
            width: 100,
            height: 80,
        });
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![image.clone()],
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);
        let generation = state.panel_generation();
        assert_eq!(
            state.apply(UiEvent::CopyItem {
                panel_generation: generation,
                id: image.id,
                content_hash: image.content_hash,
            }),
            UiAction::QueueCopy {
                id: image.id,
                content_hash: image.content_hash,
            }
        );
        assert_eq!(state.snapshot.selected_index, Some(0));
        assert!(state.panel_visible);

        let image_id = image.id;
        let image_hash = image.content_hash;
        super::UI_STATE.with(|slot| {
            let mut global = slot.borrow_mut();
            *global = UiState::default();
            global.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
                items: vec![image],
                selected_index: None,
            }));
            global.apply(UiEvent::OpenPanel);
        });
        assert!(matches!(
            super::resolve_copy_item(0),
            Some(UiEvent::CopyItem {
                id,
                content_hash,
                ..
            }) if id == image_id && content_hash == image_hash
        ));
    }

    /// 没有可见面板时的迟到复制按钮事件必须被 reducer 丢弃。
    #[test]
    fn 隐藏面板忽略迟到复制按钮事件() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));

        assert_eq!(
            state.apply(UiEvent::CopyItem {
                panel_generation: 1,
                id: 1,
                content_hash: [0; 32],
            }),
            UiAction::None
        );
    }

    /// 旧代次或当前列表中身份不匹配的按钮事件不能排入后台复制邮箱。
    #[test]
    fn 显式复制拒绝旧代次和错误身份() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0), test_item(1)],
            selected_index: None,
        }));
        state.apply(UiEvent::OpenPanel);
        let generation = state.panel_generation();

        for event in [
            UiEvent::CopyItem {
                panel_generation: generation.saturating_sub(1),
                id: 2,
                content_hash: [1; 32],
            },
            UiEvent::CopyItem {
                panel_generation: generation,
                id: 2,
                content_hash: [9; 32],
            },
            UiEvent::CopyItem {
                panel_generation: generation,
                id: 9,
                content_hash: [1; 32],
            },
        ] {
            assert_eq!(state.apply(event), UiAction::None);
            assert_eq!(state.snapshot.selected_index, Some(0));
        }
    }

    /// 复制按钮索引必须在排队前同步冻结为代次、ID 和哈希。
    #[test]
    fn 复制按钮索引同步解析为稳定身份() {
        super::UI_STATE.with(|slot| {
            let mut state = slot.borrow_mut();
            *state = UiState::default();
            state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
                items: vec![test_item(0), test_item(1)],
                selected_index: None,
            }));
            state.apply(UiEvent::OpenPanel);
        });

        assert_eq!(
            super::resolve_copy_item(1),
            Some(UiEvent::CopyItem {
                panel_generation: 1,
                id: 2,
                content_hash: [1; 32],
            })
        );
        assert_eq!(super::resolve_copy_item(-1), None);
        assert_eq!(super::resolve_copy_item(2), None);
    }

    /// 快照中存在首批以后的合法旧索引时，恢复逻辑必须保留该选择。
    #[test]
    fn 首批以后的合法选择被保留() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: (0..(UI_FIRST_BATCH_SIZE + 10)).map(test_item).collect(),
            selected_index: Some(UI_FIRST_BATCH_SIZE + 5),
        }));

        assert_eq!(state.snapshot.selected_index, Some(UI_FIRST_BATCH_SIZE + 5));
    }

    /// 外部快照带有越过实际条数的索引时，替换过程必须先夹到最后一条再恢复选择。
    #[test]
    fn 快照非法选择索引被安全夹紧() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0), test_item(1)],
            selected_index: Some(usize::MAX),
        }));

        assert_eq!(state.snapshot.selected_index, Some(1));
    }

    /// 选中项进入视口上方、下方和内容边界时，偏移必须使用负向 viewport-y 并夹紧。
    #[test]
    fn 选中项视口定位使用固定卡片高度() {
        assert_eq!(selection_viewport_y(0.0, 78.0, 0.0, 212.0, 1000.0), 0.0);
        assert_eq!(
            selection_viewport_y(424.0, 502.0, 0.0, 212.0, 1000.0),
            -290.0
        );
        assert_eq!(
            selection_viewport_y(3074.0, 3152.0, 0.0, 212.0, 3152.0),
            -2940.0
        );
        assert_eq!(
            selection_viewport_y(78.0, 156.0, -318.0, 212.0, 1000.0),
            -78.0
        );
        assert_eq!(selection_viewport_y(78.0, 156.0, 0.0, 0.0, 1000.0), 0.0);
    }

    /// 文本行高必须由生产常量固定为 78px；混合图片项使用 92px，不能回退到旧高度。
    #[test]
    fn 文本历史项使用七十八像素几何() {
        let snapshot = UiSnapshot {
            items: vec![test_item(0), test_item(1), test_image_item(2)],
            selected_index: None,
        };

        assert_eq!(selection_item_bounds(&snapshot, 0), Some((0.0, 78.0)));
        assert_eq!(selection_item_bounds(&snapshot, 1), Some((78.0, 156.0)));
        assert_eq!(selection_item_bounds(&snapshot, 2), Some((156.0, 248.0)));
    }

    /// 图片和文本使用各自固定高度，前序图片不能让后续选择偏移少算。
    #[test]
    fn 混合卡片选择边界累加图片高度() {
        let mut image = test_item(0);
        image.kind = UiClipboardItemKind::Image(UiImageSummary {
            thumbnail_path: std::path::PathBuf::from("thumbnail.webp"),
            width: 100,
            height: 80,
        });
        let snapshot = UiSnapshot {
            items: vec![image, test_item(1), test_item(2)],
            selected_index: Some(2),
        };

        assert_eq!(selection_item_bounds(&snapshot, 2), Some((170.0, 248.0)));
    }

    /// 图片行高必须由生产常量固定为 92px，并与前后文本行形成精确混合边界。
    #[test]
    fn 图片历史项使用九十二像素几何() {
        let snapshot = UiSnapshot {
            items: vec![
                test_item(0),
                test_image_item(1),
                test_item(2),
                test_image_item(3),
            ],
            selected_index: None,
        };

        assert_eq!(selection_item_bounds(&snapshot, 1), Some((78.0, 170.0)));
        assert_eq!(selection_item_bounds(&snapshot, 2), Some((170.0, 248.0)));
        assert_eq!(selection_item_bounds(&snapshot, 3), Some((248.0, 340.0)));
        assert_eq!(thumbnail_retained_range(&snapshot, -78, 92), 0..4);
    }

    /// 滚出视口的迟到结果虽然被拒绝，但必须结束在途身份，滚回后才能重新请求。
    #[test]
    fn 迟到缩略图结果释放在途身份() {
        let item = test_item(0);
        super::UI_THUMBNAIL_REQUESTED.with(|requested| {
            requested
                .borrow_mut()
                .insert((9, item.id, item.content_hash));
        });

        let applied = apply_thumbnail_result(
            &ThumbnailLoadResult {
                panel_generation: 9,
                id: item.id,
                content_hash: item.content_hash,
                outcome: Err(ThumbnailLoadFailure::Unavailable),
            },
            &UiSnapshot {
                items: vec![item.clone()],
                selected_index: None,
            },
        );

        assert!(!applied);
        super::UI_THUMBNAIL_REQUESTED.with(|requested| {
            assert!(!requested
                .borrow()
                .contains(&(9, item.id, item.content_hash)));
        });
    }

    /// 隐藏面板的模型刷新不得安排图片读取，并应清除旧可见集。
    #[test]
    fn 隐藏面板不调度缩略图() {
        super::UI_STATE.with(|state| {
            *state.borrow_mut() = UiState::default();
        });
        super::UI_THUMBNAIL_VISIBLE.with(|visible| {
            visible.borrow_mut().insert((99, [0x33; 32]));
        });

        schedule_thumbnail_requests(
            &UiSnapshot {
                items: vec![test_item(0)],
                selected_index: None,
            },
            4,
            0,
            500,
        );

        super::UI_THUMBNAIL_VISIBLE.with(|visible| {
            assert!(visible.borrow().is_empty());
        });
    }

    /// 缓存满载时按 LRU 逐项淘汰，不能整表清空造成同屏图片闪回。
    #[test]
    fn 缩略图缓存按单项淘汰保持有界() {
        super::UI_THUMBNAIL_CACHE.with(|cache| cache.borrow_mut().clear());
        super::UI_THUMBNAIL_CACHE_ORDER.with(|order| order.borrow_mut().clear());
        super::UI_THUMBNAIL_FAILED.with(|failed| failed.borrow_mut().clear());
        super::UI_THUMBNAIL_VISIBLE.with(|visible| visible.borrow_mut().clear());
        for id in 0_u64..THUMBNAIL_CACHE_CAPACITY as u64 {
            let identity = (id, [id as u8; 32]);
            reserve_thumbnail_cache_slot(identity);
            super::UI_THUMBNAIL_FAILED.with(|failed| {
                failed.borrow_mut().insert(identity);
            });
        }
        touch_thumbnail_cache((0, [0; 32]));
        let newest = (THUMBNAIL_CACHE_CAPACITY as u64, [0xF0; 32]);
        reserve_thumbnail_cache_slot(newest);
        super::UI_THUMBNAIL_FAILED.with(|failed| {
            failed.borrow_mut().insert(newest);
        });

        super::UI_THUMBNAIL_FAILED.with(|failed| {
            let failed = failed.borrow();
            assert_eq!(failed.len(), THUMBNAIL_CACHE_CAPACITY);
            assert!(!failed.contains(&(1, [1; 32])));
            assert!(failed.contains(&(0, [0; 32])));
            assert!(failed.contains(&newest));
        });
    }

    /// 混合文本和图片使用真实卡片高度计算可视区，并向前后扩展十条记录。
    #[test]
    fn 混合卡片缩略图保留范围按条目扩展十条() {
        let items = (0..32)
            .map(|index| {
                if index % 2 == 1 {
                    test_image_item(index)
                } else {
                    test_item(index)
                }
            })
            .collect::<Vec<_>>();
        let snapshot = UiSnapshot {
            items,
            selected_index: None,
        };

        // 首张图片从 78px 开始；78..170 的视口只覆盖该图片，缓冲应覆盖 0..12。
        assert_eq!(thumbnail_retained_range(&snapshot, -78, 92), 0..12);
        // 32 条交错行的新总高约 2720px；-1400 明确落在中部而非旧高度下的底部。
        let middle = thumbnail_retained_range(&snapshot, -1_400, 92);
        assert!(middle.start > 0);
        assert!(middle.len() <= THUMBNAIL_ITEM_BUFFER * 2 + 2);
    }

    /// 视口离开后必须同时释放 Rust 缓存和模型仍可能持有的范围外身份。
    #[test]
    fn 缩略图滚出保留范围后释放图片缓存() {
        super::UI_STATE.with(|state| {
            let mut state = state.borrow_mut();
            *state = UiState::default();
            state.apply(UiEvent::OpenPanel);
        });
        let items = (0..32).map(test_image_item).collect::<Vec<_>>();
        let first = (items[0].id, items[0].content_hash);
        let outside = (items[31].id, items[31].content_hash);
        let image = Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::new(1, 1));
        super::UI_THUMBNAIL_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.insert(first, image.clone());
            cache.insert(outside, image);
        });
        super::UI_THUMBNAIL_FAILED.with(|failed| {
            failed.borrow_mut().insert(outside);
        });
        super::UI_THUMBNAIL_CACHE_ORDER.with(|order| {
            let mut order = order.borrow_mut();
            order.extend([first, outside]);
        });

        let changed = schedule_thumbnail_requests(
            &UiSnapshot {
                items,
                selected_index: None,
            },
            1,
            0,
            92,
        );
        assert!(changed);
        super::UI_THUMBNAIL_CACHE.with(|cache| {
            let cache = cache.borrow();
            assert!(cache.contains_key(&first));
            assert!(!cache.contains_key(&outside));
        });
        super::UI_THUMBNAIL_FAILED.with(|failed| assert!(!failed.borrow().contains(&outside)));
        super::UI_THUMBNAIL_CACHE_ORDER.with(|order| {
            assert_eq!(order.borrow().as_slices().0, &[first]);
        });
    }

    /// 500 张图片连续进入缓存时容量保持硬上限，最近访问项优先留存。
    #[test]
    fn 滚动五百张图片缓存容量保持有界() {
        super::UI_THUMBNAIL_CACHE.with(|cache| cache.borrow_mut().clear());
        super::UI_THUMBNAIL_CACHE_ORDER.with(|order| order.borrow_mut().clear());
        super::UI_THUMBNAIL_FAILED.with(|failed| failed.borrow_mut().clear());
        for id in 0_u64..(THUMBNAIL_CACHE_CAPACITY as u64 + 50) {
            let identity = (id, [id as u8; 32]);
            reserve_thumbnail_cache_slot(identity);
            super::UI_THUMBNAIL_FAILED.with(|failed| {
                failed.borrow_mut().insert(identity);
            });
        }
        super::UI_THUMBNAIL_CACHE_ORDER.with(|order| {
            assert_eq!(order.borrow().len(), THUMBNAIL_CACHE_CAPACITY);
        });
        super::UI_THUMBNAIL_FAILED.with(|failed| {
            assert_eq!(failed.borrow().len(), THUMBNAIL_CACHE_CAPACITY);
        });
    }

    /// 收藏点击只建立 pending 和明确期望状态，数据库成功前不能乐观改变星标。
    #[test]
    fn 收藏点击不乐观更新且重复点击被拒绝() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let generation = state.panel_generation();

        let action = state.apply(UiEvent::PinItem {
            panel_generation: generation,
            id: 1,
            content_hash: [0; 32],
            is_pinned: true,
        });
        let UiAction::QueuePin(request) = action else {
            panic!("合法收藏点击必须生成后台请求");
        };
        assert_eq!(request.mutation_token, 1);
        assert!(!state.snapshot.items[0].is_pinned);
        assert_eq!(state.pending_pin_mutation, Some(request));
        assert_eq!(
            state.apply(UiEvent::PinItem {
                panel_generation: generation,
                id: 1,
                content_hash: [0; 32],
                is_pinned: true,
            }),
            UiAction::None
        );
        assert_eq!(state.pending_pin_mutation, Some(request));
    }

    /// 入队失败必须清除 pending、保持旧状态并展示固定失败提示。
    #[test]
    fn 收藏入队失败恢复按钮且不改变状态() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let UiAction::QueuePin(request) = state.apply(UiEvent::PinItem {
            panel_generation: state.panel_generation(),
            id: 1,
            content_hash: [0; 32],
            is_pinned: true,
        }) else {
            panic!("合法收藏点击必须生成后台请求");
        };

        state.mark_pin_submission_failed(&request);
        assert!(state.pending_pin_mutation.is_none());
        assert!(!state.snapshot.items[0].is_pinned);
        assert!(state.pin_error_visible);
    }

    /// 结果必须完整匹配活动五元身份；隐藏和重开不取消已接受事务。
    #[test]
    fn 收藏结果隔离迟到身份并允许隐藏期间完成() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let old_generation = state.panel_generation();
        let UiAction::QueuePin(request) = state.apply(UiEvent::PinItem {
            panel_generation: old_generation,
            id: 1,
            content_hash: [0; 32],
            is_pinned: true,
        }) else {
            panic!("合法收藏点击必须生成后台请求");
        };

        for stale in [
            PinMutationResult {
                mutation_token: request.mutation_token + 1,
                panel_generation: request.panel_generation,
                id: request.id,
                content_hash: request.content_hash,
                is_pinned: request.is_pinned,
                outcome: Ok(()),
            },
            PinMutationResult {
                mutation_token: request.mutation_token,
                panel_generation: request.panel_generation + 1,
                id: request.id,
                content_hash: request.content_hash,
                is_pinned: request.is_pinned,
                outcome: Ok(()),
            },
            PinMutationResult {
                mutation_token: request.mutation_token,
                panel_generation: request.panel_generation,
                id: request.id,
                content_hash: [9; 32],
                is_pinned: request.is_pinned,
                outcome: Ok(()),
            },
        ] {
            state.apply(UiEvent::PinMutationCompleted(stale));
            assert_eq!(state.pending_pin_mutation, Some(request));
            assert!(!state.snapshot.items[0].is_pinned);
        }

        state.apply(UiEvent::HidePanel {
            generation: old_generation,
        });
        state.apply(UiEvent::OpenPanel);
        state.apply(UiEvent::PinMutationCompleted(PinMutationResult {
            mutation_token: request.mutation_token,
            panel_generation: request.panel_generation,
            id: request.id,
            content_hash: request.content_hash,
            is_pinned: request.is_pinned,
            outcome: Ok(()),
        }));
        assert!(state.pending_pin_mutation.is_none());
        assert!(state.snapshot.items[0].is_pinned);
    }

    /// 收藏失败只清除 pending 和显示固定状态，不提前改变数据库状态镜像。
    #[test]
    fn 收藏失败保持旧状态() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let UiAction::QueuePin(request) = state.apply(UiEvent::PinItem {
            panel_generation: state.panel_generation(),
            id: 1,
            content_hash: [0; 32],
            is_pinned: true,
        }) else {
            panic!("合法收藏点击必须生成后台请求");
        };

        state.apply(UiEvent::PinMutationCompleted(PinMutationResult {
            mutation_token: request.mutation_token,
            panel_generation: request.panel_generation,
            id: request.id,
            content_hash: request.content_hash,
            is_pinned: request.is_pinned,
            outcome: Err(PinMutationFailure::StorageUnavailable),
        }));
        assert!(state.pending_pin_mutation.is_none());
        assert!(!state.snapshot.items[0].is_pinned);
        assert!(state.pin_error_visible);
    }

    /// 收藏筛选取消收藏要立即移除深分页记录，并使先前查询结果失效。
    #[test]
    fn 收藏筛选取消深分页记录并拒绝旧查询结果() {
        let mut items = (0..150).map(test_item).collect::<Vec<_>>();
        items[120].is_pinned = true;
        let target = items[120].clone();
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items,
            selected_index: Some(120),
        }));
        state.apply(UiEvent::OpenPanel);
        let old_request = state
            .take_pending_history_request()
            .expect("打开面板应存在旧首页请求");
        state.search_filter = SearchFilter::Pinned;

        let UiAction::QueuePin(request) = state.apply(UiEvent::PinItem {
            panel_generation: state.panel_generation(),
            id: target.id,
            content_hash: target.content_hash,
            is_pinned: false,
        }) else {
            panic!("深分页记录必须可取消收藏");
        };
        state.apply(UiEvent::PinMutationCompleted(PinMutationResult {
            mutation_token: request.mutation_token,
            panel_generation: request.panel_generation,
            id: request.id,
            content_hash: request.content_hash,
            is_pinned: request.is_pinned,
            outcome: Ok(()),
        }));
        assert!(!state.snapshot.items.iter().any(|item| item.id == target.id));
        assert!(state.take_pending_history_request().is_some());

        state.apply_history_page_result(page_result(
            &old_request,
            Ok(UiHistoryPage {
                items: vec![target.clone()],
                next_cursor: None,
            }),
        ));
        assert!(!state.snapshot.items.iter().any(|item| item.id == target.id));
    }

    /// 删除点击只建立 pending；事务成功前卡片不能从当前快照消失。
    #[test]
    fn 删除点击不乐观移除且重复点击被拒绝() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0), test_item(1)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let generation = state.panel_generation();

        let action = state.apply(UiEvent::DeleteItem {
            panel_generation: generation,
            id: 1,
            content_hash: [0; 32],
        });
        let UiAction::QueueDelete(request) = action else {
            panic!("合法删除点击必须生成后台请求");
        };
        assert_eq!(request.mutation_token, 1);
        assert_eq!(state.snapshot.items.len(), 2);
        assert_eq!(state.pending_delete_mutation, Some(request));
        assert_eq!(
            state.apply(UiEvent::DeleteItem {
                panel_generation: generation,
                id: 1,
                content_hash: [0; 32],
            }),
            UiAction::None
        );
        assert_eq!(state.snapshot.items.len(), 2);
    }

    /// 收藏与删除必须双向共享全局 mutation 锁，避免两个独立桥产生交错结果。
    #[test]
    fn 收藏与删除请求双向互斥() {
        let mut pin_first = UiState::default();
        pin_first.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        pin_first.apply(UiEvent::OpenPanel);
        let generation = pin_first.panel_generation();
        assert!(matches!(
            pin_first.apply(UiEvent::PinItem {
                panel_generation: generation,
                id: 1,
                content_hash: [0; 32],
                is_pinned: true,
            }),
            UiAction::QueuePin(_)
        ));
        assert_eq!(
            pin_first.apply(UiEvent::DeleteItem {
                panel_generation: generation,
                id: 1,
                content_hash: [0; 32],
            }),
            UiAction::None
        );

        let mut delete_first = UiState::default();
        delete_first.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        delete_first.apply(UiEvent::OpenPanel);
        let generation = delete_first.panel_generation();
        assert!(matches!(
            delete_first.apply(UiEvent::DeleteItem {
                panel_generation: generation,
                id: 1,
                content_hash: [0; 32],
            }),
            UiAction::QueueDelete(_)
        ));
        assert_eq!(
            delete_first.apply(UiEvent::PinItem {
                panel_generation: generation,
                id: 1,
                content_hash: [0; 32],
                is_pinned: true,
            }),
            UiAction::None
        );
    }

    /// 删除入队失败必须清 pending、保留原卡片并展示固定失败提示。
    #[test]
    fn 删除入队失败恢复按钮且保留记录() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let UiAction::QueueDelete(request) = state.apply(UiEvent::DeleteItem {
            panel_generation: state.panel_generation(),
            id: 1,
            content_hash: [0; 32],
        }) else {
            panic!("合法删除点击必须生成后台请求");
        };

        state.mark_delete_submission_failed(&request);
        assert!(state.pending_delete_mutation.is_none());
        assert_eq!(state.snapshot.items.len(), 1);
        assert!(state.delete_error_visible);
    }

    /// 删除结果必须匹配 pending；隐藏重开不能吞掉已提交成功。
    #[test]
    fn 删除结果隔离迟到身份并允许隐藏期间完成() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0), test_item(1)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let old_generation = state.panel_generation();
        let UiAction::QueueDelete(request) = state.apply(UiEvent::DeleteItem {
            panel_generation: old_generation,
            id: 1,
            content_hash: [0; 32],
        }) else {
            panic!("合法删除点击必须生成后台请求");
        };

        for stale in [
            DeleteMutationResult {
                mutation_token: request.mutation_token + 1,
                panel_generation: request.panel_generation,
                id: request.id,
                content_hash: request.content_hash,
                outcome: Ok(()),
            },
            DeleteMutationResult {
                mutation_token: request.mutation_token,
                panel_generation: request.panel_generation + 1,
                id: request.id,
                content_hash: request.content_hash,
                outcome: Ok(()),
            },
            DeleteMutationResult {
                mutation_token: request.mutation_token,
                panel_generation: request.panel_generation,
                id: request.id,
                content_hash: [9; 32],
                outcome: Ok(()),
            },
        ] {
            state.apply(UiEvent::DeleteMutationCompleted(stale));
            assert_eq!(state.pending_delete_mutation, Some(request));
            assert_eq!(state.snapshot.items.len(), 2);
        }

        state.apply(UiEvent::HidePanel {
            generation: old_generation,
        });
        state.apply(UiEvent::OpenPanel);
        state.apply(UiEvent::DeleteMutationCompleted(DeleteMutationResult {
            mutation_token: request.mutation_token,
            panel_generation: request.panel_generation,
            id: request.id,
            content_hash: request.content_hash,
            outcome: Ok(()),
        }));
        assert!(state.pending_delete_mutation.is_none());
        assert_eq!(state.snapshot.items.len(), 1);
        assert_eq!(state.snapshot.items[0].id, 2);
        assert_eq!(state.snapshot.selected_index, Some(0));
    }

    /// 存储失败只清 pending 和显示固定状态，当前记录与缓存都必须保留。
    #[test]
    fn 删除失败保持记录() {
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![test_item(0)],
            selected_index: Some(0),
        }));
        state.apply(UiEvent::OpenPanel);
        let UiAction::QueueDelete(request) = state.apply(UiEvent::DeleteItem {
            panel_generation: state.panel_generation(),
            id: 1,
            content_hash: [0; 32],
        }) else {
            panic!("合法删除点击必须生成后台请求");
        };

        state.apply(UiEvent::DeleteMutationCompleted(DeleteMutationResult {
            mutation_token: request.mutation_token,
            panel_generation: request.panel_generation,
            id: request.id,
            content_hash: request.content_hash,
            outcome: Err(DeleteMutationFailure::StorageUnavailable),
        }));
        assert!(state.pending_delete_mutation.is_none());
        assert_eq!(state.snapshot.items.len(), 1);
        assert_eq!(state.history.items().len(), 1);
        assert!(state.delete_error_visible);
    }

    /// 深分页删除要立即移除目标，并通过新数据集代次拒绝删除前的旧查询结果。
    #[test]
    fn 删除深分页记录并拒绝旧查询复活() {
        let items = (0..150).map(test_item).collect::<Vec<_>>();
        let target = items[120].clone();
        let mut state = UiState::default();
        state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
            items,
            selected_index: Some(120),
        }));
        state.apply(UiEvent::OpenPanel);
        let old_request = state
            .take_pending_history_request()
            .expect("打开面板应存在旧首页请求");
        // 打开面板会按产品契约先选中第一条；这里模拟用户已滚动并选中深分页目标。
        state.snapshot.selected_index = Some(120);

        let UiAction::QueueDelete(request) = state.apply(UiEvent::DeleteItem {
            panel_generation: state.panel_generation(),
            id: target.id,
            content_hash: target.content_hash,
        }) else {
            panic!("深分页记录必须可删除");
        };
        state.apply(UiEvent::DeleteMutationCompleted(DeleteMutationResult {
            mutation_token: request.mutation_token,
            panel_generation: request.panel_generation,
            id: request.id,
            content_hash: request.content_hash,
            outcome: Ok(()),
        }));
        assert!(!state.snapshot.items.iter().any(|item| item.id == target.id));
        assert_eq!(state.snapshot.selected_index, Some(120));
        assert!(state.take_pending_history_request().is_some());

        state.apply_history_page_result(page_result(
            &old_request,
            Ok(UiHistoryPage {
                items: vec![target.clone()],
                next_cursor: None,
            }),
        ));
        assert!(!state.snapshot.items.iter().any(|item| item.id == target.id));
    }

    /// 删除按钮索引必须在排队前同步冻结 ID、哈希和当前面板代次。
    #[test]
    fn 删除按钮索引同步解析为稳定身份() {
        super::UI_STATE.with(|slot| {
            let mut state = slot.borrow_mut();
            *state = UiState::default();
            state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
                items: vec![test_item(0), test_item(1)],
                selected_index: None,
            }));
            state.apply(UiEvent::OpenPanel);
        });

        assert_eq!(
            super::resolve_delete_item(1),
            Some(UiEvent::DeleteItem {
                panel_generation: 1,
                id: 2,
                content_hash: [1; 32],
            })
        );
        assert_eq!(super::resolve_delete_item(-1), None);
        assert_eq!(super::resolve_delete_item(2), None);
    }

    /// 收藏按钮索引必须在排队前同步冻结身份和相反的明确状态。
    #[test]
    fn 收藏按钮索引同步解析为稳定期望状态() {
        super::UI_STATE.with(|slot| {
            let mut state = slot.borrow_mut();
            *state = UiState::default();
            let mut pinned = test_item(1);
            pinned.is_pinned = true;
            state.apply(UiEvent::ReplaceSnapshot(UiSnapshot {
                items: vec![test_item(0), pinned],
                selected_index: None,
            }));
            state.apply(UiEvent::OpenPanel);
        });

        assert_eq!(
            super::resolve_pin_item(1),
            Some(UiEvent::PinItem {
                panel_generation: 1,
                id: 2,
                content_hash: [1; 32],
                is_pinned: false,
            })
        );
        assert_eq!(super::resolve_pin_item(-1), None);
        assert_eq!(super::resolve_pin_item(2), None);
    }
}

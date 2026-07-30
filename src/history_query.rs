//! 此模块实现 SQLite 历史分页的 UI 协调器、latest-wins 双向邮箱和后台查询线程。
//!
//! UI 只在短互斥区覆盖请求或提取结果；SQLite 同步查询始终在独立 worker 中执行。
//! generation、token 与 requested_cursor 共同构成响应身份，避免迟到首页污染新数据集。

use std::{
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    command::{UiClipboardItem, UiClipboardItemKind, UiImageSummary},
    storage::{HistoryCursor, HistoryPage, HistoryQuery, HistorySummary, StorageClient},
};

/// SQLite 首页固定请求数量。
pub const FIRST_PAGE_LIMIT: u32 = 30;
/// 滚动续页的标准批量。
pub const NEXT_PAGE_LIMIT: u32 = 50;
/// 单个 UI 数据集允许保留的最大摘要数，防止无限滚动持续增长内存。
pub const MAX_LOADED_ITEMS: usize = 2_000;

/// 数据集代次；搜索、捕获、隐藏或重新打开都会使旧代次失效。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryDatasetGeneration(u64);

impl HistoryDatasetGeneration {
    /// 返回可跨线程传输的数值身份。
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// 单次查询令牌；同一数据集未来可以依次分配首页和续页请求。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryRequestToken(u64);

impl HistoryRequestToken {
    /// 返回可跨线程传输的数值身份。
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// 后台查询请求；拥有全部筛选值，不借用 UI 字符串。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryPageRequest {
    /// 请求所属数据集。
    pub generation: HistoryDatasetGeneration,
    /// 本次请求的唯一令牌。
    pub token: HistoryRequestToken,
    /// 安全绑定参数和复合游标查询。
    pub query: HistoryQuery,
}

/// 对 UI 暴露的有限查询失败类别；不得携带 SQLite 详情或剪贴板正文。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryQueryFailure {
    /// 存储 worker、队列或 SQLite 查询不可用。
    StorageUnavailable,
    /// 摘要字段不能安全转换为 UI 卡片，整页拒绝。
    InvalidSummary,
}

/// 已转换为轻量 UI DTO 的一页结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiHistoryPage {
    /// 严格保持数据库顺序的卡片摘要。
    pub items: Vec<UiClipboardItem>,
    /// 数据库返回的下一页复合游标。
    pub next_cursor: Option<HistoryCursor>,
}

/// worker 返回的精确身份结果；requested_cursor 必须与活动请求完全一致。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryPageResult {
    /// 请求所属数据集。
    pub generation: HistoryDatasetGeneration,
    /// 请求令牌。
    pub token: HistoryRequestToken,
    /// worker 实际查询时使用的游标；首页必须为 None。
    pub requested_cursor: Option<HistoryCursor>,
    /// 成功页或有限失败类别。
    pub outcome: Result<UiHistoryPage, HistoryQueryFailure>,
}

/// UI 分页协调器的身份耗尽错误；禁止回绕复用旧身份。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryPageCoordinatorError {
    /// 数据集 generation 已耗尽。
    GenerationExhausted,
    /// 单次 request token 已耗尽。
    TokenExhausted,
    /// 尚未建立活动数据集。
    NoActiveDataset,
    /// 当前数据集已有一个尚未结束的请求，禁止覆盖其身份。
    RequestAlreadyActive,
    /// 当前数据集已经到达内存上限或没有下一页游标。
    DatasetExhausted,
    /// 续页失败后必须由显式用户动作解除重试门禁。
    RetryRequired,
}

/// 当前唯一活动请求的三元身份。
#[derive(Clone, Copy, Debug)]
struct ActiveRequest {
    /// 数据集 generation。
    generation: HistoryDatasetGeneration,
    /// 单次请求 token。
    token: HistoryRequestToken,
    /// 首页为空；WCB-INT-10 将使用复合游标。
    requested_cursor: Option<HistoryCursor>,
    /// 请求签发时的条目上限；worker 返回更多条目时必须整页拒绝。
    issued_limit: u32,
    /// 请求签发的单调时钟；只用于 request-to-accept 数值观测。
    requested_at: Instant,
}

impl ActiveRequest {
    /// 只比较跨线程传输的稳定身份，不把本地时钟或签发上限作为响应字段。
    fn matches(&self, result: &HistoryPageResult) -> bool {
        self.generation == result.generation
            && self.token == result.token
            && self.requested_cursor == result.requested_cursor
    }
}

/// 对外公开的纯数值分页性能快照，不包含查询、游标、路径或活动请求。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HistoryPerformanceSnapshot {
    /// 当前数据集已成功接受的页数。
    pub accepted_pages: u64,
    /// 当前数据集已加载的唯一条目总数。
    pub loaded_items: usize,
    /// 已加载条目中的文本数量。
    pub text_items: usize,
    /// 已加载条目中的图片数量。
    pub image_items: usize,
    /// 当前数据集在页内或跨页丢弃的重复条目数。
    pub duplicate_items: usize,
    /// 最近一次成功页从请求签发到 UI 接受的耗时。
    pub last_request_to_accept_duration: Duration,
    /// 当前数据集所有成功页 request-to-accept 耗时之和。
    pub total_request_to_accept_duration: Duration,
}

/// 协调器返回给 reducer 的纯分页决策；协调器不持有 Slint 模型。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoryPageApplication {
    /// 成功首页已经去重，应替换当前可见数据集。
    Replace(Vec<UiClipboardItem>),
    /// 成功续页已经去重，应追加到当前可见数据集。
    Append(Vec<UiClipboardItem>),
    /// 首页查询、提交或响应协议失败。
    FirstPageFailed,
    /// 续页查询、提交或响应协议失败，原游标仍可用于显式重试。
    NextPageFailed,
}

/// 当前数据集的私有可变分页状态；公开快照只复制其中的数值性能字段。
#[derive(Clone, Debug, Default)]
struct HistoryPaginationState {
    /// 最近成功页返回的数据库游标。
    next_cursor: Option<HistoryCursor>,
    /// 已接受的唯一条目数量。
    loaded_items: usize,
    /// 续页失败后是否等待显式重试动作。
    retry_required: bool,
    /// 与最终唯一条目同源更新的纯数值性能指标。
    performance: HistoryPerformanceSnapshot,
}

/// UI 线程独占的分页身份协调器，不执行查询也不触碰 Slint。
pub struct HistoryPageCoordinator {
    /// 最近分配的数据集 generation。
    next_generation: u64,
    /// 最近分配的请求 token。
    next_token: u64,
    /// 当前数据集；隐藏和退出时清空。
    current_generation: Option<HistoryDatasetGeneration>,
    /// 当前最多一个活动请求。
    active: Option<ActiveRequest>,
    /// 当前数据集的游标、容量、重试与性能状态。
    pagination: HistoryPaginationState,
}

impl Default for HistoryPageCoordinator {
    /// 创建尚未建立数据集的协调器。
    fn default() -> Self {
        Self {
            next_generation: 0,
            next_token: 0,
            current_generation: None,
            active: None,
            pagination: HistoryPaginationState::default(),
        }
    }
}

impl HistoryPageCoordinator {
    /// 开始新数据集并立即使旧 token 失效。
    pub fn begin_dataset(
        &mut self,
    ) -> Result<HistoryDatasetGeneration, HistoryPageCoordinatorError> {
        let next = self
            .next_generation
            .checked_add(1)
            .ok_or(HistoryPageCoordinatorError::GenerationExhausted)?;
        let generation = HistoryDatasetGeneration(next);
        self.next_generation = next;
        self.current_generation = Some(generation);
        self.active = None;
        self.pagination = HistoryPaginationState::default();
        Ok(generation)
    }

    /// 使当前数据集和活动请求失效；旧结果随后必然被拒绝。
    pub fn invalidate(&mut self) {
        self.current_generation = None;
        self.active = None;
        self.pagination = HistoryPaginationState::default();
    }

    /// 为当前数据集分配首页请求；强制 cursor=None、limit=30。
    pub fn request_first_page(
        &mut self,
        query: HistoryQuery,
    ) -> Result<HistoryPageRequest, HistoryPageCoordinatorError> {
        self.request_first_page_at(query, Instant::now())
    }

    /// 使用可注入单调时钟签发首页请求，供确定性耗时测试复用生产协议。
    fn request_first_page_at(
        &mut self,
        mut query: HistoryQuery,
        requested_at: Instant,
    ) -> Result<HistoryPageRequest, HistoryPageCoordinatorError> {
        let generation = self
            .current_generation
            .ok_or(HistoryPageCoordinatorError::NoActiveDataset)?;
        if self.active.is_some() {
            return Err(HistoryPageCoordinatorError::RequestAlreadyActive);
        }
        let next = self
            .next_token
            .checked_add(1)
            .ok_or(HistoryPageCoordinatorError::TokenExhausted)?;
        let token = HistoryRequestToken(next);
        query.cursor = None;
        query.limit = FIRST_PAGE_LIMIT;
        self.next_token = next;
        self.active = Some(ActiveRequest {
            generation,
            token,
            requested_cursor: None,
            issued_limit: query.limit,
            requested_at,
        });
        Ok(HistoryPageRequest {
            generation,
            token,
            query,
        })
    }

    /// 为当前数据集分配滚动续页请求；同一数据集只允许一个活动 token。
    pub fn request_next_page(
        &mut self,
        query: HistoryQuery,
    ) -> Result<HistoryPageRequest, HistoryPageCoordinatorError> {
        self.request_next_page_at(query, Instant::now())
    }

    /// 使用协调器私有游标和容量签发续页，并记录可注入的单调时钟起点。
    fn request_next_page_at(
        &mut self,
        mut query: HistoryQuery,
        requested_at: Instant,
    ) -> Result<HistoryPageRequest, HistoryPageCoordinatorError> {
        let generation = self
            .current_generation
            .ok_or(HistoryPageCoordinatorError::NoActiveDataset)?;
        if self.active.is_some() {
            return Err(HistoryPageCoordinatorError::RequestAlreadyActive);
        }
        if self.pagination.retry_required {
            return Err(HistoryPageCoordinatorError::RetryRequired);
        }
        let cursor = self
            .pagination
            .next_cursor
            .ok_or(HistoryPageCoordinatorError::DatasetExhausted)?;
        let remaining = MAX_LOADED_ITEMS.saturating_sub(self.pagination.loaded_items);
        if remaining == 0 {
            return Err(HistoryPageCoordinatorError::DatasetExhausted);
        }
        let next = self
            .next_token
            .checked_add(1)
            .ok_or(HistoryPageCoordinatorError::TokenExhausted)?;
        let token = HistoryRequestToken(next);
        query.cursor = Some(cursor);
        query.limit = NEXT_PAGE_LIMIT.min(u32::try_from(remaining).unwrap_or(u32::MAX));
        self.next_token = next;
        self.active = Some(ActiveRequest {
            generation,
            token,
            requested_cursor: Some(cursor),
            issued_limit: query.limit,
            requested_at,
        });
        Ok(HistoryPageRequest {
            generation,
            token,
            query,
        })
    }

    /// 接受当前可见数据集的一次精确身份响应，并返回已经去重的 reducer 决策。
    pub fn accept_page(
        &mut self,
        visible: bool,
        result: HistoryPageResult,
        current_items: &[UiClipboardItem],
    ) -> Option<HistoryPageApplication> {
        self.accept_page_at(visible, result, current_items, Instant::now())
    }

    /// 使用可注入接受时刻完成响应状态转移；时钟倒退时耗时按零处理。
    fn accept_page_at(
        &mut self,
        visible: bool,
        result: HistoryPageResult,
        current_items: &[UiClipboardItem],
        accepted_at: Instant,
    ) -> Option<HistoryPageApplication> {
        let active = self.active.as_ref()?;
        if !visible
            || self.current_generation != Some(result.generation)
            || !active.matches(&result)
        {
            return None;
        }
        let active = self.active.take().expect("已验证活动请求必须存在");
        let is_first_page = active.requested_cursor.is_none();
        let page = match result.outcome {
            Ok(page) if page.items.len() <= active.issued_limit as usize => page,
            Ok(_) | Err(_) => return Some(self.fail_page(is_first_page)),
        };

        let request_to_accept_duration = accepted_at
            .checked_duration_since(active.requested_at)
            .unwrap_or(Duration::ZERO);
        let application = if is_first_page {
            // 同一 generation 也可能因显式刷新重新签发首页；只有成功首页才能原子替换
            // 旧数据集观测，失败首页仍保留原卡片和指标供用户继续操作。
            self.pagination = HistoryPaginationState::default();
            let (items, duplicate_items) = deduplicate_page(page.items, &[]);
            self.pagination.next_cursor = (items.len() < MAX_LOADED_ITEMS)
                .then_some(page.next_cursor)
                .flatten();
            self.update_success_metrics(&items, duplicate_items, request_to_accept_duration);
            HistoryPageApplication::Replace(items)
        } else {
            let (items, duplicate_items) = deduplicate_page(page.items, current_items);
            let remaining = MAX_LOADED_ITEMS.saturating_sub(current_items.len());
            let items = items.into_iter().take(remaining).collect::<Vec<_>>();
            self.pagination.next_cursor = (current_items.len() + items.len() < MAX_LOADED_ITEMS)
                .then_some(page.next_cursor)
                .flatten();
            let mut final_items = Vec::with_capacity(current_items.len() + items.len());
            final_items.extend_from_slice(current_items);
            final_items.extend(items.iter().cloned());
            self.update_success_metrics(&final_items, duplicate_items, request_to_accept_duration);
            HistoryPageApplication::Append(items)
        };
        self.pagination.retry_required = false;
        Some(application)
    }

    /// 当请求无法提交到 worker 时，按原三元身份消费活动 token并返回固定失败决策。
    pub fn fail_submission(
        &mut self,
        visible: bool,
        request: &HistoryPageRequest,
    ) -> Option<HistoryPageApplication> {
        let active = self.active.as_ref()?;
        if !visible
            || self.current_generation != Some(request.generation)
            || active.generation != request.generation
            || active.token != request.token
            || active.requested_cursor != request.query.cursor
        {
            return None;
        }
        let active = self.active.take().expect("已验证活动请求必须存在");
        Some(self.fail_page(active.requested_cursor.is_none()))
    }

    /// 返回是否存在尚未结束的请求，供底部边沿状态机抑制重复派发。
    pub const fn has_active_request(&self) -> bool {
        self.active.is_some()
    }

    /// 返回当前数据集 generation，供确定性测试和捕获刷新门禁使用。
    pub const fn current_generation(&self) -> Option<HistoryDatasetGeneration> {
        self.current_generation
    }

    /// 返回公开纯性能快照；内部游标、重试和活动 token 不会越过该边界。
    pub const fn performance_snapshot(&self) -> HistoryPerformanceSnapshot {
        self.pagination.performance
    }

    /// 返回当前是否等待显式续页重试；只在 crate 内供 reducer 驱动 UI 提示。
    pub(crate) const fn retry_required(&self) -> bool {
        self.pagination.retry_required
    }

    /// 显式用户动作解除一次续页重试门禁，下一次请求仍会分配新 token。
    pub(crate) fn allow_retry(&mut self) {
        self.pagination.retry_required = false;
    }

    /// 返回最近成功页的数据库游标，仅供 reducer 和确定性测试验证内部状态。
    #[cfg(test)]
    pub(crate) const fn next_cursor(&self) -> Option<HistoryCursor> {
        self.pagination.next_cursor
    }

    /// 把当前页失败映射到固定决策；续页失败保留原游标并进入重试门禁。
    fn fail_page(&mut self, is_first_page: bool) -> HistoryPageApplication {
        if is_first_page {
            HistoryPageApplication::FirstPageFailed
        } else {
            self.pagination.retry_required = true;
            HistoryPageApplication::NextPageFailed
        }
    }

    /// 由最终唯一卡片集合一次性更新容量、分类和 request-to-accept 数值。
    fn update_success_metrics(
        &mut self,
        items: &[UiClipboardItem],
        duplicate_items: usize,
        duration: Duration,
    ) {
        let (text_items, image_items) = count_item_types(items);
        self.pagination.loaded_items = items.len();
        self.pagination.performance.accepted_pages =
            self.pagination.performance.accepted_pages.saturating_add(1);
        self.pagination.performance.loaded_items = items.len();
        self.pagination.performance.text_items = text_items;
        self.pagination.performance.image_items = image_items;
        self.pagination.performance.duplicate_items = self
            .pagination
            .performance
            .duplicate_items
            .saturating_add(duplicate_items);
        self.pagination.performance.last_request_to_accept_duration = duration;
        self.pagination.performance.total_request_to_accept_duration = self
            .pagination
            .performance
            .total_request_to_accept_duration
            .checked_add(duration)
            .unwrap_or(Duration::MAX);
    }
}

/// 按稳定记录 ID 对当前页去重；返回数据库顺序和本次丢弃数量。
fn deduplicate_page(
    items: Vec<UiClipboardItem>,
    existing: &[UiClipboardItem],
) -> (Vec<UiClipboardItem>, usize) {
    let mut seen = existing
        .iter()
        .map(|item| item.id)
        .collect::<std::collections::HashSet<_>>();
    let original_len = items.len();
    let items = items
        .into_iter()
        .filter(|item| seen.insert(item.id))
        .collect::<Vec<_>>();
    let duplicate_items = original_len.saturating_sub(items.len());
    (items, duplicate_items)
}

/// 从最终唯一卡片集合统计文本和图片数量，保证分类数与 loaded_items 同源。
fn count_item_types(items: &[UiClipboardItem]) -> (usize, usize) {
    items
        .iter()
        .fold((0, 0), |(text, image), item| match item.kind {
            UiClipboardItemKind::Text => (text.saturating_add(1), image),
            UiClipboardItemKind::Image(_) => (text, image.saturating_add(1)),
        })
}

/// latest-wins 请求邮箱的共享状态。
struct RequestState {
    /// 尚未被 worker 取走的最新请求。
    latest: Option<HistoryPageRequest>,
    /// 关闭后拒绝提交并唤醒 worker 退出。
    closed: bool,
}

/// UI 持有的请求发送端；提交只覆盖单槽，不等待 SQLite。
#[derive(Clone)]
pub struct HistoryRequestSender {
    /// 请求状态和阻塞 worker 使用的条件变量。
    shared: Arc<(Mutex<RequestState>, Condvar)>,
}

/// 查询 worker 持有的请求接收端。
pub struct HistoryRequestReceiver {
    /// 与 UI 发送端共享同一个 latest 槽。
    shared: Arc<(Mutex<RequestState>, Condvar)>,
}

/// 创建一对容量一 latest-wins 请求端点。
pub fn history_request_channel() -> (HistoryRequestSender, HistoryRequestReceiver) {
    let shared = Arc::new((
        Mutex::new(RequestState {
            latest: None,
            closed: false,
        }),
        Condvar::new(),
    ));
    (
        HistoryRequestSender {
            shared: Arc::clone(&shared),
        },
        HistoryRequestReceiver { shared },
    )
}

impl HistoryRequestSender {
    /// 非阻塞覆盖最新请求；短互斥区内不访问 SQLite 或 Slint。
    pub fn submit(&self, request: HistoryPageRequest) -> Result<(), HistoryBridgeClosed> {
        let (mutex, wake) = &*self.shared;
        let mut state = mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed {
            return Err(HistoryBridgeClosed);
        }
        state.latest = Some(request);
        wake.notify_one();
        Ok(())
    }

    /// 关闭请求入口、丢弃尚未取出的无效请求并唤醒 worker。
    pub fn close(&self) {
        let (mutex, wake) = &*self.shared;
        let mut state = mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closed = true;
        state.latest = None;
        wake.notify_all();
    }
}

impl HistoryRequestReceiver {
    /// 阻塞等待最新请求；关闭且槽为空时返回 None。
    fn wait_take_latest(&self) -> Option<HistoryPageRequest> {
        let (mutex, wake) = &*self.shared;
        let mut state = mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(request) = state.latest.take() {
                return Some(request);
            }
            if state.closed {
                return None;
            }
            state = wake
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// 取出当前最新请求而不等待；worker 用它跳过执行期间积累的中间请求。
    fn take_latest(&self) -> Option<HistoryPageRequest> {
        let (mutex, _) = &*self.shared;
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .latest
            .take()
    }
}

/// latest 结果槽及其 UI 唤醒闩锁。
struct ResultState {
    /// 尚未被 UI 提取的最新结果。
    latest: Option<HistoryPageResult>,
    /// true 表示已有一个 UI wake 在队列中，producer 不重复投递。
    wake_pending: bool,
    /// UI 退出或 wake 投递失败后拒绝新结果。
    closed: bool,
}

/// worker 持有的结果发布端。
#[derive(Clone)]
pub struct HistoryResultSender {
    /// latest、wake_pending 和 closed 必须由同一把锁保护。
    shared: Arc<Mutex<ResultState>>,
}

/// UI 线程持有的结果提取端。
#[derive(Clone)]
pub struct HistoryResultReceiver {
    /// 与 worker 发布端共享同一结果状态。
    shared: Arc<Mutex<ResultState>>,
}

/// 发布后是否需要投递一次 UI wake。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublishOutcome {
    /// 本次完成 false→true，需要投递 wake。
    Wake,
    /// 已有 wake 在途，只替换 latest。
    Coalesced,
    /// 结果桥已经关闭。
    Closed,
}

/// 结果桥已经关闭的稳定错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryBridgeClosed;

/// 创建一对 latest-wins 结果端点。
pub fn history_result_channel() -> (HistoryResultSender, HistoryResultReceiver) {
    let shared = Arc::new(Mutex::new(ResultState {
        latest: None,
        wake_pending: false,
        closed: false,
    }));
    (
        HistoryResultSender {
            shared: Arc::clone(&shared),
        },
        HistoryResultReceiver { shared },
    )
}

impl HistoryResultSender {
    /// 发布最新结果，并仅在 wake_pending 从 false 变 true 时要求唤醒 UI。
    fn publish(&self, result: HistoryPageResult) -> PublishOutcome {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed {
            return PublishOutcome::Closed;
        }
        state.latest = Some(result);
        if state.wake_pending {
            PublishOutcome::Coalesced
        } else {
            state.wake_pending = true;
            PublishOutcome::Wake
        }
    }

    /// wake 投递失败时关闭结果桥，防止 wake_pending 永久卡住。
    fn close_after_wake_failure(&self) {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closed = true;
        state.latest = None;
        state.wake_pending = false;
    }
}

impl HistoryResultReceiver {
    /// 在同一临界区取 latest 并清 wake_pending，保证读空边界不会丢后续唤醒。
    pub fn take_latest(&self) -> Option<HistoryPageResult> {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = state.latest.take();
        state.wake_pending = false;
        result
    }

    /// UI 退出时关闭结果入口并丢弃迟到结果。
    pub fn close(&self) {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closed = true;
        state.latest = None;
        state.wake_pending = false;
    }
}

/// 启动后台 SQLite 查询 worker；wake 回调只投递无正文的 UI 事件。
pub fn start_history_query_worker<F>(
    storage: StorageClient,
    requests: HistoryRequestReceiver,
    results: HistoryResultSender,
    mut wake_ui: F,
) -> std::io::Result<JoinHandle<()>>
where
    F: FnMut() -> bool + Send + 'static,
{
    thread::Builder::new()
        .name("clipboard-board-history-query".to_owned())
        .spawn(move || {
            run_history_query_loop(
                requests,
                results,
                |query| {
                    storage
                        .query_history_summaries(query)
                        .map_err(|_| HistoryQueryFailure::StorageUnavailable)
                        .and_then(convert_page)
                },
                &mut wake_ui,
            );
        })
}

/// 运行查询消费循环；查询函数接缝让并发测试无需真实 sleep 即可控制 Q1/Q3 乱序。
fn run_history_query_loop<Q, F>(
    requests: HistoryRequestReceiver,
    results: HistoryResultSender,
    mut query_page: Q,
    wake_ui: &mut F,
) where
    Q: FnMut(HistoryQuery) -> Result<UiHistoryPage, HistoryQueryFailure>,
    F: FnMut() -> bool,
{
    while let Some(mut request) = requests.wait_take_latest() {
        // worker 尚未开始 SQLite 前再次取 latest，尽量跳过 UI 快速覆盖的中间请求。
        while let Some(newer) = requests.take_latest() {
            request = newer;
        }
        let requested_cursor = request.query.cursor;
        let result = HistoryPageResult {
            generation: request.generation,
            token: request.token,
            requested_cursor,
            outcome: query_page(request.query),
        };
        match results.publish(result) {
            PublishOutcome::Wake if !wake_ui() => {
                results.close_after_wake_failure();
                break;
            }
            PublishOutcome::Wake | PublishOutcome::Coalesced => {}
            PublishOutcome::Closed => break,
        }
    }
}

/// 将整页摘要严格转换为 UI DTO；任一坏记录都会拒绝整页。
fn convert_page(page: HistoryPage) -> Result<UiHistoryPage, HistoryQueryFailure> {
    let now = unix_millis_now();
    let items = page
        .items
        .iter()
        .map(|summary| ui_item_from_summary(summary, now))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(UiHistoryPage {
        items,
        next_cursor: page.next_cursor,
    })
}

/// 将单条摘要转换为不含正文的 UI 卡片。
fn ui_item_from_summary(
    summary: &HistorySummary,
    now: i64,
) -> Result<UiClipboardItem, HistoryQueryFailure> {
    let kind = match summary.item_type.as_str() {
        "text" if summary.image.is_none() => UiClipboardItemKind::Text,
        "image" => {
            let image = summary
                .image
                .as_ref()
                .ok_or(HistoryQueryFailure::InvalidSummary)?;
            UiClipboardItemKind::Image(UiImageSummary {
                thumbnail_path: image.thumbnail_absolute_path(),
                width: image.metadata.width().get(),
                height: image.metadata.height().get(),
            })
        }
        _ => return Err(HistoryQueryFailure::InvalidSummary),
    };
    let id = u64::try_from(summary.id).map_err(|_| HistoryQueryFailure::InvalidSummary)?;
    let copy_count =
        u64::try_from(summary.copy_count).map_err(|_| HistoryQueryFailure::InvalidSummary)?;
    if id == 0 || copy_count == 0 {
        return Err(HistoryQueryFailure::InvalidSummary);
    }
    let source = summary
        .source_app
        .as_deref()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            summary
                .source_exe
                .as_deref()
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("未知来源")
        .to_owned();
    Ok(UiClipboardItem {
        id,
        preview: summary.preview_text.clone(),
        source,
        relative_time: relative_time(summary.copied_at, now),
        content_hash: summary.content_hash,
        copy_count,
        is_pinned: summary.is_pinned,
        kind,
    })
}

/// 返回不会溢出的 Unix 毫秒时间。
fn unix_millis_now() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

/// 将复制时间转换为短相对时间文案。
fn relative_time(copied_at: i64, now: i64) -> String {
    let age = now.saturating_sub(copied_at).max(0) as u64;
    if age < 60_000 {
        "刚刚".to_owned()
    } else if age < 3_600_000 {
        format!("{}分钟前", age / 60_000)
    } else {
        format!("{}小时前", age / 3_600_000)
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证三元身份、latest-wins 覆盖和结果唤醒边界。

    use super::*;
    use crate::{
        command::UiClipboardItemKind,
        domain::{ImageAssetRootId, ImageMetadata},
        storage::{HistoryImageSummary, StorageExecutor, TextUpsertInput},
    };
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::sync_channel,
    };

    static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    /// 创建当前测试独占的 SQLite 目录。
    fn test_directory() -> std::path::PathBuf {
        let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "clipboard-board-wcb-int-09-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("创建查询测试目录失败");
        directory
    }

    /// 生成固定首页查询。
    fn query(keyword: &str) -> HistoryQuery {
        HistoryQuery {
            keyword: Some(keyword.to_owned()),
            limit: FIRST_PAGE_LIMIT,
            ..HistoryQuery::default()
        }
    }

    /// 生成成功的空首页响应。
    fn empty_result(request: &HistoryPageRequest) -> HistoryPageResult {
        HistoryPageResult {
            generation: request.generation,
            token: request.token,
            requested_cursor: request.query.cursor,
            outcome: Ok(UiHistoryPage {
                items: Vec::new(),
                next_cursor: None,
            }),
        }
    }

    /// 生成只含稳定身份的文本卡片，分页测试不需要真实正文或文件系统。
    fn text_item(id: u64) -> UiClipboardItem {
        UiClipboardItem {
            id,
            preview: format!("文本-{id}"),
            source: "测试".to_owned(),
            relative_time: "刚刚".to_owned(),
            content_hash: [id as u8; 32],
            copy_count: 1,
            is_pinned: false,
            kind: UiClipboardItemKind::Text,
        }
    }

    /// 生成只含虚拟缩略图定位的图片卡片，测试不会读取该路径。
    fn image_item(id: u64) -> UiClipboardItem {
        UiClipboardItem {
            id,
            preview: format!("图片-{id}"),
            source: "测试".to_owned(),
            relative_time: "刚刚".to_owned(),
            content_hash: [id as u8; 32],
            copy_count: 1,
            is_pinned: false,
            kind: UiClipboardItemKind::Image(UiImageSummary {
                thumbnail_path: std::path::PathBuf::from("thumbnail.webp"),
                width: 320,
                height: 200,
            }),
        }
    }

    /// 用指定卡片和数据库游标构造与请求精确匹配的成功结果。
    fn successful_result(
        request: &HistoryPageRequest,
        items: Vec<UiClipboardItem>,
        next_cursor: Option<HistoryCursor>,
    ) -> HistoryPageResult {
        HistoryPageResult {
            generation: request.generation,
            token: request.token,
            requested_cursor: request.query.cursor,
            outcome: Ok(UiHistoryPage { items, next_cursor }),
        }
    }

    /// 旧 generation、错误 token、非空 cursor 和重复响应都必须被拒绝。
    #[test]
    fn 首页响应必须精确匹配活动三元身份且只接受一次() {
        let mut coordinator = HistoryPageCoordinator::default();
        coordinator.begin_dataset().expect("建立数据集失败");
        let request = coordinator
            .request_first_page(query("current"))
            .expect("创建首页请求失败");
        let mut wrong = empty_result(&request);
        wrong.token = HistoryRequestToken(request.token.as_u64() + 1);
        assert_eq!(coordinator.accept_page(true, wrong, &[]), None);
        wrong = empty_result(&request);
        wrong.requested_cursor = Some(HistoryCursor {
            copied_at: 1,
            id: 1,
        });
        assert_eq!(coordinator.accept_page(true, wrong, &[]), None);
        let result = empty_result(&request);
        assert_eq!(
            coordinator.accept_page(true, result.clone(), &[]),
            Some(HistoryPageApplication::Replace(Vec::new()))
        );
        assert_eq!(coordinator.accept_page(true, result, &[]), None);
    }

    /// 续页批量必须按 2,000 上限收缩，并拒绝覆盖同代次活动请求。
    #[test]
    fn 续页请求遵守单活动身份和容量边界() {
        let cursor = HistoryCursor {
            copied_at: 42,
            id: 7,
        };
        for (loaded, expected) in [(1_950, 50), (1_980, 20), (1_999, 1)] {
            let mut coordinator = HistoryPageCoordinator::default();
            coordinator.begin_dataset().unwrap();
            coordinator.pagination.next_cursor = Some(cursor);
            coordinator.pagination.loaded_items = loaded;
            let request = coordinator.request_next_page(query("page")).unwrap();
            assert_eq!(request.query.cursor, Some(cursor));
            assert_eq!(request.query.limit, expected);
            assert_eq!(
                coordinator.request_next_page(query("duplicate")),
                Err(HistoryPageCoordinatorError::RequestAlreadyActive)
            );
        }

        for loaded in [2_000, 2_001] {
            let mut coordinator = HistoryPageCoordinator::default();
            coordinator.begin_dataset().unwrap();
            coordinator.pagination.next_cursor = Some(cursor);
            coordinator.pagination.loaded_items = loaded;
            assert_eq!(
                coordinator.request_next_page(query("full")),
                Err(HistoryPageCoordinatorError::DatasetExhausted)
            );
        }
    }

    /// 续页结果和提交失败都必须精确匹配 generation、token 与 requested_cursor。
    #[test]
    fn 续页身份严格匹配且提交失败释放活动请求() {
        let cursor = HistoryCursor {
            copied_at: 100,
            id: 10,
        };
        let mut coordinator = HistoryPageCoordinator::default();
        coordinator.begin_dataset().unwrap();
        coordinator.pagination.next_cursor = Some(cursor);
        coordinator.pagination.loaded_items = 30;
        let request = coordinator.request_next_page(query("page")).unwrap();
        let mut wrong = empty_result(&request);
        wrong.requested_cursor = Some(HistoryCursor {
            copied_at: 99,
            id: 9,
        });
        assert_eq!(coordinator.accept_page(true, wrong, &[]), None);
        assert_eq!(
            coordinator.fail_submission(true, &request),
            Some(HistoryPageApplication::NextPageFailed)
        );
        assert_eq!(coordinator.fail_submission(true, &request), None);
        assert!(!coordinator.has_active_request());
    }

    /// worker 取槽前连续提交只保留最后一个请求。
    #[test]
    fn 请求槽在消费前只保留最新请求() {
        let (sender, receiver) = history_request_channel();
        let mut coordinator = HistoryPageCoordinator::default();
        coordinator.begin_dataset().expect("建立数据集失败");
        let first = coordinator.request_first_page(query("a")).unwrap();
        coordinator.begin_dataset().expect("推进数据集失败");
        let second = coordinator.request_first_page(query("ab")).unwrap();
        coordinator.begin_dataset().expect("推进数据集失败");
        let third = coordinator.request_first_page(query("abc")).unwrap();
        sender.submit(first).unwrap();
        sender.submit(second).unwrap();
        sender.submit(third.clone()).unwrap();
        assert_eq!(receiver.wait_take_latest(), Some(third));
        sender.close();
        assert!(receiver.wait_take_latest().is_none());
    }

    /// Q1 执行期间到达的 Q2/Q3 只允许 Q3 在 Q1 后执行，中间请求必须被覆盖。
    #[test]
    fn 查询执行期间的新请求只保留最后一个() {
        let (sender, receiver) = history_request_channel();
        let (result_sender, _result_receiver) = history_result_channel();
        let (executed_sender, executed_receiver) = sync_channel(2);
        let (release_sender, release_receiver) = sync_channel(1);
        let worker = thread::spawn(move || {
            run_history_query_loop(
                receiver,
                result_sender,
                |query| {
                    let keyword = query.keyword.unwrap_or_default();
                    executed_sender
                        .send(keyword.clone())
                        .expect("发送执行关键词失败");
                    if keyword == "q1" {
                        release_receiver.recv().expect("等待释放 Q1 失败");
                    }
                    Ok(UiHistoryPage {
                        items: Vec::new(),
                        next_cursor: None,
                    })
                },
                &mut || true,
            );
        });

        let mut coordinator = HistoryPageCoordinator::default();
        coordinator.begin_dataset().unwrap();
        sender
            .submit(coordinator.request_first_page(query("q1")).unwrap())
            .unwrap();
        assert_eq!(executed_receiver.recv().unwrap(), "q1");
        coordinator.begin_dataset().unwrap();
        sender
            .submit(coordinator.request_first_page(query("q2")).unwrap())
            .unwrap();
        coordinator.begin_dataset().unwrap();
        sender
            .submit(coordinator.request_first_page(query("q3")).unwrap())
            .unwrap();
        release_sender.send(()).unwrap();
        assert_eq!(executed_receiver.recv().unwrap(), "q3");
        sender.close();
        worker.join().expect("查询循环线程 panic");
        assert!(executed_receiver.try_recv().is_err());
    }

    /// pending=true 时连续发布只需一个 wake，UI 提取时得到最新结果。
    #[test]
    fn 结果槽合并唤醒并交付最新结果() {
        let (sender, receiver) = history_result_channel();
        let mut coordinator = HistoryPageCoordinator::default();
        coordinator.begin_dataset().unwrap();
        let first = coordinator.request_first_page(query("a")).unwrap();
        coordinator.begin_dataset().unwrap();
        let second = coordinator.request_first_page(query("b")).unwrap();
        assert_eq!(sender.publish(empty_result(&first)), PublishOutcome::Wake);
        assert_eq!(
            sender.publish(empty_result(&second)),
            PublishOutcome::Coalesced
        );
        assert_eq!(receiver.take_latest(), Some(empty_result(&second)));
    }

    /// UI 清空 wake_pending 后 producer 再写必须重新产生 wake，不能卡在读空边界。
    #[test]
    fn 结果在_ui_读空后写入会再次唤醒() {
        let (sender, receiver) = history_result_channel();
        assert!(receiver.take_latest().is_none());
        let mut coordinator = HistoryPageCoordinator::default();
        coordinator.begin_dataset().unwrap();
        let request = coordinator.request_first_page(query("late")).unwrap();
        assert_eq!(sender.publish(empty_result(&request)), PublishOutcome::Wake);
        assert!(receiver.take_latest().is_some());
    }

    /// UI wake 投递失败后结果桥必须关闭，不能把 wake_pending 永久留在 true。
    #[test]
    fn 唤醒失败关闭结果桥且后续发布不阻塞() {
        let (sender, receiver) = history_result_channel();
        let mut coordinator = HistoryPageCoordinator::default();
        coordinator.begin_dataset().unwrap();
        let request = coordinator
            .request_first_page(query("wake-failed"))
            .unwrap();
        assert_eq!(sender.publish(empty_result(&request)), PublishOutcome::Wake);
        sender.close_after_wake_failure();
        assert_eq!(
            sender.publish(empty_result(&request)),
            PublishOutcome::Closed
        );
        assert!(receiver.take_latest().is_none());
    }

    /// SQLite worker 必须能检索启动内存 100 条之外的旧记录，并只交付 30 条首页。
    #[test]
    fn 后台查询可命中一百条之外记录() {
        let directory = test_directory();
        let storage = StorageExecutor::open_at(&directory).expect("启动查询存储失败");
        for index in 0_u8..150 {
            let text = if index == 0 {
                "deep-target".to_owned()
            } else {
                format!("普通记录-{index}")
            };
            storage
                .upsert_text(TextUpsertInput {
                    content_hash: [index; 32],
                    text_content: text.clone(),
                    preview_text: text,
                    source_exe: None,
                    source_app: None,
                    copied_at: i64::from(index),
                })
                .expect("写入查询测试历史失败");
        }

        let (request_sender, request_receiver) = history_request_channel();
        let (result_sender, result_receiver) = history_result_channel();
        let (wake_sender, wake_receiver) = sync_channel(1);
        let worker = start_history_query_worker(
            storage.client(),
            request_receiver,
            result_sender,
            move || wake_sender.send(()).is_ok(),
        )
        .expect("启动历史查询 worker 失败");
        let mut coordinator = HistoryPageCoordinator::default();
        coordinator.begin_dataset().unwrap();
        let request = coordinator
            .request_first_page(query("deep-target"))
            .unwrap();
        request_sender.submit(request.clone()).unwrap();
        wake_receiver.recv().expect("查询结果未唤醒 UI");
        let result = result_receiver.take_latest().expect("结果槽为空");
        assert_eq!(result.generation, request.generation);
        let page = result.outcome.expect("SQLite 首页查询失败");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].preview, "deep-target");

        request_sender.close();
        result_receiver.close();
        worker.join().expect("历史查询 worker 异常退出");
        drop(storage);
        std::fs::remove_dir_all(directory).expect("清理查询测试目录失败");
    }

    /// 非文本摘要不能进入当前文本卡片模型，删除按钮因此不会绕过图片生命周期门禁。
    #[test]
    fn 非文本摘要不能转换为界面卡片() {
        let summary = crate::storage::HistorySummary {
            id: 1,
            item_type: "image".to_owned(),
            preview_text: "图片预览".to_owned(),
            content_hash: [9; 32],
            source_exe: None,
            source_app: None,
            copy_count: 1,
            is_pinned: false,
            created_at: 1,
            copied_at: 1,
            last_used_at: None,
            image: None,
        };

        assert_eq!(
            ui_item_from_summary(&summary, 1),
            Err(HistoryQueryFailure::InvalidSummary)
        );
    }

    /// 完整图片摘要必须转换为带尺寸、缩略图定位和复制能力的卡片。
    #[test]
    fn 图片摘要转换为可复制界面卡片() {
        let hash_hex = "aa".repeat(32);
        let metadata = ImageMetadata::new(
            [0xaa; 32],
            ImageAssetRootId::new([0xbb; 32]),
            format!("aa/{hash_hex}.png"),
            format!("aa/{hash_hex}.webp"),
            320,
            200,
            512,
        )
        .expect("构造查询图片元数据失败");
        let summary = crate::storage::HistorySummary {
            id: 1,
            item_type: "image".to_owned(),
            preview_text: "图片 320 × 200".to_owned(),
            content_hash: [0xaa; 32],
            source_exe: None,
            source_app: Some("截图工具".to_owned()),
            copy_count: 1,
            is_pinned: false,
            created_at: 1,
            copied_at: 1,
            last_used_at: None,
            image: Some(HistoryImageSummary {
                metadata,
                canonical_root: std::path::PathBuf::from(r"C:\images"),
            }),
        };

        let item = ui_item_from_summary(&summary, 1).expect("转换图片摘要失败");
        let UiClipboardItemKind::Image(image) = &item.kind else {
            panic!("转换结果应为图片");
        };
        assert_eq!((image.width, image.height), (320, 200));
        assert!(image
            .thumbnail_path
            .ends_with(format!("aa/{hash_hex}.webp")));
        assert!(item.copy_enabled());
    }

    /// 文本与图片页必须更新同一份纯性能快照，分类数与唯一条目总数保持同源。
    #[test]
    fn 混合页共用统一分页状态和性能快照() {
        let started_at = Instant::now();
        let accepted_at = started_at + Duration::from_millis(17);
        let mut coordinator = HistoryPageCoordinator::default();
        coordinator.begin_dataset().expect("建立数据集失败");
        let request = coordinator
            .request_first_page_at(query("mixed"), started_at)
            .expect("创建首页请求失败");
        let text = UiClipboardItem {
            id: 1,
            preview: "文本".to_owned(),
            source: "测试".to_owned(),
            relative_time: "刚刚".to_owned(),
            content_hash: [1; 32],
            copy_count: 1,
            is_pinned: false,
            kind: UiClipboardItemKind::Text,
        };
        let image = UiClipboardItem {
            id: 2,
            preview: "图片".to_owned(),
            source: "测试".to_owned(),
            relative_time: "刚刚".to_owned(),
            content_hash: [2; 32],
            copy_count: 1,
            is_pinned: false,
            kind: UiClipboardItemKind::Image(UiImageSummary {
                thumbnail_path: std::path::PathBuf::from("thumbnail.webp"),
                width: 320,
                height: 200,
            }),
        };
        let result = HistoryPageResult {
            generation: request.generation,
            token: request.token,
            requested_cursor: request.query.cursor,
            outcome: Ok(UiHistoryPage {
                items: vec![text.clone(), image.clone(), image.clone()],
                next_cursor: None,
            }),
        };

        assert_eq!(
            coordinator.accept_page_at(true, result, &[], accepted_at),
            Some(HistoryPageApplication::Replace(vec![text, image]))
        );
        assert_eq!(
            coordinator.performance_snapshot(),
            HistoryPerformanceSnapshot {
                accepted_pages: 1,
                loaded_items: 2,
                text_items: 1,
                image_items: 1,
                duplicate_items: 1,
                last_request_to_accept_duration: Duration::from_millis(17),
                total_request_to_accept_duration: Duration::from_millis(17),
            }
        );
    }

    /// worker 返回超过签发 limit 的页必须整页失败，不能截断后接受已经前移的游标。
    #[test]
    fn 超过签发_limit_的续页整页拒绝并保留原游标() {
        let started_at = Instant::now();
        let mut coordinator = HistoryPageCoordinator::default();
        coordinator.begin_dataset().expect("建立数据集失败");
        let first = coordinator
            .request_first_page_at(query("oversized"), started_at)
            .expect("创建首页请求失败");
        let first_item = UiClipboardItem {
            id: 1,
            preview: "首页".to_owned(),
            source: "测试".to_owned(),
            relative_time: "刚刚".to_owned(),
            content_hash: [1; 32],
            copy_count: 1,
            is_pinned: false,
            kind: UiClipboardItemKind::Text,
        };
        let original_cursor = HistoryCursor {
            copied_at: 100,
            id: 1,
        };
        let first_result = HistoryPageResult {
            generation: first.generation,
            token: first.token,
            requested_cursor: first.query.cursor,
            outcome: Ok(UiHistoryPage {
                items: vec![first_item.clone()],
                next_cursor: Some(original_cursor),
            }),
        };
        let first_application = coordinator
            .accept_page_at(
                true,
                first_result,
                &[],
                started_at + Duration::from_millis(3),
            )
            .expect("首页应被接受");
        assert_eq!(
            first_application,
            HistoryPageApplication::Replace(vec![first_item.clone()])
        );
        let metrics_before = coordinator.performance_snapshot();
        let next = coordinator
            .request_next_page_at(query("oversized"), started_at + Duration::from_millis(4))
            .expect("创建续页请求失败");
        assert_eq!(next.query.limit, NEXT_PAGE_LIMIT);
        let oversized_items = (2..=u64::from(NEXT_PAGE_LIMIT) + 2)
            .map(|id| UiClipboardItem {
                id,
                preview: format!("续页-{id}"),
                source: "测试".to_owned(),
                relative_time: "刚刚".to_owned(),
                content_hash: [id as u8; 32],
                copy_count: 1,
                is_pinned: false,
                kind: UiClipboardItemKind::Text,
            })
            .collect();
        let advanced_cursor = HistoryCursor {
            copied_at: 1,
            id: 999,
        };
        let oversized_result = HistoryPageResult {
            generation: next.generation,
            token: next.token,
            requested_cursor: next.query.cursor,
            outcome: Ok(UiHistoryPage {
                items: oversized_items,
                next_cursor: Some(advanced_cursor),
            }),
        };

        assert_eq!(
            coordinator.accept_page_at(
                true,
                oversized_result,
                &[first_item],
                started_at + Duration::from_millis(8),
            ),
            Some(HistoryPageApplication::NextPageFailed)
        );
        assert_eq!(coordinator.next_cursor(), Some(original_cursor));
        assert!(coordinator.retry_required());
        assert_eq!(coordinator.performance_snapshot(), metrics_before);
    }

    /// 显式测试时钟早于签发起点时耗时按零处理，不能 panic 或产生虚假大值。
    #[test]
    fn 请求接受时钟倒退按零计时() {
        let accepted_at = Instant::now();
        let requested_at = accepted_at + Duration::from_millis(10);
        let mut coordinator = HistoryPageCoordinator::default();
        coordinator.begin_dataset().expect("建立数据集失败");
        let request = coordinator
            .request_first_page_at(query("clock"), requested_at)
            .expect("创建首页请求失败");

        assert_eq!(
            coordinator.accept_page_at(true, empty_result(&request), &[], accepted_at),
            Some(HistoryPageApplication::Replace(Vec::new()))
        );
        assert_eq!(
            coordinator
                .performance_snapshot()
                .last_request_to_accept_duration,
            Duration::ZERO
        );
    }

    /// 同一 generation 再次成功首页必须替换旧页、续页和累计观测，不能沿用旧指标。
    #[test]
    fn 同代次再次成功首页重置旧分页观测() {
        let started_at = Instant::now();
        let mut coordinator = HistoryPageCoordinator::default();
        let generation = coordinator.begin_dataset().expect("建立数据集失败");
        let first = coordinator
            .request_first_page_at(query("refresh"), started_at)
            .expect("创建首次首页失败");
        let original_cursor = HistoryCursor {
            copied_at: 90,
            id: 2,
        };
        let first_items = vec![text_item(1), image_item(2)];
        assert_eq!(
            coordinator.accept_page_at(
                true,
                successful_result(&first, first_items.clone(), Some(original_cursor)),
                &[],
                started_at + Duration::from_millis(2),
            ),
            Some(HistoryPageApplication::Replace(first_items.clone()))
        );
        let next = coordinator
            .request_next_page_at(query("refresh"), started_at + Duration::from_millis(3))
            .expect("创建续页失败");
        let appended = text_item(3);
        assert_eq!(
            coordinator.accept_page_at(
                true,
                successful_result(&next, vec![first_items[0].clone(), appended.clone()], None,),
                &first_items,
                started_at + Duration::from_millis(7),
            ),
            Some(HistoryPageApplication::Append(vec![appended]))
        );
        assert_eq!(coordinator.performance_snapshot().accepted_pages, 2);
        assert_eq!(coordinator.performance_snapshot().duplicate_items, 1);

        let refreshed = coordinator
            .request_first_page_at(query("refresh"), started_at + Duration::from_millis(10))
            .expect("同代次创建刷新首页失败");
        assert_eq!(refreshed.generation, generation);
        let refreshed_text = text_item(10);
        let refreshed_image = image_item(11);
        assert_eq!(
            coordinator.accept_page_at(
                true,
                successful_result(
                    &refreshed,
                    vec![
                        refreshed_text.clone(),
                        refreshed_image.clone(),
                        refreshed_image.clone(),
                    ],
                    None,
                ),
                &[first_items[0].clone(), first_items[1].clone(), text_item(3)],
                started_at + Duration::from_millis(16),
            ),
            Some(HistoryPageApplication::Replace(vec![
                refreshed_text,
                refreshed_image,
            ]))
        );
        assert_eq!(
            coordinator.performance_snapshot(),
            HistoryPerformanceSnapshot {
                accepted_pages: 1,
                loaded_items: 2,
                text_items: 1,
                image_items: 1,
                duplicate_items: 1,
                last_request_to_accept_duration: Duration::from_millis(6),
                total_request_to_accept_duration: Duration::from_millis(6),
            }
        );
        assert!(!coordinator.retry_required());
        assert_eq!(coordinator.next_cursor(), None);
    }

    /// worker 返回条目数恰好等于签发 limit 时合法，不能把满页误判为协议超限。
    #[test]
    fn 等于签发_limit_的首页合法接受() {
        let started_at = Instant::now();
        let mut coordinator = HistoryPageCoordinator::default();
        coordinator.begin_dataset().expect("建立数据集失败");
        let request = coordinator
            .request_first_page_at(query("exact"), started_at)
            .expect("创建首页失败");
        let items = (1..=u64::from(FIRST_PAGE_LIMIT))
            .map(text_item)
            .collect::<Vec<_>>();

        assert_eq!(
            coordinator.accept_page_at(
                true,
                successful_result(&request, items.clone(), None),
                &[],
                started_at + Duration::from_millis(1),
            ),
            Some(HistoryPageApplication::Replace(items))
        );
        assert_eq!(
            coordinator.performance_snapshot().loaded_items,
            FIRST_PAGE_LIMIT as usize
        );
    }

    /// 容量只剩 20 条时按收缩 limit 校验，返回 21 条必须整页失败且不接受前移游标。
    #[test]
    fn 收缩后的签发_limit_仍执行超限整页拒绝() {
        let started_at = Instant::now();
        let original_cursor = HistoryCursor {
            copied_at: 50,
            id: 1_980,
        };
        let mut coordinator = HistoryPageCoordinator::default();
        coordinator.begin_dataset().expect("建立数据集失败");
        coordinator.pagination.next_cursor = Some(original_cursor);
        coordinator.pagination.loaded_items = 1_980;
        coordinator.pagination.performance.loaded_items = 1_980;
        coordinator.pagination.performance.text_items = 1_980;
        let metrics_before = coordinator.performance_snapshot();
        let request = coordinator
            .request_next_page_at(query("remaining"), started_at)
            .expect("创建收缩续页失败");
        assert_eq!(request.query.limit, 20);
        let oversized = (2_000..=2_020).map(text_item).collect::<Vec<_>>();

        assert_eq!(
            coordinator.accept_page_at(
                true,
                successful_result(
                    &request,
                    oversized,
                    Some(HistoryCursor {
                        copied_at: 1,
                        id: 2_020,
                    }),
                ),
                &[],
                started_at + Duration::from_millis(2),
            ),
            Some(HistoryPageApplication::NextPageFailed)
        );
        assert_eq!(coordinator.next_cursor(), Some(original_cursor));
        assert_eq!(coordinator.performance_snapshot(), metrics_before);
    }

    /// 错误身份和匹配失败结果都不得改变已经成功接受的页数、分类或耗时。
    #[test]
    fn 迟到与失败结果不污染成功性能指标() {
        let started_at = Instant::now();
        let mut coordinator = HistoryPageCoordinator::default();
        coordinator.begin_dataset().expect("建立数据集失败");
        let first = coordinator
            .request_first_page_at(query("stable"), started_at)
            .expect("创建首页失败");
        let cursor = HistoryCursor {
            copied_at: 10,
            id: 1,
        };
        let current = vec![text_item(1)];
        coordinator.accept_page_at(
            true,
            successful_result(&first, current.clone(), Some(cursor)),
            &[],
            started_at + Duration::from_millis(3),
        );
        let metrics_before = coordinator.performance_snapshot();
        let next = coordinator
            .request_next_page_at(query("stable"), started_at + Duration::from_millis(4))
            .expect("创建续页失败");
        let mut stale = successful_result(&next, vec![image_item(2)], None);
        stale.token = HistoryRequestToken(next.token.as_u64().saturating_add(1));
        assert_eq!(
            coordinator.accept_page_at(
                true,
                stale,
                &current,
                started_at + Duration::from_millis(8),
            ),
            None
        );
        assert_eq!(coordinator.performance_snapshot(), metrics_before);
        assert_eq!(
            coordinator.accept_page_at(
                true,
                HistoryPageResult {
                    generation: next.generation,
                    token: next.token,
                    requested_cursor: next.query.cursor,
                    outcome: Err(HistoryQueryFailure::StorageUnavailable),
                },
                &current,
                started_at + Duration::from_millis(9),
            ),
            Some(HistoryPageApplication::NextPageFailed)
        );
        assert_eq!(coordinator.performance_snapshot(), metrics_before);
    }

    /// 累计 request-to-accept 耗时接近 Duration 上限时必须饱和而不是回绕。
    #[test]
    fn 累计请求接受耗时在上限处饱和() {
        let started_at = Instant::now();
        let cursor = HistoryCursor {
            copied_at: 10,
            id: 1,
        };
        let current = vec![text_item(1)];
        let mut coordinator = HistoryPageCoordinator::default();
        coordinator.begin_dataset().expect("建立数据集失败");
        coordinator.pagination.next_cursor = Some(cursor);
        coordinator.pagination.loaded_items = 1;
        coordinator.pagination.performance = HistoryPerformanceSnapshot {
            accepted_pages: 1,
            loaded_items: 1,
            text_items: 1,
            total_request_to_accept_duration: Duration::MAX
                .checked_sub(Duration::from_millis(5))
                .expect("测试上限减法应有效"),
            ..HistoryPerformanceSnapshot::default()
        };
        let request = coordinator
            .request_next_page_at(query("duration"), started_at)
            .expect("创建续页失败");

        assert_eq!(
            coordinator.accept_page_at(
                true,
                successful_result(&request, vec![image_item(2)], None),
                &current,
                started_at + Duration::from_millis(10),
            ),
            Some(HistoryPageApplication::Append(vec![image_item(2)]))
        );
        let metrics = coordinator.performance_snapshot();
        assert_eq!(metrics.accepted_pages, 2);
        assert_eq!(
            metrics.last_request_to_accept_duration,
            Duration::from_millis(10)
        );
        assert_eq!(metrics.total_request_to_accept_duration, Duration::MAX);
        assert_eq!(
            metrics.text_items + metrics.image_items,
            metrics.loaded_items
        );
    }
}

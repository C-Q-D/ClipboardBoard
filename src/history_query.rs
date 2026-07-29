//! 此模块实现 SQLite 历史分页的 UI 协调器、latest-wins 双向邮箱和后台查询线程。
//!
//! UI 只在短互斥区覆盖请求或提取结果；SQLite 同步查询始终在独立 worker 中执行。
//! generation、token 与 requested_cursor 共同构成响应身份，避免迟到首页污染新数据集。

use std::{
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    command::UiClipboardItem,
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
}

/// 当前唯一活动请求的三元身份。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveRequest {
    /// 数据集 generation。
    generation: HistoryDatasetGeneration,
    /// 单次请求 token。
    token: HistoryRequestToken,
    /// 首页为空；WCB-INT-10 将使用复合游标。
    requested_cursor: Option<HistoryCursor>,
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
}

impl Default for HistoryPageCoordinator {
    /// 创建尚未建立数据集的协调器。
    fn default() -> Self {
        Self {
            next_generation: 0,
            next_token: 0,
            current_generation: None,
            active: None,
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
        Ok(generation)
    }

    /// 使当前数据集和活动请求失效；旧结果随后必然被拒绝。
    pub fn invalidate(&mut self) {
        self.current_generation = None;
        self.active = None;
    }

    /// 为当前数据集分配首页请求；强制 cursor=None、limit=30。
    pub fn request_first_page(
        &mut self,
        mut query: HistoryQuery,
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
        mut query: HistoryQuery,
        cursor: Option<HistoryCursor>,
        loaded_count: usize,
    ) -> Result<HistoryPageRequest, HistoryPageCoordinatorError> {
        let generation = self
            .current_generation
            .ok_or(HistoryPageCoordinatorError::NoActiveDataset)?;
        if self.active.is_some() {
            return Err(HistoryPageCoordinatorError::RequestAlreadyActive);
        }
        let cursor = cursor.ok_or(HistoryPageCoordinatorError::DatasetExhausted)?;
        let remaining = MAX_LOADED_ITEMS.saturating_sub(loaded_count);
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
        });
        Ok(HistoryPageRequest {
            generation,
            token,
            query,
        })
    }

    /// 接受当前可见数据集的一次精确身份响应；成功和失败都会消费活动 token。
    pub fn accept_page(&mut self, visible: bool, result: &HistoryPageResult) -> bool {
        let identity = ActiveRequest {
            generation: result.generation,
            token: result.token,
            requested_cursor: result.requested_cursor,
        };
        if visible
            && self.current_generation == Some(result.generation)
            && self.active == Some(identity)
        {
            self.active = None;
            true
        } else {
            false
        }
    }

    /// 当请求无法提交到 worker 时，按原三元身份消费活动 token。
    pub fn fail_submission(&mut self, visible: bool, request: &HistoryPageRequest) -> bool {
        let identity = ActiveRequest {
            generation: request.generation,
            token: request.token,
            requested_cursor: request.query.cursor,
        };
        if visible
            && self.current_generation == Some(request.generation)
            && self.active == Some(identity)
        {
            self.active = None;
            true
        } else {
            false
        }
    }

    /// 返回是否存在尚未结束的请求，供底部边沿状态机抑制重复派发。
    pub const fn has_active_request(&self) -> bool {
        self.active.is_some()
    }

    /// 返回当前数据集 generation，供确定性测试和捕获刷新门禁使用。
    pub const fn current_generation(&self) -> Option<HistoryDatasetGeneration> {
        self.current_generation
    }
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
    if summary.item_type != "text" {
        return Err(HistoryQueryFailure::InvalidSummary);
    }
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
    use crate::storage::{StorageExecutor, TextUpsertInput};
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
        assert!(!coordinator.accept_page(true, &wrong));
        wrong = empty_result(&request);
        wrong.requested_cursor = Some(HistoryCursor {
            copied_at: 1,
            id: 1,
        });
        assert!(!coordinator.accept_page(true, &wrong));
        let result = empty_result(&request);
        assert!(coordinator.accept_page(true, &result));
        assert!(!coordinator.accept_page(true, &result));
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
            let request = coordinator
                .request_next_page(query("page"), Some(cursor), loaded)
                .unwrap();
            assert_eq!(request.query.cursor, Some(cursor));
            assert_eq!(request.query.limit, expected);
            assert_eq!(
                coordinator.request_next_page(query("duplicate"), Some(cursor), loaded),
                Err(HistoryPageCoordinatorError::RequestAlreadyActive)
            );
        }

        for loaded in [2_000, 2_001] {
            let mut coordinator = HistoryPageCoordinator::default();
            coordinator.begin_dataset().unwrap();
            assert_eq!(
                coordinator.request_next_page(query("full"), Some(cursor), loaded),
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
        let request = coordinator
            .request_next_page(query("page"), Some(cursor), 30)
            .unwrap();
        let mut wrong = empty_result(&request);
        wrong.requested_cursor = Some(HistoryCursor {
            copied_at: 99,
            id: 9,
        });
        assert!(!coordinator.accept_page(true, &wrong));
        assert!(coordinator.fail_submission(true, &request));
        assert!(!coordinator.fail_submission(true, &request));
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
        };

        assert_eq!(
            ui_item_from_summary(&summary, 1),
            Err(HistoryQueryFailure::InvalidSummary)
        );
    }
}

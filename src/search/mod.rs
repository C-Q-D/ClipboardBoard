//! 此模块提供搜索请求的防抖和结果代次协调，不执行 SQLite 查询或触碰 UI 对象。
//!
//! `SearchCoordinator` 由未来 UI 线程独占；它只保存最新的 `HistoryQuery`，在截止时间后
//! 派发一个请求，并以 generation/in-flight 门禁拒绝迟到、取消后或重复的结果。

use std::time::{Duration, Instant};

use crate::storage::HistoryQuery;

/// 产品默认搜索防抖窗口；真实 UI 接线由 ATOM-25 负责。
pub const DEFAULT_SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);

/// 搜索请求的单调代次标识；结果只能回写与当前代次完全一致的请求。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SearchGeneration(u64);

impl SearchGeneration {
    /// 返回可记录或传输的数值代次；零永远不作为有效请求代次。
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// 防抖截止后交给查询 worker 的拥有型请求；不携带正文或 UI 引用。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchRequest {
    /// 该请求对应的单调代次。
    pub generation: SearchGeneration,
    /// ATOM-23 定义的安全筛选和复合游标参数。
    pub query: HistoryQuery,
}

/// 搜索协调器无法再分配新代次时返回的错误；避免回绕后旧结果误匹配。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchCoordinatorError {
    /// 代次已达到 `u64::MAX`，继续提交会破坏单调性，因此拒绝请求。
    GenerationExhausted,
}

/// 尚未到截止时间的最新请求；截止时间只由 UI 线程使用，不暴露给 worker。
struct PendingSearch {
    /// 到期后应派发的拥有型请求。
    request: SearchRequest,
    /// 防抖截止时间；测试通过注入 `Instant` 推进，不真实等待。
    deadline: Instant,
}

/// UI 线程拥有的搜索防抖与结果代次状态机。
pub struct SearchCoordinator {
    /// 当前防抖窗口；生产默认 120 ms，测试可注入更短或零窗口。
    debounce: Duration,
    /// 最近分配的数值代次；从零开始，首次提交分配 1。
    next_generation: u64,
    /// 当前最新提交的代次；用于屏蔽旧结果。
    latest_generation: Option<SearchGeneration>,
    /// 尚未到期的最新请求；新输入会覆盖它。
    pending: Option<PendingSearch>,
    /// 已派发但尚未应用结果的代次；新输入或取消会清除它。
    in_flight: Option<SearchGeneration>,
}

impl Default for SearchCoordinator {
    /// 使用产品固定的 120 ms 防抖窗口创建协调器。
    fn default() -> Self {
        Self::new()
    }
}

impl SearchCoordinator {
    /// 创建使用默认 120 ms 防抖窗口的协调器。
    pub fn new() -> Self {
        Self::with_debounce(DEFAULT_SEARCH_DEBOUNCE)
    }

    /// 创建可注入防抖窗口的协调器；仅影响截止时间，不改变代次和结果门禁。
    pub fn with_debounce(debounce: Duration) -> Self {
        Self {
            debounce,
            next_generation: 0,
            latest_generation: None,
            pending: None,
            in_flight: None,
        }
    }

    /// 返回当前防抖窗口，便于 UI 或测试显示明确配置。
    pub const fn debounce(&self) -> Duration {
        self.debounce
    }

    /// 提交一条新查询并替换尚未派发的旧查询；每次成功提交都会分配新代次。
    pub fn submit(
        &mut self,
        query: HistoryQuery,
        now: Instant,
    ) -> Result<SearchGeneration, SearchCoordinatorError> {
        let next = self
            .next_generation
            .checked_add(1)
            .ok_or(SearchCoordinatorError::GenerationExhausted)?;
        let generation = SearchGeneration(next);
        let deadline = now.checked_add(self.debounce).unwrap_or(now);

        self.next_generation = next;
        self.latest_generation = Some(generation);
        self.in_flight = None;
        self.pending = Some(PendingSearch {
            request: SearchRequest { generation, query },
            deadline,
        });
        Ok(generation)
    }

    /// 在指定时间点取出已到期的最新请求；未到期或没有待处理请求时返回 `None`。
    pub fn poll(&mut self, now: Instant) -> Option<SearchRequest> {
        let is_ready = self
            .pending
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline);
        if !is_ready {
            return None;
        }

        let pending = self.pending.take()?;
        self.in_flight = Some(pending.request.generation);
        Some(pending.request)
    }

    /// 接受当前 in-flight 代次的第一个结果；旧代次、取消后结果和重复结果均返回 `false`。
    pub fn accept_result(&mut self, generation: SearchGeneration) -> bool {
        if self.latest_generation == Some(generation) && self.in_flight == Some(generation) {
            self.in_flight = None;
            true
        } else {
            false
        }
    }

    /// 取消待处理和进行中的查询；清除 in-flight 即可使所有迟到结果失效。
    pub fn cancel(&mut self) {
        self.pending = None;
        self.in_flight = None;
    }

    /// 返回最新提交代次；尚未提交任何查询时返回 `None`。
    pub const fn latest_generation(&self) -> Option<SearchGeneration> {
        self.latest_generation
    }

    /// 返回当前是否存在尚未到期的请求。
    pub const fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// 返回当前是否存在等待结果应用的请求。
    pub const fn has_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证防抖截止、代次递增、取消和迟到结果隔离，不真实等待系统时钟。

    use super::SearchCoordinator;
    use crate::storage::HistoryQuery;
    use std::time::{Duration, Instant};

    /// 生成带关键词的最小查询，测试只观察请求身份而不访问 SQLite。
    fn query(keyword: &str) -> HistoryQuery {
        HistoryQuery {
            keyword: Some(keyword.to_owned()),
            limit: 30,
            ..HistoryQuery::default()
        }
    }

    /// 三次快速输入只能在截止时间后派发最后一次查询。
    #[test]
    fn 快速输入只派发最新请求() {
        let start = Instant::now();
        let mut coordinator = SearchCoordinator::new();
        let first = coordinator
            .submit(query("a"), start)
            .expect("首次搜索代次分配失败");
        let second = coordinator
            .submit(query("ab"), start + Duration::from_millis(30))
            .expect("第二次搜索代次分配失败");
        let third = coordinator
            .submit(query("abc"), start + Duration::from_millis(60))
            .expect("第三次搜索代次分配失败");

        assert!(first.as_u64() < second.as_u64());
        assert!(second.as_u64() < third.as_u64());
        assert!(coordinator
            .poll(start + Duration::from_millis(179))
            .is_none());

        let request = coordinator
            .poll(start + Duration::from_millis(180))
            .expect("截止时间应派发最后请求");
        assert_eq!(request.generation, third);
        assert_eq!(request.query.keyword.as_deref(), Some("abc"));
        assert!(!coordinator.has_pending());
        assert!(coordinator.has_in_flight());
    }

    /// 120 ms 截止点本身必须可派发，避免额外延迟一个事件循环周期。
    #[test]
    fn 截止点恰好派发() {
        let start = Instant::now();
        let mut coordinator = SearchCoordinator::new();
        let generation = coordinator
            .submit(query("exact"), start)
            .expect("搜索代次分配失败");

        assert!(coordinator
            .poll(start + Duration::from_millis(119))
            .is_none());
        assert_eq!(
            coordinator
                .poll(start + Duration::from_millis(120))
                .expect("120 ms 截止点未派发")
                .generation,
            generation
        );
    }

    /// 已派发请求被新输入替换后，旧结果不得污染新 generation。
    #[test]
    fn 新输入使旧结果失效且新结果只接受一次() {
        let start = Instant::now();
        let mut coordinator = SearchCoordinator::new();
        let old_generation = coordinator
            .submit(query("old"), start)
            .expect("旧查询代次分配失败");
        let old_request = coordinator
            .poll(start + Duration::from_millis(120))
            .expect("旧查询未派发");
        assert_eq!(old_request.generation, old_generation);

        let new_generation = coordinator
            .submit(query("new"), start + Duration::from_millis(121))
            .expect("新查询代次分配失败");
        assert!(!coordinator.accept_result(old_generation));
        assert!(coordinator
            .poll(start + Duration::from_millis(241))
            .is_some());
        assert!(coordinator.accept_result(new_generation));
        assert!(!coordinator.accept_result(new_generation));
    }

    /// 取消或关闭语义必须同时丢弃 pending 和 in-flight 请求。
    #[test]
    fn 取消后迟到结果被拒绝() {
        let start = Instant::now();
        let mut coordinator = SearchCoordinator::with_debounce(Duration::ZERO);
        let generation = coordinator
            .submit(query("cancel"), start)
            .expect("取消查询代次分配失败");
        assert!(coordinator.poll(start).is_some());
        coordinator.cancel();
        assert!(!coordinator.has_pending());
        assert!(!coordinator.has_in_flight());
        assert!(!coordinator.accept_result(generation));
        assert_eq!(coordinator.latest_generation(), Some(generation));
    }

    /// 代次从 1 开始且在可用范围内严格递增，避免零值与旧事件偶然相等。
    #[test]
    fn 代次从一开始并单调递增() {
        let start = Instant::now();
        let mut coordinator = SearchCoordinator::with_debounce(Duration::ZERO);
        let first = coordinator
            .submit(query("one"), start)
            .expect("首次代次分配失败");
        let second = coordinator
            .submit(query("two"), start)
            .expect("第二次代次分配失败");
        assert_eq!(first.as_u64(), 1);
        assert!(first.as_u64() < second.as_u64());
    }

    /// 零窗口测试接缝允许在同一时间点立即派发，证明测试不依赖真实 sleep。
    #[test]
    fn 零窗口立即派发() {
        let now = Instant::now();
        let mut coordinator = SearchCoordinator::with_debounce(Duration::ZERO);
        coordinator
            .submit(query("instant"), now)
            .expect("零窗口代次分配失败");
        assert!(coordinator.poll(now).is_some());
    }
}

//! 此模块建立专用 ClipboardIO worker 线程和容量为一的 latest-wins 请求队列。
//!
//! 消息线程只提交剪贴板 sequence 与来源快照；worker 是当前唯一允许调用 Win32 剪贴板
//! 读取 API 的线程。队列在锁内替换尚未开始的旧请求，快速复制时不会阻塞消息泵或无界
//! 堆积；请求响应通过拥有型 DTO 返回，后续业务层可以继续在 UI 线程外处理正文。

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use super::reader::{
    read_text_with_backend, ClipboardReadError, RetryPolicy, Win32ClipboardBackend,
};
use super::writer::{ClipboardWriteExpectationStore, ClipboardWriteFormat};
use crate::domain::ClipboardPayload;
use crate::platform::windows::ProcessSource;

/// worker 请求的有限错误集合，不携带线程 panic 文本或外部字符串。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardWorkerError {
    /// worker 线程无法创建。
    ThreadStart,
    /// 保留给旧调用方的队列错误；latest-wins 队列不会返回该分支。
    QueueFull,
    /// worker 已停止或响应通道已断开。
    Disconnected,
    /// worker 线程退出时发生 panic；调用方只能重新创建 worker。
    ThreadPanicked,
}

/// 消息线程提交给 worker 的一次剪贴板捕获请求。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardCaptureRequest {
    /// `WM_CLIPBOARDUPDATE` 到达时观察到的剪贴板序号。
    pub sequence: u32,
    /// 消息线程同步捕获的来源进程快照；失败时为空，不阻塞正文读取。
    pub source: Option<ProcessSource>,
}

impl ClipboardCaptureRequest {
    /// 创建带来源快照的捕获请求；正文不会在消息线程读取或复制。
    pub fn new(sequence: u32, source: Option<ProcessSource>) -> Self {
        Self { sequence, source }
    }
}

/// worker 成功完成一次捕获后返回的拥有型结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardCaptureResult {
    /// 与本次读取绑定的剪贴板序号，供后续历史协调器建立幂等键。
    pub sequence: u32,
    /// 与该序号同时捕获的来源快照；不会从 worker 重新查询前台窗口。
    pub source: Option<ProcessSource>,
    /// 已脱离 HGLOBAL 生命周期的正文 payload。
    pub payload: ClipboardPayload,
}

/// UI 请求只复制某条历史时提交的轻量命令；正文仍必须从存储线程按 ID 读取。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardCopyRequest {
    /// 目标历史记录的数据库 ID。
    pub id: u64,
    /// UI 当前卡片看到的哈希，用于拒绝旧选择误写入新记录正文。
    pub content_hash: [u8; 32],
}

impl ClipboardCopyRequest {
    /// 创建带一致性哈希的仅复制请求，不携带完整剪贴板正文。
    pub fn new(id: u64, content_hash: [u8; 32]) -> Self {
        Self { id, content_hash }
    }
}

/// 结果泵需要消费的捕获和仅复制工作；两者共享一个有界唤醒通道。
#[derive(Debug)]
pub enum ClipboardWorkItem {
    /// ClipboardIO worker 发布的捕获结果。
    Capture(Result<ClipboardCaptureResult, ClipboardReadError>),
    /// UI 投递的按 ID 仅复制请求。
    Copy(ClipboardCopyRequest),
}

/// 从消息线程移交捕获结果的容量为一 latest-wins 桥。
///
/// 生产消费者可以在 UI 或历史协调器线程调用 `try_take`，不会访问消息线程的 HWND、
/// Receiver 或 TLS；新结果会替换尚未消费的旧结果，保证复制高峰不会形成无界正文缓存。
#[derive(Clone)]
pub struct ClipboardCaptureInbox {
    /// 结果状态由共享互斥锁保护，以便 worker 和消费者跨线程安全交接拥有型数据。
    state: Arc<Mutex<CaptureInboxState>>,
    /// 新结果或关闭动作唤醒阻塞消费者。
    wake: Arc<Condvar>,
}

/// 捕获结果桥的内部状态。
struct CaptureInboxState {
    /// 尚未消费的唯一最新结果；成功和 sequence 失配错误都占用同一槽位。
    pending: Option<Result<ClipboardCaptureResult, ClipboardReadError>>,
    /// 尚未处理的最新仅复制请求；快速重复按键只保留最后一次选择。
    pending_copy: Option<ClipboardCopyRequest>,
    /// 关闭闩锁；worker 停止后消费者不再等待。
    closed: bool,
}

impl ClipboardCaptureInbox {
    /// 创建开放的空结果桥。
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(CaptureInboxState {
                pending: None,
                pending_copy: None,
                closed: false,
            })),
            wake: Arc::new(Condvar::new()),
        }
    }

    /// 非阻塞取出最新结果；没有结果、桥已关闭或锁已失效时返回 `None`。
    pub fn try_take(&self) -> Option<Result<ClipboardCaptureResult, ClipboardReadError>> {
        self.state.lock().ok()?.pending.take()
    }

    /// 阻塞等待一个结果；桥关闭且没有待处理结果时返回 `None`。
    pub fn wait_take(&self) -> Option<Result<ClipboardCaptureResult, ClipboardReadError>> {
        loop {
            match self.wait_take_work()? {
                ClipboardWorkItem::Capture(result) => return Some(result),
                ClipboardWorkItem::Copy(_) => {
                    // 兼容旧 API 的调用方不消费复制命令；生产结果泵使用 wait_take_work。
                }
            }
        }
    }

    /// 投递最新仅复制请求；队列关闭后立即拒绝，UI 线程不等待数据库或系统剪贴板。
    pub fn request_copy(&self, request: ClipboardCopyRequest) -> Result<(), ClipboardWorkerError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ClipboardWorkerError::Disconnected)?;
        if state.closed {
            return Err(ClipboardWorkerError::Disconnected);
        }
        state.pending_copy = Some(request);
        self.wake.notify_one();
        Ok(())
    }

    /// 阻塞取得最新捕获或写回命令；最新复制动作优先，避免用户操作被捕获高峰饿死。
    pub fn wait_take_work(&self) -> Option<ClipboardWorkItem> {
        let mut state = self.state.lock().ok()?;
        loop {
            if let Some(request) = state.pending_copy.take() {
                return Some(ClipboardWorkItem::Copy(request));
            }
            if let Some(result) = state.pending.take() {
                return Some(ClipboardWorkItem::Capture(result));
            }
            if state.closed {
                return None;
            }
            state = self.wake.wait(state).ok()?;
        }
    }

    /// 标记结果桥关闭；保留已经发布的捕获结果，但丢弃尚未执行的复制命令。
    ///
    /// 该方法只由 worker 在 `join` 完成后调用；这样在途读取仍有机会发布最终结果，
    /// 关闭不会把已经交接给桥的正文静默丢弃，也不会在 UI 已退出后启动新的 Win32 写回。
    pub(crate) fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.pending_copy = None;
            state.closed = true;
            self.wake.notify_all();
        }
    }

    /// 从 worker 发布最新结果；桥关闭或锁中毒时安全丢弃，不阻塞剪贴板读取线程。
    fn publish(&self, result: Result<ClipboardCaptureResult, ClipboardReadError>) {
        if let Ok(mut state) = self.state.lock() {
            if state.closed {
                return;
            }
            state.pending = Some(result);
            self.wake.notify_one();
        }
    }
}

impl Default for ClipboardCaptureInbox {
    /// 默认创建开放的空结果桥，便于应用启动时直接建立跨线程接缝。
    fn default() -> Self {
        Self::new()
    }
}

/// 专用 ClipboardIO worker；生产代码通过 `request_capture` 获取带来源的异步结果。
pub struct ClipboardIoWorker {
    /// 共享 latest-wins 队列；Option 便于 stop 时先关闭队列再 join。
    queue: Option<Arc<LatestRequestQueue>>,
    /// worker 线程句柄，确保生命周期可控且退出时不遗留后台线程。
    join_handle: Option<JoinHandle<()>>,
    /// 结果桥的共享句柄；worker 成功或 sequence 失配后向此处发布最新结果。
    inbox: ClipboardCaptureInbox,
}

/// 单次读取请求的内部响应类型；旧 payload API 与新捕获 API 共用同一个队列。
enum ReadResponse {
    /// 兼容 ATOM-10 的无来源正文响应。
    Payload(mpsc::Sender<Result<ClipboardPayload, ClipboardReadError>>),
    /// ATOM-11 使用的带 sequence/来源关联响应。
    Capture {
        /// 捕获结果响应通道。
        response: mpsc::Sender<Result<ClipboardCaptureResult, ClipboardReadError>>,
        /// 与读取前后 sequence 复核绑定的提交序号。
        sequence: u32,
        /// 消息线程在同一更新消息中捕获的来源快照。
        source: Option<ProcessSource>,
    },
}

/// 进入 latest-wins 队列的请求；响应发送端随旧请求替换而断开，通知调用方结果已丢弃。
struct ReadRequest {
    /// worker 读取前必须匹配的序号；旧 API 可为空以建立读取前基线。
    expected_sequence: Option<u32>,
    /// worker 完成读取后发送拥有型结果。
    response: ReadResponse,
}

/// 容量为一且可替换待处理项的同步队列。
struct LatestRequestQueue {
    /// 队列状态由互斥锁保护，确保替换与关闭是原子操作。
    state: Mutex<LatestQueueState>,
    /// 新请求或关闭动作唤醒 worker，避免轮询消耗 CPU。
    wake: Condvar,
}

/// latest-wins 队列的内部状态。
struct LatestQueueState {
    /// 尚未被 worker 取走的唯一请求；新请求会覆盖它。
    pending: Option<ReadRequest>,
    /// 关闭闩锁；置位后不再接受请求，worker 在清空当前工作后退出。
    closed: bool,
}

impl LatestRequestQueue {
    /// 创建空的开放队列。
    fn new() -> Self {
        Self {
            state: Mutex::new(LatestQueueState {
                pending: None,
                closed: false,
            }),
            wake: Condvar::new(),
        }
    }

    /// 写入最新请求；旧的尚未取走请求会被丢弃且不会让调用线程等待。
    fn push_latest(&self, request: ReadRequest) -> Result<(), ClipboardWorkerError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ClipboardWorkerError::Disconnected)?;
        if state.closed {
            return Err(ClipboardWorkerError::Disconnected);
        }

        // 替换会立即释放旧响应发送端，接收方得到断开信号，从而知道结果不再有效。
        state.pending = Some(request);
        self.wake.notify_one();
        Ok(())
    }

    /// 阻塞等待最新请求；关闭后丢弃尚未开始的请求并返回 None。
    fn pop(&self) -> Option<ReadRequest> {
        let mut state = self.state.lock().ok()?;
        loop {
            if let Some(request) = state.pending.take() {
                return Some(request);
            }
            if state.closed {
                return None;
            }
            state = self.wake.wait(state).ok()?;
        }
    }

    /// 关闭队列并丢弃尚未开始的请求；正在 worker 内执行的读取由 worker 自行完成。
    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.pending.take();
            self.wake.notify_all();
        }
    }
}

impl ClipboardIoWorker {
    /// 创建并启动 ClipboardIO worker 线程；创建失败返回有限错误而不 panic。
    pub fn start() -> Result<Self, ClipboardWorkerError> {
        Self::start_with_inbox(ClipboardCaptureInbox::new())
    }

    /// 使用调用方提供的结果桥启动 worker，便于消息线程和后续协调器共享同一接缝。
    pub fn start_with_inbox(inbox: ClipboardCaptureInbox) -> Result<Self, ClipboardWorkerError> {
        Self::start_with_inbox_and_expectations(inbox, ClipboardWriteExpectationStore::new())
    }

    /// 使用调用方提供的写回预期启动 worker；自身写回事件会在发布历史前一次性消费。
    pub fn start_with_inbox_and_expectations(
        inbox: ClipboardCaptureInbox,
        expectations: ClipboardWriteExpectationStore,
    ) -> Result<Self, ClipboardWorkerError> {
        let queue = Arc::new(LatestRequestQueue::new());
        let worker_queue = Arc::clone(&queue);
        let worker_inbox = inbox.clone();
        let worker_expectations = expectations;
        let join_handle = thread::Builder::new()
            .name("ClipboardIoWorker".to_owned())
            .spawn(move || worker_loop(worker_queue, worker_inbox, worker_expectations))
            .map_err(|_| ClipboardWorkerError::ThreadStart)?;

        Ok(Self {
            queue: Some(queue),
            join_handle: Some(join_handle),
            inbox,
        })
    }

    /// 返回可跨线程消费的结果桥副本；调用方不会取得 worker 或消息线程所有权。
    pub fn inbox(&self) -> ClipboardCaptureInbox {
        self.inbox.clone()
    }

    /// 提交兼容 ATOM-10 的文本读取；请求仍采用 latest-wins，不在调用线程等待剪贴板。
    pub fn request(
        &self,
        expected_sequence: Option<u32>,
    ) -> Result<Receiver<Result<ClipboardPayload, ClipboardReadError>>, ClipboardWorkerError> {
        let (response_sender, response_receiver) = mpsc::channel();
        self.enqueue(ReadRequest {
            expected_sequence,
            response: ReadResponse::Payload(response_sender),
        })?;
        Ok(response_receiver)
    }

    /// 提交带 sequence/来源快照的捕获请求；新请求会替换尚未开始的旧请求。
    pub fn request_capture(
        &self,
        capture: ClipboardCaptureRequest,
    ) -> Result<Receiver<Result<ClipboardCaptureResult, ClipboardReadError>>, ClipboardWorkerError>
    {
        let (response_sender, response_receiver) = mpsc::channel();
        self.enqueue(ReadRequest {
            expected_sequence: Some(capture.sequence),
            response: ReadResponse::Capture {
                response: response_sender,
                sequence: capture.sequence,
                source: capture.source,
            },
        })?;
        Ok(response_receiver)
    }

    /// 将内部请求交给 latest-wins 队列；队列关闭时立即返回，不阻塞消息线程。
    fn enqueue(&self, request: ReadRequest) -> Result<(), ClipboardWorkerError> {
        self.queue
            .as_ref()
            .ok_or(ClipboardWorkerError::Disconnected)?
            .push_latest(request)
    }

    /// 关闭请求队列并等待 worker 退出；打开重试仍受 200ms 总预算约束。
    pub fn stop(mut self) -> Result<(), ClipboardWorkerError> {
        if let Some(queue) = self.queue.take() {
            queue.close();
        }
        let join_handle = self.join_handle.take();
        let join_result = join_handle
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| ClipboardWorkerError::ThreadPanicked)
            })
            .unwrap_or(Ok(()));
        // 只有 worker 已 join 后才关闭 inbox，确保在途读取能够先发布最终结果。
        self.inbox.close();
        join_result
    }
}

impl Drop for ClipboardIoWorker {
    /// 丢弃 worker 时也关闭队列并尽力回收线程，避免测试或异常路径遗留后台线程。
    fn drop(&mut self) {
        if let Some(queue) = self.queue.take() {
            queue.close();
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
        // 与 stop 保持相同的顺序，异常展开也不能在 worker 完成读取前关闭结果桥。
        self.inbox.close();
    }
}

/// worker 主循环；每个请求创建短生命周期 Win32 backend，避免句柄跨请求复用。
fn worker_loop(
    queue: Arc<LatestRequestQueue>,
    inbox: ClipboardCaptureInbox,
    expectations: ClipboardWriteExpectationStore,
) {
    while let Some(request) = queue.pop() {
        let mut backend = Win32ClipboardBackend;
        let result = read_text_with_backend(
            &mut backend,
            request.expected_sequence,
            RetryPolicy::default(),
        );
        match request.response {
            ReadResponse::Payload(response) => {
                let _ = response.send(result);
            }
            ReadResponse::Capture {
                response,
                sequence,
                source,
            } => {
                let capture_result = result.map(|payload| ClipboardCaptureResult {
                    sequence,
                    source,
                    payload,
                });
                let publish_capture = should_publish_capture(&capture_result, &expectations);
                // 先发布到公共桥，再尝试响应直连调用方；即使直连 Receiver 已被丢弃，
                // 后续历史协调器仍能观察到同一个最新结果或 sequence 失配错误。
                if publish_capture {
                    inbox.publish(capture_result.clone());
                }
                let _ = response.send(capture_result);
            }
        }
    }
}

/// 仅在捕获结果不是已登记的自身 Unicode 写回时发布到历史桥，并保证预期只消费一次。
fn should_publish_capture(
    capture_result: &Result<ClipboardCaptureResult, ClipboardReadError>,
    expectations: &ClipboardWriteExpectationStore,
) -> bool {
    !capture_result.as_ref().is_ok_and(|capture| {
        expectations.consume_if_matches(
            capture.sequence,
            capture.payload.summary().content_hash,
            ClipboardWriteFormat::UnicodeText,
        )
    })
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证 worker 的 latest-wins 队列、异步响应和停止回收协议。

    use super::{
        should_publish_capture, ClipboardCaptureInbox, ClipboardCaptureRequest,
        ClipboardCaptureResult, ClipboardCopyRequest, ClipboardIoWorker, ClipboardWorkItem,
        ClipboardWorkerError, LatestRequestQueue, ReadRequest, ReadResponse,
    };
    use crate::clipboard::writer::{ClipboardWriteExpectationStore, ClipboardWriteFormat};
    use crate::domain::ClipboardPayload;
    use std::sync::mpsc;
    use std::time::Duration;

    /// 新 worker 可以提交请求；当前桌面无剪贴板时结果仍必须有限返回而不挂起调用方。
    #[test]
    fn worker_可以异步接收请求() {
        let worker = ClipboardIoWorker::start().expect("worker 线程应能启动");
        let response = worker.request(None).expect("请求应进入容量为一的队列");
        let _ = response
            .recv_timeout(Duration::from_secs(1))
            .expect("worker 应在有限时间内响应");
        worker.stop().expect("worker 应能正常停止");
    }

    /// 第二个尚未被 worker 取走的请求必须替换第一个，不得等待或形成无界队列。
    #[test]
    fn latest_wins_队列替换旧请求() {
        let queue = LatestRequestQueue::new();
        let (first_sender, first_receiver) = mpsc::channel();
        let (second_sender, _second_receiver) = mpsc::channel();
        queue
            .push_latest(ReadRequest {
                expected_sequence: Some(1),
                response: ReadResponse::Payload(first_sender),
            })
            .expect("首个请求应进入队列");
        queue
            .push_latest(ReadRequest {
                expected_sequence: Some(2),
                response: ReadResponse::Payload(second_sender),
            })
            .expect("新请求应替换旧请求");

        let request = queue.pop().expect("队列应保留最新请求");
        assert_eq!(request.expected_sequence, Some(2));
        assert!(first_receiver.try_recv().is_err());
    }

    /// 快速复制 A、B 时，队列中保留的序号和来源必须属于同一个最新请求。
    #[test]
    fn latest_wins_保持序号与来源绑定() {
        let queue = LatestRequestQueue::new();
        let (first_sender, _first_receiver) = mpsc::channel();
        let (second_sender, _second_receiver) = mpsc::channel();
        queue
            .push_latest(ReadRequest {
                expected_sequence: Some(10),
                response: ReadResponse::Capture {
                    response: first_sender,
                    sequence: 10,
                    source: Some(crate::platform::windows::ProcessSource {
                        executable: "a.exe".to_owned(),
                        display_name: "a".to_owned(),
                        process_id: 10,
                    }),
                },
            })
            .expect("A 请求应进入队列");
        queue
            .push_latest(ReadRequest {
                expected_sequence: Some(11),
                response: ReadResponse::Capture {
                    response: second_sender,
                    sequence: 11,
                    source: Some(crate::platform::windows::ProcessSource {
                        executable: "b.exe".to_owned(),
                        display_name: "b".to_owned(),
                        process_id: 11,
                    }),
                },
            })
            .expect("B 请求应替换 A");

        let request = queue.pop().expect("队列应保留 B 请求");
        match request.response {
            ReadResponse::Capture {
                sequence, source, ..
            } => {
                assert_eq!(sequence, 11);
                assert_eq!(source.expect("B 应有来源").executable, "b.exe");
            }
            ReadResponse::Payload(_) => panic!("捕获请求不能降级为无来源响应"),
        }
    }

    /// 结果桥只能保留最新一项，证明未消费的旧正文不会在高峰期间无限增长。
    #[test]
    fn 结果桥采用_latest_wins() {
        let inbox = ClipboardCaptureInbox::new();
        inbox.publish(Ok(ClipboardCaptureResult {
            sequence: 20,
            source: None,
            payload: ClipboardPayload::from_text("A"),
        }));
        inbox.publish(Ok(ClipboardCaptureResult {
            sequence: 21,
            source: None,
            payload: ClipboardPayload::from_text("B"),
        }));

        let result = inbox
            .try_take()
            .expect("结果桥应有最新结果")
            .expect("B 应成功");
        assert_eq!(result.sequence, 21);
        assert_eq!(result.payload.as_text(), "B");
        assert!(inbox.try_take().is_none());
    }

    /// worker 关闭结果桥后，已经发布的最后结果仍应可被消费者取走，不能静默丢失正文。
    #[test]
    fn 结果桥关闭后保留已发布结果() {
        let inbox = ClipboardCaptureInbox::new();
        inbox.publish(Ok(ClipboardCaptureResult {
            sequence: 30,
            source: None,
            payload: ClipboardPayload::from_text("保留到关闭后"),
        }));
        inbox.close();

        let result = inbox
            .try_take()
            .expect("关闭不应清除已发布结果")
            .expect("结果应保持成功");
        assert_eq!(result.sequence, 30);
        assert!(inbox.wait_take().is_none());
    }

    /// 结果桥关闭时必须丢弃尚未执行的复制命令，避免停止阶段重新写入系统剪贴板。
    #[test]
    fn 关闭时丢弃待处理复制命令() {
        let inbox = ClipboardCaptureInbox::new();
        inbox
            .request_copy(ClipboardCopyRequest::new(88, [8; 32]))
            .expect("开放桥应接受复制命令");
        inbox.close();
        assert!(inbox.wait_take_work().is_none());
    }

    /// 捕获请求必须把序号和来源快照绑定到同一个响应通道，避免快速复制时错配来源。
    #[test]
    fn 捕获请求保存序号和来源快照() {
        let inbox = ClipboardCaptureInbox::new();
        let worker =
            ClipboardIoWorker::start_with_inbox(inbox.clone()).expect("worker 线程应能启动");
        let response = worker
            .request_capture(ClipboardCaptureRequest::new(0, None))
            .expect("捕获请求应进入队列");
        let response_result = response
            .recv_timeout(Duration::from_secs(1))
            .expect("捕获 worker 应在有限时间内返回");
        let inbox_result = inbox.try_take().expect("worker 结果应离开消息线程");
        assert_eq!(inbox_result, response_result);
        worker.stop().expect("worker 应能正常停止");
    }

    /// 队列关闭后不得继续接受新事件，确保消息线程退出时不会重新启动读取。
    #[test]
    fn 关闭后拒绝新请求() {
        let queue = LatestRequestQueue::new();
        queue.close();
        let (sender, _receiver) = mpsc::channel();
        let result = queue.push_latest(ReadRequest {
            expected_sequence: None,
            response: ReadResponse::Payload(sender),
        });
        assert_eq!(result, Err(ClipboardWorkerError::Disconnected));
    }

    /// 仅复制命令必须和捕获结果共用有界唤醒桥，并保留最新选择的 ID 与哈希。
    #[test]
    fn 仅复制请求进入工作桥() {
        let inbox = ClipboardCaptureInbox::new();
        inbox
            .request_copy(ClipboardCopyRequest::new(9, [4; 32]))
            .expect("开放桥应接受仅复制请求");

        match inbox.wait_take_work().expect("应取得复制命令") {
            ClipboardWorkItem::Copy(request) => {
                assert_eq!(request.id, 9);
                assert_eq!(request.content_hash, [4; 32]);
            }
            ClipboardWorkItem::Capture(_) => panic!("复制命令不能伪装为捕获结果"),
        }
    }

    /// 多次尚未消费的复制请求只保留最后一项，避免快速点击形成无界写回积压。
    #[test]
    fn 仅复制请求采用_latest_wins() {
        let inbox = ClipboardCaptureInbox::new();
        inbox
            .request_copy(ClipboardCopyRequest::new(1, [1; 32]))
            .expect("首个复制请求应进入工作桥");
        inbox
            .request_copy(ClipboardCopyRequest::new(2, [2; 32]))
            .expect("新复制请求应替换旧请求");

        match inbox.wait_take_work().expect("应取得最新复制命令") {
            ClipboardWorkItem::Copy(request) => {
                assert_eq!(request.id, 2);
                assert_eq!(request.content_hash, [2; 32]);
            }
            ClipboardWorkItem::Capture(_) => panic!("最新复制命令不能伪装为捕获结果"),
        }
    }

    /// 同时存在复制和捕获时必须先交付复制，再保留已发布捕获供下一次消费。
    #[test]
    fn 仅复制优先于已发布捕获() {
        let inbox = ClipboardCaptureInbox::new();
        inbox.publish(Ok(ClipboardCaptureResult {
            sequence: 41,
            source: None,
            payload: ClipboardPayload::from_text("待处理捕获"),
        }));
        inbox
            .request_copy(ClipboardCopyRequest::new(3, [3; 32]))
            .expect("复制请求应进入工作桥");

        assert!(matches!(
            inbox.wait_take_work(),
            Some(ClipboardWorkItem::Copy(request)) if request.id == 3
        ));
        assert!(matches!(
            inbox.wait_take_work(),
            Some(ClipboardWorkItem::Capture(Ok(result))) if result.sequence == 41
        ));
    }

    /// 桥关闭后不允许再写入复制命令，避免 UI 退出阶段阻塞或复活后台任务。
    #[test]
    fn 关闭后拒绝仅复制请求() {
        let inbox = ClipboardCaptureInbox::new();
        inbox.close();
        assert_eq!(
            inbox.request_copy(ClipboardCopyRequest::new(1, [1; 32])),
            Err(ClipboardWorkerError::Disconnected)
        );
    }

    /// 匹配的自身写回结果必须从历史桥吞掉，第二次看到同一结果则恢复正常发布。
    #[test]
    fn 自身写回结果只消费一次() {
        let expectations = ClipboardWriteExpectationStore::new();
        let payload = ClipboardPayload::from_text("自身写回");
        let hash = payload.summary().content_hash;
        let token = expectations
            .arm(hash, ClipboardWriteFormat::UnicodeText)
            .expect("预期队列应接受自身写回");
        expectations.bind_sequence(token, 44);
        let result = Ok(ClipboardCaptureResult {
            sequence: 44,
            source: None,
            payload,
        });

        assert!(!should_publish_capture(&result, &expectations));
        assert!(should_publish_capture(&result, &expectations));
    }
}

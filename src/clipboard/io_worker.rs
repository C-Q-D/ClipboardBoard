//! 此模块建立专用 ClipboardIO worker 线程和容量为一的 latest-wins 请求队列。
//!
//! 消息线程只提交剪贴板 sequence 与来源快照；worker 是当前唯一允许调用 Win32 剪贴板
//! 读取 API 的线程。队列在锁内替换尚未开始的旧请求，快速复制时不会阻塞消息泵或无界
//! 堆积；请求响应通过拥有型 DTO 返回，后续业务层可以继续在 UI 线程外处理正文。

use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use super::reader::{
    read_capture_payload_with_backend, read_text_with_backend, ClipboardCapturePayload,
    ClipboardReadError, RetryPolicy, Win32ClipboardBackend,
};
use super::writer::{ClipboardWriteExpectationStore, ClipboardWriteFormat};
use crate::domain::ClipboardPayload;
use crate::platform::windows::ProcessSource;
use crate::privacy::{GateMode, RecordingGate};

/// worker 请求的有限错误集合，不携带线程 panic 文本或外部字符串。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardWorkerError {
    /// worker 线程无法创建。
    ThreadStart,
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
#[derive(Clone, Eq, PartialEq)]
pub struct ClipboardCaptureResult {
    /// 与本次读取绑定的剪贴板序号，供后续历史协调器建立幂等键。
    pub sequence: u32,
    /// 与该序号同时捕获的来源快照；不会从 worker 重新查询前台窗口。
    pub source: Option<ProcessSource>,
    /// 已脱离 HGLOBAL 生命周期的唯一文本或图片 payload。
    pub payload: ClipboardCapturePayload,
}

impl std::fmt::Debug for ClipboardCaptureResult {
    /// 只输出序号、来源是否存在和 payload 摘要，禁止诊断递归展开正文。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClipboardCaptureResult")
            .field("sequence", &self.sequence)
            .field("source_present", &self.source.is_some())
            .field("payload", &self.payload)
            .finish()
    }
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
    /// 复制请求使用独立锁自由槽，避免 UI 等待捕获结果的互斥临界区。
    copy_slot: Arc<CopyRequestSlot>,
    /// 容量一唤醒令牌发送端；满槽代表消费者已经有一次待处理唤醒。
    wake_sender: SyncSender<()>,
    /// 唯一消费者通过共享接收端阻塞等待，克隆 inbox 不会复制令牌队列。
    wake_receiver: Arc<Mutex<Receiver<()>>>,
}

/// 捕获结果桥的内部状态。
struct CaptureInboxState {
    /// 尚未消费的唯一最新结果；成功和 sequence 失配错误都占用同一槽位。
    pending: Option<Result<ClipboardCaptureResult, ClipboardReadError>>,
    /// 关闭闩锁；worker 停止后消费者不再等待。
    closed: bool,
}

/// 容量一的锁自由复制请求槽；关闭哨兵把发布、取出和关闭线性化在同一原子指针上。
struct CopyRequestSlot {
    /// 空指针表示无请求，悬空哨兵表示永久关闭，其余指针独占一个堆分配请求。
    pointer: AtomicPtr<ClipboardCopyRequest>,
}

impl CopyRequestSlot {
    /// 创建开放且为空的复制槽。
    fn new() -> Self {
        Self {
            pointer: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    /// 返回地址为 1 的失配指针作为关闭哨兵；真实 Box 指针满足类型对齐，不会与其相等。
    fn closed_pointer() -> *mut ClipboardCopyRequest {
        std::ptr::without_provenance_mut(1)
    }

    /// 锁自由发布最新请求；成功 CAS 后调用方取得旧指针的唯一释放权。
    fn publish(&self, request: ClipboardCopyRequest) -> Result<(), ClipboardWorkerError> {
        let next = Box::into_raw(Box::new(request));
        loop {
            let current = self.pointer.load(Ordering::Acquire);
            if current == Self::closed_pointer() {
                // SAFETY: `next` 尚未进入原子槽，仍由当前调用独占。
                unsafe { drop(Box::from_raw(next)) };
                return Err(ClipboardWorkerError::Disconnected);
            }
            match self.pointer.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(previous) => {
                    if !previous.is_null() {
                        // SAFETY: CAS 已把 previous 移出槽，且关闭哨兵在上方被排除。
                        unsafe { drop(Box::from_raw(previous)) };
                    }
                    return Ok(());
                }
                Err(_) => {
                    // 其他发布、取出或关闭赢得本轮；重读状态，不等待任何互斥锁。
                }
            }
        }
    }

    /// 锁自由取出当前请求；关闭或空槽都返回 `None`。
    fn take(&self) -> Option<ClipboardCopyRequest> {
        loop {
            let current = self.pointer.load(Ordering::Acquire);
            if current.is_null() || current == Self::closed_pointer() {
                return None;
            }
            match self.pointer.compare_exchange_weak(
                current,
                std::ptr::null_mut(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(previous) => {
                    // SAFETY: CAS 已把 previous 从槽中唯一取出，且它不是空或关闭哨兵。
                    return Some(unsafe { *Box::from_raw(previous) });
                }
                Err(_) => {
                    // 与发布或关闭竞争失败时只重试原子操作，不阻塞生产者或消费者。
                }
            }
        }
    }

    /// 永久关闭复制槽并丢弃尚未取出的请求；关闭前已取出的请求仍由消费者独占完成。
    fn close(&self) {
        let previous = self.pointer.swap(Self::closed_pointer(), Ordering::AcqRel);
        if !previous.is_null() && previous != Self::closed_pointer() {
            // SAFETY: swap 已把 previous 唯一移出槽，消费者不再能取得它。
            unsafe { drop(Box::from_raw(previous)) };
        }
    }
}

impl Drop for CopyRequestSlot {
    /// 释放最后一个 inbox 被丢弃时仍留在槽中的请求。
    fn drop(&mut self) {
        let pointer = *self.pointer.get_mut();
        if !pointer.is_null() && pointer != Self::closed_pointer() {
            // SAFETY: Drop 持有 CopyRequestSlot 独占引用，不存在并发原子访问。
            unsafe { drop(Box::from_raw(pointer)) };
        }
    }
}

impl ClipboardCaptureInbox {
    /// 创建开放的空结果桥。
    pub fn new() -> Self {
        let (wake_sender, wake_receiver) = mpsc::sync_channel(1);
        Self {
            state: Arc::new(Mutex::new(CaptureInboxState {
                pending: None,
                closed: false,
            })),
            copy_slot: Arc::new(CopyRequestSlot::new()),
            wake_sender,
            wake_receiver: Arc::new(Mutex::new(wake_receiver)),
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

    /// 通过锁自由原子槽非阻塞发布最新复制请求；永久关闭后立即返回 `Disconnected`。
    pub fn request_copy(&self, request: ClipboardCopyRequest) -> Result<(), ClipboardWorkerError> {
        self.copy_slot.publish(request)?;
        self.signal_work();
        Ok(())
    }

    /// 阻塞取得最新捕获或写回命令；最新复制动作优先，避免用户操作被捕获高峰饿死。
    pub fn wait_take_work(&self) -> Option<ClipboardWorkItem> {
        loop {
            if let Some(request) = self.copy_slot.take() {
                return Some(ClipboardWorkItem::Copy(request));
            }
            {
                let mut state = self.state.lock().ok()?;
                // 获取捕获锁后再次检查复制槽，缩小复制与捕获同时到达时的优先级竞态。
                if let Some(request) = self.copy_slot.take() {
                    return Some(ClipboardWorkItem::Copy(request));
                }
                if let Some(result) = state.pending.take() {
                    return Some(ClipboardWorkItem::Capture(result));
                }
                if state.closed {
                    return None;
                }
            }
            // 容量一令牌不会丢失“检查后、等待前”到达的唤醒，且生产者只调用 try_send。
            self.wake_receiver.lock().ok()?.recv().ok()?;
        }
    }

    /// UI 接受退出事件时立即关闭复制入口；该方法只执行原子交换和非阻塞唤醒。
    ///
    /// 关闭线性化点之前已经由消费者取出的请求属于允许完成的在途工作，主线程会在
    /// 进程退出前 join 结果泵；线性化点之后的新请求一律返回 `Disconnected`。
    pub fn close_copy_requests(&self) {
        self.copy_slot.close();
        self.signal_work();
    }

    /// 标记结果桥关闭，保留已经发布的捕获结果并唤醒等待中的结果泵。
    ///
    /// 生命周期协调器应在 join 结果泵之前调用它：线性化点之前已取出的在途读取仍可
    /// 发布最终结果，之后的新捕获和写回请求一律被拒绝，不会在 UI 已退出后继续工作。
    pub fn close(&self) {
        self.close_copy_requests();
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
        }
        self.signal_work();
    }

    /// 从 worker 发布最新结果；桥关闭或锁中毒时安全丢弃，不阻塞剪贴板读取线程。
    pub(crate) fn publish(&self, result: Result<ClipboardCaptureResult, ClipboardReadError>) {
        if let Ok(mut state) = self.state.lock() {
            if state.closed {
                return;
            }
            state.pending = Some(result);
            self.signal_work();
        }
    }

    /// 非阻塞发布容量一唤醒令牌；Full 表示已有令牌，Disconnected 表示消费者已结束。
    fn signal_work(&self) {
        let _ = self.wake_sender.try_send(());
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
        Self::start_with_gate(
            ClipboardCaptureInbox::new(),
            ClipboardWriteExpectationStore::new(),
            RecordingGate::new(GateMode::Active),
        )
    }

    /// 使用调用方提供的结果桥启动 worker，便于消息线程和后续协调器共享同一接缝。
    pub fn start_with_inbox(inbox: ClipboardCaptureInbox) -> Result<Self, ClipboardWorkerError> {
        Self::start_with_gate(
            inbox,
            ClipboardWriteExpectationStore::new(),
            RecordingGate::new(GateMode::Active),
        )
    }

    /// 使用调用方提供的写回预期启动 worker；自身写回事件会在发布历史前一次性消费。
    pub fn start_with_inbox_and_expectations(
        inbox: ClipboardCaptureInbox,
        expectations: ClipboardWriteExpectationStore,
    ) -> Result<Self, ClipboardWorkerError> {
        Self::start_with_gate(inbox, expectations, RecordingGate::new(GateMode::Active))
    }

    /// 使用调用方共享门禁启动 worker，暂停时 backend 甚至不会被构造。
    pub fn start_with_gate(
        inbox: ClipboardCaptureInbox,
        expectations: ClipboardWriteExpectationStore,
        gate: RecordingGate,
    ) -> Result<Self, ClipboardWorkerError> {
        let queue = Arc::new(LatestRequestQueue::new());
        let worker_queue = Arc::clone(&queue);
        let worker_inbox = inbox.clone();
        let worker_expectations = expectations;
        let worker_gate = gate.clone();
        let join_handle = thread::Builder::new()
            .name("ClipboardIoWorker".to_owned())
            .spawn(move || {
                worker_loop(worker_queue, worker_inbox, worker_expectations, worker_gate)
            })
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
    gate: RecordingGate,
) {
    while let Some(request) = queue.pop() {
        match request.response {
            ReadResponse::Payload(response) => {
                let result = gate.try_read().and_then(|_permit| {
                    let mut backend = Win32ClipboardBackend;
                    read_text_with_backend(
                        &mut backend,
                        request.expected_sequence,
                        RetryPolicy::default(),
                    )
                });
                let _ = response.send(result);
            }
            ReadResponse::Capture {
                response,
                sequence,
                source,
            } => {
                let capture_result = capture_with_factory(
                    &gate,
                    request.expected_sequence,
                    sequence,
                    source,
                    &inbox,
                    &expectations,
                    || Win32ClipboardBackend,
                );
                let _ = response.send(capture_result);
            }
        }
    }
}

/// 生产与测试共用的唯一捕获路径；许可覆盖 factory、读取、结果形成和 inbox 发布。
fn capture_with_factory<B, F>(
    gate: &RecordingGate,
    expected_sequence: Option<u32>,
    sequence: u32,
    source: Option<ProcessSource>,
    inbox: &ClipboardCaptureInbox,
    expectations: &ClipboardWriteExpectationStore,
    factory: F,
) -> Result<ClipboardCaptureResult, ClipboardReadError>
where
    B: super::reader::ClipboardBackend,
    F: FnOnce() -> B,
{
    let _permit = gate.try_read()?;
    let mut backend = factory();
    let result =
        read_capture_payload_with_backend(&mut backend, expected_sequence, RetryPolicy::default());
    let capture_result = result.map(|payload| ClipboardCaptureResult {
        sequence,
        source,
        payload,
    });
    if should_publish_capture(&capture_result, expectations) {
        inbox.publish(capture_result.clone());
    }
    capture_result
}

/// 仅在捕获结果不是已登记的自身 Unicode 写回时发布到历史桥，并保证预期只消费一次。
fn should_publish_capture(
    capture_result: &Result<ClipboardCaptureResult, ClipboardReadError>,
    expectations: &ClipboardWriteExpectationStore,
) -> bool {
    !capture_result.as_ref().is_ok_and(|capture| {
        let ClipboardCapturePayload::Text(payload) = &capture.payload else {
            return false;
        };
        expectations.consume_if_matches(
            capture.sequence,
            payload.summary().content_hash,
            ClipboardWriteFormat::UnicodeText,
        )
    })
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证 worker 的 latest-wins 队列、异步响应和停止回收协议。

    use super::{
        capture_with_factory, should_publish_capture, ClipboardCaptureInbox,
        ClipboardCaptureRequest, ClipboardCaptureResult, ClipboardCopyRequest, ClipboardIoWorker,
        ClipboardWorkItem, ClipboardWorkerError, LatestRequestQueue, ReadRequest, ReadResponse,
    };
    use crate::clipboard::reader::{ClipboardBackend, ClipboardReadError, DibClipboardBytes};
    use crate::clipboard::writer::{ClipboardWriteExpectationStore, ClipboardWriteFormat};
    use crate::clipboard::{ClipboardCapturePayload, ClipboardImageBytes};
    use crate::domain::ClipboardPayload;
    use crate::privacy::{GateMode, RecordingGate};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    /// 捕获结果的 Debug 只能输出类型摘要，不得回显文本正文或图片编码字节。
    #[test]
    fn 捕获结果_debug不泄漏正文() {
        let result = ClipboardCaptureResult {
            sequence: 7,
            source: None,
            payload: ClipboardCapturePayload::Text(ClipboardPayload::from_text("敏感剪贴板正文")),
        };
        let debug = format!("{result:?}");
        assert!(!debug.contains("敏感剪贴板正文"));
        assert!(debug.contains("byte_len"));
        assert!(debug.contains("source_present"));
    }

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
            payload: ClipboardPayload::from_text("A").into(),
        }));
        inbox.publish(Ok(ClipboardCaptureResult {
            sequence: 21,
            source: None,
            payload: ClipboardPayload::from_text("B").into(),
        }));

        let result = inbox
            .try_take()
            .expect("结果桥应有最新结果")
            .expect("B 应成功");
        assert_eq!(result.sequence, 21);
        let ClipboardCapturePayload::Text(payload) = result.payload else {
            panic!("最新结果应为文本");
        };
        assert_eq!(payload.as_text(), "B");
        assert!(inbox.try_take().is_none());
    }

    /// worker 关闭结果桥后，已经发布的最后结果仍应可被消费者取走，不能静默丢失正文。
    #[test]
    fn 结果桥关闭后保留已发布结果() {
        let inbox = ClipboardCaptureInbox::new();
        inbox.publish(Ok(ClipboardCaptureResult {
            sequence: 30,
            source: None,
            payload: ClipboardPayload::from_text("保留到关闭后").into(),
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

    /// 复制槽不依赖捕获状态锁，持锁期间连续发布仍必须成功并保留最新请求。
    #[test]
    fn 仅复制槽不受捕获锁竞争并保留最新() {
        let inbox = ClipboardCaptureInbox::new();
        let _held = inbox.state.lock().expect("测试应持有结果桥锁");

        inbox
            .request_copy(ClipboardCopyRequest::new(7, [7; 32]))
            .expect("锁自由复制槽不应等待捕获锁");
        inbox
            .request_copy(ClipboardCopyRequest::new(8, [8; 32]))
            .expect("第二次发布应原子替换旧请求");
        drop(_held);

        assert!(matches!(
            inbox.wait_take_work(),
            Some(ClipboardWorkItem::Copy(request))
                if request.id == 8 && request.content_hash == [8; 32]
        ));
    }

    /// 同时存在复制和捕获时必须先交付复制，再保留已发布捕获供下一次消费。
    #[test]
    fn 仅复制优先于已发布捕获() {
        let inbox = ClipboardCaptureInbox::new();
        inbox.publish(Ok(ClipboardCaptureResult {
            sequence: 41,
            source: None,
            payload: ClipboardPayload::from_text("待处理捕获").into(),
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

    /// 关闭线性化点前已取出的请求允许完成，关闭后尚未取出和新发布的请求都必须被拒绝。
    #[test]
    fn 复制关闭门禁区分在途和待处理请求() {
        let inbox = ClipboardCaptureInbox::new();
        inbox
            .request_copy(ClipboardCopyRequest::new(1, [1; 32]))
            .expect("首个请求应成功发布");
        let in_flight = match inbox.wait_take_work() {
            Some(ClipboardWorkItem::Copy(request)) => request,
            _ => panic!("关闭前请求应成为消费者独占的在途工作"),
        };
        inbox
            .request_copy(ClipboardCopyRequest::new(2, [2; 32]))
            .expect("第二个请求应等待消费");

        inbox.close_copy_requests();

        assert_eq!(in_flight.id, 1);
        assert!(inbox.copy_slot.take().is_none());
        assert_eq!(
            inbox.request_copy(ClipboardCopyRequest::new(3, [3; 32])),
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
            payload: payload.into(),
        });

        assert!(!should_publish_capture(&result, &expectations));
        assert!(should_publish_capture(&result, &expectations));
    }

    /// 假 backend 支持文本或注册 PNG，对照同一生产捕获函数的两类结果。
    struct FakeCaptureBackend {
        image: bool,
    }

    impl ClipboardBackend for FakeCaptureBackend {
        fn open(&mut self) -> bool {
            true
        }

        fn close(&mut self) -> bool {
            true
        }

        fn sequence(&mut self) -> u32 {
            44
        }

        fn read_unicode_text(
            &mut self,
            _max_bytes: usize,
        ) -> Result<ClipboardPayload, ClipboardReadError> {
            Ok(ClipboardPayload::from_text("允许读取"))
        }

        fn read_registered_png_bytes(
            &mut self,
            _max_bytes: usize,
        ) -> Result<Vec<u8>, ClipboardReadError> {
            if self.image {
                Ok(vec![1, 2, 3])
            } else {
                Err(ClipboardReadError::RegisteredPngUnavailable)
            }
        }

        fn read_dib_bytes(
            &mut self,
            _max_bytes: usize,
        ) -> Result<DibClipboardBytes, ClipboardReadError> {
            Err(ClipboardReadError::DibUnavailable)
        }
    }

    /// 暂停路径不得构造 backend；Active 对照必须发布文本和图片。
    #[test]
    fn 生产捕获路径在factory之前执行暂停门禁() {
        let paused = RecordingGate::new(GateMode::Paused);
        let inbox = ClipboardCaptureInbox::new();
        let expectations = ClipboardWriteExpectationStore::new();
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&factory_calls);
        let result = capture_with_factory(
            &paused,
            Some(44),
            44,
            None,
            &inbox,
            &expectations,
            move || {
                calls.fetch_add(1, Ordering::AcqRel);
                FakeCaptureBackend { image: false }
            },
        );
        assert_eq!(result, Err(ClipboardReadError::Paused));
        assert_eq!(factory_calls.load(Ordering::Acquire), 0);
        assert!(inbox.try_take().is_none());

        let active = RecordingGate::new(GateMode::Active);
        for image in [false, true] {
            let result =
                capture_with_factory(&active, Some(44), 44, None, &inbox, &expectations, || {
                    FakeCaptureBackend { image }
                })
                .unwrap();
            match (image, result.payload) {
                (true, ClipboardCapturePayload::Image(ClipboardImageBytes::RegisteredPng(_)))
                | (false, ClipboardCapturePayload::Text(_)) => {}
                _ => panic!("Active 对照返回了错误类型"),
            }
            assert!(inbox.try_take().is_some());
        }
    }
}

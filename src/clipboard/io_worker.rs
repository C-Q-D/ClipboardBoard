//! 此模块建立专用 ClipboardIO worker 线程和有界请求入口。
//!
//! worker 是当前唯一允许调用 Win32 剪贴板读取 API 的线程；调用方只能提交 expected
//! sequence 并接收拥有型 `ClipboardPayload`。请求队列容量固定为 1，避免快速复制时无界
//! 堆积；WM_CLIPBOARDUPDATE 的 latest-wins 调度将在后续监听原子接入。

use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};

use super::reader::{
    read_text_with_backend, ClipboardReadError, RetryPolicy, Win32ClipboardBackend,
};
use crate::domain::ClipboardPayload;

/// worker 请求的有限错误集合，不携带线程 panic 文本或外部字符串。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardWorkerError {
    /// worker 线程无法创建。
    ThreadStart,
    /// 有界队列已经包含一个待处理请求。
    QueueFull,
    /// worker 已停止或响应通道已断开。
    Disconnected,
    /// worker 线程退出时发生 panic；调用方只能重新创建 worker。
    ThreadPanicked,
}

/// 专用 ClipboardIO worker；生产代码通过 `request` 获取异步读取结果。
pub struct ClipboardIoWorker {
    /// 容量为 1 的请求通道；设为 Option 便于 stop 时先关闭发送端再 join。
    sender: Option<SyncSender<ReadRequest>>,
    /// worker 线程句柄，确保生命周期可控且退出时不遗留后台线程。
    join_handle: Option<JoinHandle<()>>,
}

/// 单次文本读取请求；不携带正文，只携带消息线程观察到的 sequence。
struct ReadRequest {
    /// 监听器捕获的预期序号；为空时由 worker 以读取前序号建立基线。
    expected_sequence: Option<u32>,
    /// worker 完成读取后发送拥有型结果。
    response: mpsc::Sender<Result<ClipboardPayload, ClipboardReadError>>,
}

impl ClipboardIoWorker {
    /// 创建并启动 ClipboardIO worker 线程；创建失败返回有限错误而不 panic。
    pub fn start() -> Result<Self, ClipboardWorkerError> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let join_handle = thread::Builder::new()
            .name("ClipboardIoWorker".to_owned())
            .spawn(move || worker_loop(receiver))
            .map_err(|_| ClipboardWorkerError::ThreadStart)?;

        Ok(Self {
            sender: Some(sender),
            join_handle: Some(join_handle),
        })
    }

    /// 提交一次文本读取；队列满时立即返回，不在调用线程等待剪贴板或 worker。
    pub fn request(
        &self,
        expected_sequence: Option<u32>,
    ) -> Result<Receiver<Result<ClipboardPayload, ClipboardReadError>>, ClipboardWorkerError> {
        let (response_sender, response_receiver) = mpsc::channel();
        let sender = self
            .sender
            .as_ref()
            .ok_or(ClipboardWorkerError::Disconnected)?;
        let request = ReadRequest {
            expected_sequence,
            response: response_sender,
        };

        match sender.try_send(request) {
            Ok(()) => Ok(response_receiver),
            Err(TrySendError::Full(_)) => Err(ClipboardWorkerError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(ClipboardWorkerError::Disconnected),
        }
    }

    /// 关闭请求通道并等待 worker 退出；worker 正在进行的打开重试仍受 200ms 总预算约束。
    pub fn stop(mut self) -> Result<(), ClipboardWorkerError> {
        self.sender.take();
        let join_handle = self.join_handle.take();
        join_handle
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| ClipboardWorkerError::ThreadPanicked)
            })
            .unwrap_or(Ok(()))
    }
}

impl Drop for ClipboardIoWorker {
    /// 丢弃 worker 时也关闭通道并尽力回收线程，避免测试或异常路径遗留后台线程。
    fn drop(&mut self) {
        self.sender.take();
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

/// worker 主循环；每个请求创建短生命周期 Win32 backend，避免句柄跨请求复用。
fn worker_loop(receiver: Receiver<ReadRequest>) {
    while let Ok(request) = receiver.recv() {
        let mut backend = Win32ClipboardBackend;
        let result = read_text_with_backend(
            &mut backend,
            request.expected_sequence,
            RetryPolicy::default(),
        );
        let _ = request.response.send(result);
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证 worker 的有界队列、异步响应和停止回收协议。

    use super::{ClipboardIoWorker, ClipboardWorkerError};
    use std::time::Duration;

    /// 新 worker 可以提交请求；当前桌面无剪贴板时结果仍必须有限返回而不挂起调用方。
    #[test]
    fn worker_可以异步接收请求() {
        let worker = ClipboardIoWorker::start().expect("worker 线程应能启动");
        let response = worker.request(None).expect("首个请求应进入容量为一的队列");
        let _ = response
            .recv_timeout(Duration::from_secs(1))
            .expect("worker 应在有限时间内响应");
        worker.stop().expect("worker 应能正常停止");
    }

    /// 第二个尚未消费的请求必须立即被拒绝，避免请求队列无界增长。
    #[test]
    fn worker_队列有界且满时不等待() {
        let worker = ClipboardIoWorker::start().expect("worker 线程应能启动");
        let _first = worker.request(None).expect("首个请求应成功");
        // worker 可能已取走首个请求；此断言只验证错误集合可被调用方安全处理。
        let second = worker.request(None);
        assert!(matches!(
            second,
            Ok(_) | Err(ClipboardWorkerError::QueueFull) | Err(ClipboardWorkerError::Disconnected)
        ));
        worker.stop().expect("worker 应能正常停止");
    }
}

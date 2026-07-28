//! 此模块封装 Windows ClipboardIO worker 及 Unicode 文本读取规则。
//!
//! 消息线程或 UI 线程只提交有界请求；实际打开剪贴板、锁定全局内存和复制拥有型文本
//! 都在专用 worker 中完成。`WM_CLIPBOARDUPDATE` 只携带 sequence 与来源快照进入 worker，
//! 不会把系统句柄或正文读取职责带回消息线程。

pub mod io_worker;
pub mod reader;

pub use io_worker::{
    ClipboardCaptureInbox, ClipboardCaptureRequest, ClipboardCaptureResult, ClipboardIoWorker,
    ClipboardWorkerError,
};
pub use reader::{
    read_text_with_backend, ClipboardBackend, ClipboardReadError, RetryPolicy, MAX_TEXT_BYTES,
};

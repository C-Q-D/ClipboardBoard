//! 此模块封装 Windows ClipboardIO worker、Unicode 文本读取和仅复制写回规则。
//!
//! 捕获消息线程或 UI 线程只提交有界请求；实际打开剪贴板、锁定全局内存和复制拥有型文本
//! 由捕获 worker 完成，历史写回则在结果泵线程完成。`WM_CLIPBOARDUPDATE` 只携带 sequence
//! 与来源快照进入 worker，不会把系统句柄或正文读取职责带回消息线程。

pub mod io_worker;
pub mod reader;
pub mod writer;

pub use io_worker::{
    ClipboardCaptureInbox, ClipboardCaptureRequest, ClipboardCaptureResult, ClipboardCopyRequest,
    ClipboardIoWorker, ClipboardWorkItem, ClipboardWorkerError,
};
pub use reader::{
    read_capture_payload_with_backend, read_text_with_backend, ClipboardBackend,
    ClipboardCapturePayload, ClipboardImageBytes, ClipboardReadError, RetryPolicy, MAX_TEXT_BYTES,
};
pub use writer::{
    ClipboardWriteError, ClipboardWriteExpectationStore, ClipboardWriteFormat, ClipboardWriter,
};

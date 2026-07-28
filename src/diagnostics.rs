//! 此模块提供隐私安全的诊断日志入口。
//!
//! 日志字段由 `DiagnosticEvent` 的有限类型白名单固定，序列化不使用 `Debug`、
//! `Display` 或任意字符串回退；全局 writer 只通过 `init` 和 `emit` 访问，写入失败时
//! 禁用日志并丢弃当前事件，避免日志故障反向阻塞 UI、Win32 消息线程或剪贴板线程。
//! 默认日志文件位于 `%LOCALAPPDATA%\\ClipboardBoard\\logs\\diagnostics.log`，打开失败时
//! 退回固定 stderr 流；路径本身不会进入日志字段。

use std::fs::{create_dir_all, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// 剪贴板记录类型的有限集合；禁止调用方传入原始格式名称或正文片段。
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ClipboardItemType {
    /// Unicode 或纯文本记录。
    Text,
    /// 位图或 PNG 等图片记录。
    Image,
    /// 后续可能支持的 HTML 记录。
    Html,
    /// 尚未识别的记录类型。
    Unknown,
}

impl ClipboardItemType {
    /// 返回稳定的 ASCII 日志字面量，避免格式化任意外部字符串。
    const fn as_log_value(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Html => "html",
            Self::Unknown => "unknown",
        }
    }
}

/// 后台线程状态的有限集合；不记录线程名、路径或其他可识别信息。
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ThreadState {
    /// 线程或应用正在启动。
    Starting,
    /// 线程已进入工作状态。
    Running,
    /// 线程收到停止请求。
    Stopping,
    /// 线程已完成清理。
    Stopped,
    /// 线程遇到未细分的错误。
    Error,
}

impl ThreadState {
    /// 返回稳定的 ASCII 日志字面量，不暴露线程名称或错误文本。
    const fn as_log_value(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Error => "error",
        }
    }
}

/// 诊断事件的唯一字段白名单。
///
/// 该结构刻意没有 `String`、`Vec<u8>`、窗口标题、线程名或错误对象字段，调用方无法
/// 通过公共事件类型把剪贴板正文、图片内容或任意外部文本传入日志序列化器。
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct DiagnosticEvent {
    /// 历史记录稳定 ID；尚未持久化时为空。
    pub record_id: Option<u64>,
    /// 固定的剪贴板记录类型。
    pub item_type: Option<ClipboardItemType>,
    /// 内容字节数，只记录大小而不记录内容。
    pub byte_count: Option<u64>,
    /// 处理耗时，单位为毫秒；超出 `u64` 时饱和到最大值。
    pub duration_ms: Option<u64>,
    /// Windows 或业务错误码；不记录错误消息文本。
    pub error_code: Option<u32>,
    /// 线程生命周期状态。
    pub thread_state: Option<ThreadState>,
}

impl DiagnosticEvent {
    /// 创建空事件；生产调用方应至少设置一个白名单字段。
    pub const fn empty() -> Self {
        Self {
            record_id: None,
            item_type: None,
            byte_count: None,
            duration_ms: None,
            error_code: None,
            thread_state: None,
        }
    }

    /// 创建一条记录摘要事件，不接收正文或图片字节。
    pub const fn record(record_id: u64, item_type: ClipboardItemType, byte_count: u64) -> Self {
        Self {
            record_id: Some(record_id),
            item_type: Some(item_type),
            byte_count: Some(byte_count),
            duration_ms: None,
            error_code: None,
            thread_state: None,
        }
    }

    /// 创建一条线程状态事件。
    pub const fn thread_state(state: ThreadState) -> Self {
        Self {
            thread_state: Some(state),
            ..Self::empty()
        }
    }

    /// 增加耗时字段；`Duration` 转换为毫秒并在 `u64` 上饱和。
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration_ms = Some(duration.as_millis().try_into().unwrap_or(u64::MAX));
        self
    }

    /// 增加错误码；错误正文必须在调用方丢弃，不得转换成字符串传入。
    pub const fn with_error_code(mut self, error_code: u32) -> Self {
        self.error_code = Some(error_code);
        self
    }
}

/// 全局 writer 的私有状态；外部只能通过 `init` 和 `emit` 触达它。
struct WriterState {
    /// writer 是否仍可用；任何写入或刷新失败后关闭，避免后续调用反复阻塞业务线程。
    enabled: bool,
    /// 私有日志目标；生产环境为固定本地文件，失败时仅回退固定 stderr 流。
    writer: Box<dyn Write + Send>,
}

/// 单条诊断日志允许的最大字节数；固定字段和有限数值会远小于该上限。
const MAX_LOG_LINE_BYTES: usize = 256;

impl WriterState {
    /// 打开固定的本地日志文件；目录或文件失败时回退标准错误流。
    fn default_sink() -> Self {
        if let Some(path) = default_log_path() {
            if let Some(parent) = path.parent() {
                if create_dir_all(parent).is_ok() {
                    if let Ok(file) = OpenOptions::new().create(true).append(true).open(path) {
                        return Self {
                            enabled: true,
                            writer: Box::new(file),
                        };
                    }
                }
            }
        }

        // fail-open：无法创建本地日志时仍保留固定字段的 stderr 诊断，不记录路径或正文。
        Self {
            enabled: true,
            writer: Box::new(io::stderr()),
        }
    }

    /// 使用隔离 writer 写入一条事件；任意写入失败都会禁用后续日志。
    fn emit(&mut self, event: DiagnosticEvent) {
        if !self.enabled {
            return;
        }

        let result = (|| {
            // 先在内存中完成整行序列化，再一次性写入文件，避免多进程追加时字段交错。
            let mut line = Vec::with_capacity(MAX_LOG_LINE_BYTES);
            serialize_event(&mut line, event)?;
            if line.len() > MAX_LOG_LINE_BYTES {
                return Err(io::Error::other("诊断日志行超过固定长度上限"));
            }
            self.writer.write_all(&line)?;
            self.writer.flush()
        })();
        if result.is_err() {
            // fail-open：日志不能影响业务线程，且不再递归记录日志错误。
            self.enabled = false;
        }
    }
}

/// 全局日志状态；OnceLock 保证重复初始化只建立一个 writer。
static LOGGER: OnceLock<Mutex<WriterState>> = OnceLock::new();

/// 初始化诊断日志；主线程应在启动 Win32/剪贴板 worker 前调用，重复调用幂等。
pub fn init() {
    let _ = LOGGER.get_or_init(|| Mutex::new(WriterState::default_sink()));
}

/// 写入一条隐私安全诊断事件；无法获取锁或 writer 已失败时直接丢弃。
pub fn emit(event: DiagnosticEvent) {
    let logger = LOGGER.get_or_init(|| Mutex::new(WriterState::default_sink()));
    emit_to_logger(logger, event);
}

/// 通过 try-lock 进入共享 writer；生产入口和隔离测试共用这条路径。
fn emit_to_logger(logger: &Mutex<WriterState>, event: DiagnosticEvent) {
    let Ok(mut state) = logger.try_lock() else {
        // try_lock 避免日志在 UI、Win32 或剪贴板线程上形成不可控等待。
        return;
    };
    state.emit(event);
}

/// 返回固定的本地日志路径；路径本身不进入任何日志字段。
fn default_log_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|root| {
        PathBuf::from(root)
            .join("ClipboardBoard")
            .join("logs")
            .join("diagnostics.log")
    })
}

/// 按固定字段顺序序列化，禁止添加正文、图片字节、窗口标题和任意字符串字段。
fn serialize_event(writer: &mut dyn Write, event: DiagnosticEvent) -> io::Result<()> {
    write!(writer, "record_id=")?;
    write_optional_u64(writer, event.record_id)?;
    write!(writer, " item_type=")?;
    match event.item_type {
        Some(item_type) => write!(writer, "{}", item_type.as_log_value())?,
        None => write!(writer, "-")?,
    }
    write!(writer, " byte_count=")?;
    write_optional_u64(writer, event.byte_count)?;
    write!(writer, " duration_ms=")?;
    write_optional_u64(writer, event.duration_ms)?;
    write!(writer, " error_code=")?;
    write_optional_u32(writer, event.error_code)?;
    write!(writer, " thread_state=")?;
    match event.thread_state {
        Some(thread_state) => write!(writer, "{}", thread_state.as_log_value())?,
        None => write!(writer, "-")?,
    }
    writeln!(writer)
}

/// 写入可选 u64；缺失字段固定为短横线，避免引入额外文本。
fn write_optional_u64(writer: &mut dyn Write, value: Option<u64>) -> io::Result<()> {
    match value {
        Some(value) => write!(writer, "{value}"),
        None => write!(writer, "-"),
    }
}

/// 写入可选 u32；错误码始终保持数值，不展开 Windows 错误消息。
fn write_optional_u32(writer: &mut dyn Write, value: Option<u32>) -> io::Result<()> {
    match value {
        Some(value) => write!(writer, "{value}"),
        None => write!(writer, "-"),
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证字段白名单、时长饱和和 writer 失败时的 fail-open 行为。

    use super::{emit_to_logger, ClipboardItemType, DiagnosticEvent, ThreadState, WriterState};
    use std::io::{self, Write};
    use std::time::Duration;

    /// 内存 sink 只用于测试，不接入全局 OnceLock，避免并行测试互相污染。
    struct MemoryWriter {
        /// 保存测试日志字节，供断言验证字段白名单和行边界。
        bytes: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl Write for MemoryWriter {
        /// 将测试日志保存在内存中供断言读取。
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes
                .lock()
                .map_err(|_| io::Error::other("内存 sink 锁中毒"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        /// 内存 sink 不需要额外刷新。
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// 特征正文只能作为测试变量存在，不能进入 DiagnosticEvent 或序列化输出。
    #[test]
    fn 日志序列化不包含正文且只输出固定字段() {
        let feature_text = "SECRET_CLIPBOARD_BODY_7f0a";
        let event = DiagnosticEvent::record(42, ClipboardItemType::Text, feature_text.len() as u64)
            .with_duration(Duration::from_millis(7))
            .with_error_code(5);
        let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let logger = std::sync::Mutex::new(WriterState {
            enabled: true,
            writer: Box::new(MemoryWriter {
                bytes: bytes.clone(),
            }),
        });

        // 走与公共 emit 相同的 try-lock 和 WriterState 路径，避免只测试孤立序列化函数。
        emit_to_logger(&logger, event);
        let output = String::from_utf8(bytes.lock().expect("内存 sink 锁不应中毒").clone())
            .expect("固定 ASCII 日志应为 UTF-8");

        assert_eq!(
            output,
            "record_id=42 item_type=text byte_count=26 duration_ms=7 error_code=5 thread_state=-\n"
        );
        assert!(!output.contains(feature_text));
        assert!(output.len() < 256);
    }

    /// 超长 Duration 必须饱和为 u64::MAX，不能溢出或改变字段单位。
    #[test]
    fn 时长转换饱和且单位固定为毫秒() {
        let event = DiagnosticEvent::empty().with_duration(Duration::from_millis(u64::MAX));
        assert_eq!(event.duration_ms, Some(u64::MAX));
    }

    /// writer 失败后必须禁用日志而不是 panic、重试或递归记录错误。
    #[test]
    fn writer_失败后进入静默禁用状态() {
        struct FailingWriter;

        impl Write for FailingWriter {
            /// 始终返回 I/O 错误，模拟磁盘或标准错误流不可用。
            fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("模拟写入失败"))
            }

            /// 刷新同样失败，验证写入后的 flush 错误也会进入 fail-open。
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("模拟刷新失败"))
            }
        }

        let mut state = WriterState {
            enabled: true,
            writer: Box::new(FailingWriter),
        };
        state.emit(DiagnosticEvent::thread_state(ThreadState::Running));
        assert!(!state.enabled);
        state.emit(DiagnosticEvent::thread_state(ThreadState::Stopped));
        assert!(!state.enabled);
    }
}

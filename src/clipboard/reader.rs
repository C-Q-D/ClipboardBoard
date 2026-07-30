//! 此模块定义 ClipboardIO 的读取算法、重试边界和 Win32 文本、注册 PNG 适配器。
//!
//! 算法先记录 sequence，再在有界时长内打开剪贴板，读取函数必须在返回前复制自有数据，
//! 最后关闭剪贴板并复核 sequence。测试通过 `ClipboardBackend` 注入假后端，不需要修改系统
//! 剪贴板状态；生产适配器只使用有限权限的系统 API，不把 HGLOBAL 句柄泄漏到领域层。

use std::thread;
use std::time::{Duration, Instant};

use crate::domain::ClipboardPayload;
use crate::image_decode::{MAX_DIB_ENCODED_BYTES, MAX_PNG_ENCODED_BYTES};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    OpenClipboard, RegisterClipboardFormatW,
};
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

/// 文本正文的默认 UTF-8 字节上限；超过上限必须在 worker 内丢弃。
pub const MAX_TEXT_BYTES: usize = 5 * 1024 * 1024;

/// Windows 预定义 Unicode 文本格式编号；windows-sys 不在 DataExchange 模块导出该常量。
const CF_UNICODETEXT_FORMAT: u32 = 13;
/// Windows 预定义 DIB 格式编号。
const CF_DIB_FORMAT: u32 = 8;
/// Windows 预定义 DIBV5 格式编号。
const CF_DIBV5_FORMAT: u32 = 17;

/// `OpenClipboard` 的默认总等待时长，避免剪贴板被其他进程占用时无限阻塞。
const DEFAULT_OPEN_TIMEOUT: Duration = Duration::from_millis(200);

/// 两次打开尝试之间的默认间隔；等待发生在专用 worker，不阻塞 UI 或消息线程。
const DEFAULT_RETRY_INTERVAL: Duration = Duration::from_millis(5);

/// 剪贴板打开重试策略；测试可以使用零间隔或零超时而不等待真实时钟。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    /// 所有尝试允许消耗的总时长。
    pub total_timeout: Duration,
    /// 相邻尝试之间的休眠时长。
    pub retry_interval: Duration,
}

impl Default for RetryPolicy {
    /// 返回生产默认的 200ms 总预算和 5ms 轮询间隔。
    fn default() -> Self {
        Self {
            total_timeout: DEFAULT_OPEN_TIMEOUT,
            retry_interval: DEFAULT_RETRY_INTERVAL,
        }
    }
}

/// ClipboardIO 读取失败的有限集合；不携带窗口标题、正文或 Win32 错误文本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardReadError {
    /// 在总预算内始终无法取得剪贴板所有权。
    OpenTimeout,
    /// 打开前或读取后 sequence 与预期不一致，结果必须丢弃。
    SequenceChanged { expected: u32, observed: u32 },
    /// 当前剪贴板没有 Unicode 文本格式。
    UnicodeTextUnavailable,
    /// 当前剪贴板没有注册 `PNG` 格式，或系统无法取得其格式编号。
    RegisteredPngUnavailable,
    /// Windows 无法注册或取得 `PNG` 剪贴板格式编号；不得把该故障当作格式缺失降级。
    ClipboardFormatRegistrationFailed,
    /// 剪贴板返回的 HGLOBAL 无法读取。
    GlobalMemoryUnavailable,
    /// 注册 PNG 的编码字节超过固定上限。
    PngEncodedTooLarge,
    /// 当前剪贴板既没有 DIBV5，也没有 DIB。
    DibUnavailable,
    /// DIB/DIBV5 编码字节超过固定上限。
    DibEncodedTooLarge,
    /// 文本缺少终止 NUL，或内存边界在上限前无法确认正文结束。
    MalformedUnicodeText,
    /// UTF-16 数据无法转换为有效 Unicode。
    InvalidUtf16,
    /// 转换后的 UTF-8 正文超过固定上限。
    TextTooLarge,
    /// 读取完成后关闭剪贴板失败；调用方不能继续假设状态一致。
    CloseFailed,
}

/// 从剪贴板取得的 DIB 格式身份，用于选择对应解析契约。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DibClipboardFormat {
    /// `CF_DIBV5`，包含 BITMAPV5HEADER 或系统合成的 V5 数据。
    DibV5,
    /// `CF_DIB`，包含 BITMAPINFOHEADER 或兼容扩展头。
    Dib,
}

impl DibClipboardFormat {
    /// 返回 Windows 预定义剪贴板格式编号。
    fn format_id(self) -> u32 {
        match self {
            Self::DibV5 => CF_DIBV5_FORMAT,
            Self::Dib => CF_DIB_FORMAT,
        }
    }
}

/// 已脱离 HGLOBAL 生命周期的 DIB 剪贴板字节。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DibClipboardBytes {
    /// 实际读取的系统格式。
    format: DibClipboardFormat,
    /// 在剪贴板打开期间复制出的拥有型字节。
    bytes: Vec<u8>,
}

/// 图片捕获的拥有型编码；Debug 只输出格式和长度，不泄漏图片字节。
#[derive(Clone, Eq, PartialEq)]
pub enum ClipboardImageBytes {
    /// Windows 注册 `PNG` 格式的原始编码。
    RegisteredPng(Vec<u8>),
    /// `CF_DIBV5` 的设备无关位图字节。
    DibV5(Vec<u8>),
    /// `CF_DIB` 的设备无关位图字节。
    Dib(Vec<u8>),
}

impl std::fmt::Debug for ClipboardImageBytes {
    /// 输出有限元数据，诊断日志不得包含图片正文。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (format, length) = match self {
            Self::RegisteredPng(bytes) => ("png", bytes.len()),
            Self::DibV5(bytes) => ("dibv5", bytes.len()),
            Self::Dib(bytes) => ("dib", bytes.len()),
        };
        formatter
            .debug_struct("ClipboardImageBytes")
            .field("format", &format)
            .field("encoded_len", &length)
            .finish()
    }
}

impl ClipboardImageBytes {
    /// 返回拥有编码的字节长度，供有界队列和测试检查。
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::RegisteredPng(bytes) | Self::DibV5(bytes) | Self::Dib(bytes) => bytes.len(),
        }
    }
}

/// 一次剪贴板捕获的唯一拥有型 payload；跨类型优先级由读取入口固定。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardCapturePayload {
    /// Unicode 文本领域 payload。
    Text(ClipboardPayload),
    /// PNG、DIBV5 或 DIB 的唯一最优图片编码。
    Image(ClipboardImageBytes),
}

impl From<ClipboardPayload> for ClipboardCapturePayload {
    /// 兼容现有文本测试和调用方的显式转换。
    fn from(value: ClipboardPayload) -> Self {
        Self::Text(value)
    }
}

impl DibClipboardBytes {
    /// 构造已拥有的 DIB 字节结果。
    fn new(format: DibClipboardFormat, bytes: Vec<u8>) -> Self {
        Self { format, bytes }
    }

    /// 返回实际读取的 DIB 格式。
    pub fn format(&self) -> DibClipboardFormat {
        self.format
    }

    /// 借用关闭剪贴板后仍然有效的编码字节。
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// 消费结果并返回拥有型编码字节。
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// ClipboardIO 使用的最小后端抽象；生产实现封装 Win32，测试实现可模拟占用和 sequence。
pub trait ClipboardBackend {
    /// 尝试打开剪贴板；失败表示被其他进程占用或不可用。
    fn open(&mut self) -> bool;

    /// 关闭当前线程打开的剪贴板，并返回 Win32 成功状态。
    fn close(&mut self) -> bool;

    /// 读取系统剪贴板 sequence；实现应在无法取得序号时返回稳定的失败标记。
    fn sequence(&mut self) -> u32;

    /// 在剪贴板仍打开时复制 Unicode 文本为领域 payload。
    fn read_unicode_text(
        &mut self,
        max_bytes: usize,
    ) -> Result<ClipboardPayload, ClipboardReadError>;

    /// 在剪贴板仍打开时把注册 PNG 的 HGLOBAL 复制为拥有型编码字节。
    fn read_registered_png_bytes(
        &mut self,
        max_bytes: usize,
    ) -> Result<Vec<u8>, ClipboardReadError>;

    /// 在剪贴板仍打开时优先复制 DIBV5，否则复制 DIB 的拥有型字节。
    fn read_dib_bytes(&mut self, max_bytes: usize)
        -> Result<DibClipboardBytes, ClipboardReadError>;
}

/// 使用后端执行一次完整的文本读取，并在打开前后复核 sequence。
///
/// `expected_sequence` 通常来自消息线程；为空时以读取前的 sequence 作为本次基线。无论
/// 读取成功或失败，函数都会尝试关闭已经打开的剪贴板，保证 HGLOBAL 不跨越后端边界。
pub fn read_text_with_backend<B: ClipboardBackend>(
    backend: &mut B,
    expected_sequence: Option<u32>,
    policy: RetryPolicy,
) -> Result<ClipboardPayload, ClipboardReadError> {
    let sequence_before = backend.sequence();
    if let Some(expected) = expected_sequence {
        if sequence_before != expected {
            return Err(ClipboardReadError::SequenceChanged {
                expected,
                observed: sequence_before,
            });
        }
    }

    open_with_retry(backend, policy)?;
    let read_result = backend.read_unicode_text(MAX_TEXT_BYTES);
    let sequence_after = backend.sequence();
    let close_succeeded = backend.close();
    if !close_succeeded {
        return Err(ClipboardReadError::CloseFailed);
    }

    if sequence_after != sequence_before {
        return Err(ClipboardReadError::SequenceChanged {
            expected: sequence_before,
            observed: sequence_after,
        });
    }
    read_result
}

/// 在一次打开/关闭周期内按 PNG、DIBV5、DIB、Unicode 文本选择唯一 payload。
///
/// “不可用”才允许降级；已选格式的超限、内存或解码前读取错误必须直接返回，避免同一
/// 剪贴板在不同机器上因错误掩盖而被记录成另一类型。
pub fn read_capture_payload_with_backend<B: ClipboardBackend>(
    backend: &mut B,
    expected_sequence: Option<u32>,
    policy: RetryPolicy,
) -> Result<ClipboardCapturePayload, ClipboardReadError> {
    let sequence_before = backend.sequence();
    if let Some(expected) = expected_sequence {
        if sequence_before != expected {
            return Err(ClipboardReadError::SequenceChanged {
                expected,
                observed: sequence_before,
            });
        }
    }

    open_with_retry(backend, policy)?;
    let read_result = match backend.read_registered_png_bytes(MAX_PNG_ENCODED_BYTES) {
        Ok(bytes) => Ok(ClipboardCapturePayload::Image(
            ClipboardImageBytes::RegisteredPng(bytes),
        )),
        Err(ClipboardReadError::RegisteredPngUnavailable) => {
            match backend.read_dib_bytes(MAX_DIB_ENCODED_BYTES) {
                Ok(dib) => {
                    let image = match dib.format() {
                        DibClipboardFormat::DibV5 => ClipboardImageBytes::DibV5(dib.into_bytes()),
                        DibClipboardFormat::Dib => ClipboardImageBytes::Dib(dib.into_bytes()),
                    };
                    Ok(ClipboardCapturePayload::Image(image))
                }
                Err(ClipboardReadError::DibUnavailable) => backend
                    .read_unicode_text(MAX_TEXT_BYTES)
                    .map(ClipboardCapturePayload::Text),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    };
    let sequence_after = backend.sequence();
    let close_succeeded = backend.close();
    if !close_succeeded {
        return Err(ClipboardReadError::CloseFailed);
    }
    if sequence_after != sequence_before {
        return Err(ClipboardReadError::SequenceChanged {
            expected: sequence_before,
            observed: sequence_after,
        });
    }
    read_result
}

/// 使用后端执行一次完整的注册 PNG 字节读取，并在打开前后复核 sequence。
///
/// 成功值已经复制到 `Vec<u8>`，因此关闭剪贴板后仍可安全解码。关闭失败优先于读取错误
/// 和 sequence 失配，避免调用方在系统所有权状态未知时误用结果。
pub fn read_registered_png_bytes_with_backend<B: ClipboardBackend>(
    backend: &mut B,
    expected_sequence: Option<u32>,
    policy: RetryPolicy,
) -> Result<Vec<u8>, ClipboardReadError> {
    let sequence_before = backend.sequence();
    if let Some(expected) = expected_sequence {
        if sequence_before != expected {
            return Err(ClipboardReadError::SequenceChanged {
                expected,
                observed: sequence_before,
            });
        }
    }

    open_with_retry(backend, policy)?;
    let read_result = backend.read_registered_png_bytes(MAX_PNG_ENCODED_BYTES);
    let sequence_after = backend.sequence();
    let close_succeeded = backend.close();
    if !close_succeeded {
        return Err(ClipboardReadError::CloseFailed);
    }
    if sequence_after != sequence_before {
        return Err(ClipboardReadError::SequenceChanged {
            expected: sequence_before,
            observed: sequence_after,
        });
    }
    read_result
}

/// 使用后端执行一次完整的 DIBV5/DIB 字节读取，并在打开前后复核 sequence。
///
/// 成功值携带实际格式和拥有型字节。函数不会解析像素；关闭失败优先于读取错误和
/// sequence 失配，与文本和注册 PNG 的所有权协议保持一致。
pub fn read_dib_bytes_with_backend<B: ClipboardBackend>(
    backend: &mut B,
    expected_sequence: Option<u32>,
    policy: RetryPolicy,
) -> Result<DibClipboardBytes, ClipboardReadError> {
    let sequence_before = backend.sequence();
    if let Some(expected) = expected_sequence {
        if sequence_before != expected {
            return Err(ClipboardReadError::SequenceChanged {
                expected,
                observed: sequence_before,
            });
        }
    }

    open_with_retry(backend, policy)?;
    let read_result = backend.read_dib_bytes(MAX_DIB_ENCODED_BYTES);
    let sequence_after = backend.sequence();
    let close_succeeded = backend.close();
    if !close_succeeded {
        return Err(ClipboardReadError::CloseFailed);
    }
    if sequence_after != sequence_before {
        return Err(ClipboardReadError::SequenceChanged {
            expected: sequence_before,
            observed: sequence_after,
        });
    }
    read_result
}

/// 在固定总时长内重试打开剪贴板；成功后把关闭责任交还给调用方。
fn open_with_retry<B: ClipboardBackend>(
    backend: &mut B,
    policy: RetryPolicy,
) -> Result<(), ClipboardReadError> {
    let started = Instant::now();
    loop {
        if backend.open() {
            return Ok(());
        }

        if started.elapsed() >= policy.total_timeout {
            return Err(ClipboardReadError::OpenTimeout);
        }

        if !policy.retry_interval.is_zero() {
            let remaining = policy.total_timeout.saturating_sub(started.elapsed());
            thread::sleep(policy.retry_interval.min(remaining));
        }
    }
}

/// 从已复制的 UTF-16 单元解析第一段 NUL 终止文本并应用 UTF-8 大小上限。
///
/// 该函数只读取调用方提供的切片；切片来自 GlobalSize 边界或测试数据，因此不会扫描未知
/// 内存。返回的 `ClipboardPayload` 已拥有正文，后续可以在关闭剪贴板后继续使用。
pub fn parse_utf16_text(
    units: &[u16],
    max_bytes: usize,
) -> Result<ClipboardPayload, ClipboardReadError> {
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .ok_or(ClipboardReadError::MalformedUnicodeText)?;
    let utf16 = &units[..end];
    // 第一遍只解码并累计精确 UTF-8 字节数；超限时不分配正文，合规时第二遍按精确容量构造。
    let mut utf8_length = 0_usize;
    for decoded in char::decode_utf16(utf16.iter().copied()) {
        let character = decoded.map_err(|_| ClipboardReadError::InvalidUtf16)?;
        utf8_length = utf8_length
            .checked_add(character.len_utf8())
            .ok_or(ClipboardReadError::TextTooLarge)?;
        if utf8_length > max_bytes {
            return Err(ClipboardReadError::TextTooLarge);
        }
    }

    let mut text = String::with_capacity(utf8_length);
    for decoded in char::decode_utf16(utf16.iter().copied()) {
        let character = decoded.map_err(|_| ClipboardReadError::InvalidUtf16)?;
        // 第一遍已验证总长度，这里的 checked_add 只保留防御性不变量，不会触发超限分支。
        let next_length = text
            .len()
            .checked_add(character.len_utf8())
            .ok_or(ClipboardReadError::TextTooLarge)?;
        if next_length > max_bytes {
            return Err(ClipboardReadError::TextTooLarge);
        }
        text.push(character);
    }
    Ok(ClipboardPayload::from_text(text))
}

/// Win32 剪贴板后端；所有原生句柄都在本函数内部关闭，不进入领域模型或 UI 线程。
pub struct Win32ClipboardBackend;

impl ClipboardBackend for Win32ClipboardBackend {
    /// 使用空 owner HWND 打开当前桌面剪贴板。
    fn open(&mut self) -> bool {
        unsafe { OpenClipboard(std::ptr::null_mut()) != 0 }
    }

    /// 关闭当前线程已打开的剪贴板。
    fn close(&mut self) -> bool {
        unsafe { CloseClipboard() != 0 }
    }

    /// 读取系统维护的单调递增 sequence。
    fn sequence(&mut self) -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }

    /// 读取 `CF_UNICODETEXT`，在返回前从 HGLOBAL 复制成 Rust `String`。
    fn read_unicode_text(
        &mut self,
        max_bytes: usize,
    ) -> Result<ClipboardPayload, ClipboardReadError> {
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT_FORMAT) } == 0 {
            return Err(ClipboardReadError::UnicodeTextUnavailable);
        }

        let handle = unsafe { GetClipboardData(CF_UNICODETEXT_FORMAT) };
        if handle.is_null() {
            return Err(ClipboardReadError::GlobalMemoryUnavailable);
        }

        read_global_unicode_text(handle, max_bytes)
    }

    /// 注册系统 `PNG` 格式，并在返回前把 HGLOBAL 完整复制为拥有型字节。
    fn read_registered_png_bytes(
        &mut self,
        max_bytes: usize,
    ) -> Result<Vec<u8>, ClipboardReadError> {
        let format_name: Vec<u16> = "PNG\0".encode_utf16().collect();
        let format = unsafe { RegisterClipboardFormatW(format_name.as_ptr()) };
        if format == 0 {
            return Err(ClipboardReadError::ClipboardFormatRegistrationFailed);
        }
        if unsafe { IsClipboardFormatAvailable(format) } == 0 {
            return Err(ClipboardReadError::RegisteredPngUnavailable);
        }

        let handle = unsafe { GetClipboardData(format) };
        if handle.is_null() {
            return Err(ClipboardReadError::GlobalMemoryUnavailable);
        }
        read_global_bytes(handle, max_bytes, ClipboardReadError::PngEncodedTooLarge)
    }

    /// 优先选择 `CF_DIBV5`，否则选择 `CF_DIB`，并复制对应 HGLOBAL。
    fn read_dib_bytes(
        &mut self,
        max_bytes: usize,
    ) -> Result<DibClipboardBytes, ClipboardReadError> {
        let format = select_dib_clipboard_format(|format_id| unsafe {
            IsClipboardFormatAvailable(format_id) != 0
        })
        .ok_or(ClipboardReadError::DibUnavailable)?;
        let handle = unsafe { GetClipboardData(format.format_id()) };
        if handle.is_null() {
            return Err(ClipboardReadError::GlobalMemoryUnavailable);
        }
        let bytes = read_global_bytes(handle, max_bytes, ClipboardReadError::DibEncodedTooLarge)?;
        Ok(DibClipboardBytes::new(format, bytes))
    }
}

/// 从 HGLOBAL 读取 Unicode 文本；扫描范围同时受全局内存大小和正文上限约束。
fn read_global_unicode_text(
    handle: HANDLE,
    max_bytes: usize,
) -> Result<ClipboardPayload, ClipboardReadError> {
    let byte_size = unsafe { GlobalSize(handle) };
    if byte_size < 2 {
        return Err(ClipboardReadError::GlobalMemoryUnavailable);
    }

    // ASCII 一个 UTF-16 单元对应一个 UTF-8 字节，因此至少扫描 max_bytes + 1 个单元，
    // 才能接受所有未超限的 ASCII 文本；多字节 Unicode 会由 parse_utf16_text 的增量预算提前拒绝。
    let max_units = max_bytes.saturating_add(1);
    let unit_count = (byte_size / 2).min(max_units);
    let locked = unsafe { GlobalLock(handle) };
    if locked.is_null() || unit_count == 0 {
        return Err(ClipboardReadError::GlobalMemoryUnavailable);
    }

    // GlobalLock 成功后必须在任何解析分支结束前 GlobalUnlock；解析结果已复制出自有字符串。
    let units = unsafe { std::slice::from_raw_parts(locked.cast::<u16>(), unit_count) };
    let result = parse_utf16_text(units, max_bytes).map_err(|error| {
        if unit_count < byte_size / 2 && error == ClipboardReadError::MalformedUnicodeText {
            ClipboardReadError::TextTooLarge
        } else {
            error
        }
    });
    unsafe {
        let _ = GlobalUnlock(handle);
    }
    result
}

/// 从 HGLOBAL 复制一份有界二进制内容，不把锁定指针或系统句柄泄漏给调用方。
fn read_global_bytes(
    handle: HANDLE,
    max_bytes: usize,
    too_large_error: ClipboardReadError,
) -> Result<Vec<u8>, ClipboardReadError> {
    let byte_size = unsafe { GlobalSize(handle) };
    if byte_size == 0 {
        return Err(ClipboardReadError::GlobalMemoryUnavailable);
    }
    if byte_size > max_bytes {
        return Err(too_large_error);
    }

    let locked = unsafe { GlobalLock(handle) };
    if locked.is_null() {
        return Err(ClipboardReadError::GlobalMemoryUnavailable);
    }
    // 复制完成后立即解锁；返回值不引用 HGLOBAL，后续关闭剪贴板不会使字节失效。
    let bytes = unsafe { std::slice::from_raw_parts(locked.cast::<u8>(), byte_size) }.to_vec();
    unsafe {
        let _ = GlobalUnlock(handle);
    }
    Ok(bytes)
}

/// 按 DIBV5、DIB 的固定顺序选择当前可用格式。
fn select_dib_clipboard_format(
    mut is_available: impl FnMut(u32) -> bool,
) -> Option<DibClipboardFormat> {
    if is_available(CF_DIBV5_FORMAT) {
        Some(DibClipboardFormat::DibV5)
    } else if is_available(CF_DIB_FORMAT) {
        Some(DibClipboardFormat::Dib)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块通过假后端验证文本、注册 PNG、重试、sequence 和资源上限边界。

    use super::{
        parse_utf16_text, read_capture_payload_with_backend, read_dib_bytes_with_backend,
        read_registered_png_bytes_with_backend, read_text_with_backend,
        select_dib_clipboard_format, ClipboardBackend, ClipboardCapturePayload,
        ClipboardImageBytes, ClipboardReadError, DibClipboardBytes, DibClipboardFormat,
        RetryPolicy, CF_DIBV5_FORMAT, CF_DIB_FORMAT, MAX_TEXT_BYTES,
    };
    use crate::domain::ClipboardPayload;
    use crate::image_decode::{MAX_DIB_ENCODED_BYTES, MAX_PNG_ENCODED_BYTES};
    use std::collections::VecDeque;
    use std::time::Duration;

    /// 可控制打开次数和 sequence 变化的内存后端，不触碰真实系统剪贴板。
    struct FakeBackend {
        /// 每次 open 调用返回的结果队列。
        opens: VecDeque<bool>,
        /// 每次 sequence 调用返回的结果队列。
        sequences: VecDeque<u32>,
        /// 队列耗尽后沿用的最后 sequence。
        last_sequence: u32,
        /// read_unicode_text 返回的固定结果。
        text: Result<ClipboardPayload, ClipboardReadError>,
        /// read_registered_png_bytes 返回的固定结果。
        png: Result<Vec<u8>, ClipboardReadError>,
        /// read_dib_bytes 返回的固定结果。
        dib: Result<DibClipboardBytes, ClipboardReadError>,
        /// close 是否成功。
        close_ok: bool,
    }

    impl ClipboardBackend for FakeBackend {
        /// 模拟剪贴板被占用后最终释放。
        fn open(&mut self) -> bool {
            self.opens.pop_front().unwrap_or(false)
        }

        /// 模拟关闭剪贴板。
        fn close(&mut self) -> bool {
            self.close_ok
        }

        /// 返回预先安排的 sequence，队列耗尽后复用最后一个值。
        fn sequence(&mut self) -> u32 {
            if let Some(sequence) = self.sequences.pop_front() {
                self.last_sequence = sequence;
            } else {
                // 队列耗尽后复用最后一次观察值，模拟剪贴板未发生变化。
            }
            self.last_sequence
        }

        /// 返回测试文本；生产后端在这里已经完成 HGLOBAL 到 String 的复制。
        fn read_unicode_text(
            &mut self,
            _max_bytes: usize,
        ) -> Result<ClipboardPayload, ClipboardReadError> {
            self.text.clone()
        }

        /// 返回测试 PNG 字节；生产后端在这里已经完成 HGLOBAL 到 Vec 的复制。
        fn read_registered_png_bytes(
            &mut self,
            max_bytes: usize,
        ) -> Result<Vec<u8>, ClipboardReadError> {
            let bytes = self.png.clone()?;
            if bytes.len() > max_bytes {
                return Err(ClipboardReadError::PngEncodedTooLarge);
            }
            Ok(bytes)
        }

        /// 返回测试 DIB 字节；生产后端在这里已经完成 HGLOBAL 到 Vec 的复制。
        fn read_dib_bytes(
            &mut self,
            max_bytes: usize,
        ) -> Result<DibClipboardBytes, ClipboardReadError> {
            let result = self.dib.clone()?;
            if result.as_bytes().len() > max_bytes {
                return Err(ClipboardReadError::DibEncodedTooLarge);
            }
            Ok(result)
        }
    }

    fn policy() -> RetryPolicy {
        RetryPolicy {
            total_timeout: Duration::ZERO,
            retry_interval: Duration::ZERO,
        }
    }

    /// PNG、DIB 和文本同时存在时必须只返回 PNG，且一次打开后完成全部选择。
    #[test]
    fn capture_prefers_png_over_dib_and_text_in_one_open_cycle() {
        let mut backend = FakeBackend {
            opens: VecDeque::from([true]),
            sequences: VecDeque::from([31, 31]),
            last_sequence: 31,
            text: Ok(ClipboardPayload::from_text("辅助文本")),
            png: Ok(vec![1, 2, 3]),
            dib: Ok(DibClipboardBytes::new(
                DibClipboardFormat::DibV5,
                vec![4, 5],
            )),
            close_ok: true,
        };

        let payload =
            read_capture_payload_with_backend(&mut backend, Some(31), policy()).expect("捕获失败");
        assert_eq!(
            payload,
            ClipboardCapturePayload::Image(ClipboardImageBytes::RegisteredPng(vec![1, 2, 3]))
        );
        assert!(backend.opens.is_empty());
    }

    /// 注册 PNG 不可用时必须保留 DIBV5/DIB 的实际格式身份，不读取文本。
    #[test]
    fn capture_falls_back_to_dibv5_then_dib() {
        for (format, expected) in [
            (
                DibClipboardFormat::DibV5,
                ClipboardImageBytes::DibV5(vec![8, 9]),
            ),
            (
                DibClipboardFormat::Dib,
                ClipboardImageBytes::Dib(vec![8, 9]),
            ),
        ] {
            let mut backend = FakeBackend {
                opens: VecDeque::from([true]),
                sequences: VecDeque::from([32, 32]),
                last_sequence: 32,
                text: Ok(ClipboardPayload::from_text("不应读取")),
                png: Err(ClipboardReadError::RegisteredPngUnavailable),
                dib: Ok(DibClipboardBytes::new(format, vec![8, 9])),
                close_ok: true,
            };
            assert_eq!(
                read_capture_payload_with_backend(&mut backend, Some(32), policy())
                    .expect("DIB 捕获失败"),
                ClipboardCapturePayload::Image(expected)
            );
        }
    }

    /// 所有图片格式不可用时才读取 Unicode 文本，保持原有文本捕获语义。
    #[test]
    fn capture_falls_back_to_unicode_text() {
        let mut backend = FakeBackend {
            opens: VecDeque::from([true]),
            sequences: VecDeque::from([33, 33]),
            last_sequence: 33,
            text: Ok(ClipboardPayload::from_text("文本回退")),
            png: Err(ClipboardReadError::RegisteredPngUnavailable),
            dib: Err(ClipboardReadError::DibUnavailable),
            close_ok: true,
        };
        assert_eq!(
            read_capture_payload_with_backend(&mut backend, Some(33), policy())
                .expect("文本回退失败"),
            ClipboardCapturePayload::Text(ClipboardPayload::from_text("文本回退"))
        );
    }

    /// 已选 PNG 的超限错误不得被 DIB 或文本掩盖，关闭仍必须执行。
    #[test]
    fn selected_png_error_does_not_fall_through() {
        let mut backend = FakeBackend {
            opens: VecDeque::from([true]),
            sequences: VecDeque::from([34, 34]),
            last_sequence: 34,
            text: Ok(ClipboardPayload::from_text("不应回退")),
            png: Ok(vec![0; MAX_PNG_ENCODED_BYTES + 1]),
            dib: Ok(DibClipboardBytes::new(DibClipboardFormat::Dib, vec![1])),
            close_ok: true,
        };
        assert_eq!(
            read_capture_payload_with_backend(&mut backend, Some(34), policy()),
            Err(ClipboardReadError::PngEncodedTooLarge)
        );
        assert!(backend.opens.is_empty());
    }

    /// PNG 格式注册故障不是“不可用”，不得被低优先级 DIB 或文本掩盖。
    #[test]
    fn png_registration_failure_does_not_fall_through() {
        let mut backend = FakeBackend {
            opens: VecDeque::from([true]),
            sequences: VecDeque::from([35, 35]),
            last_sequence: 35,
            text: Ok(ClipboardPayload::from_text("不应回退")),
            png: Err(ClipboardReadError::ClipboardFormatRegistrationFailed),
            dib: Ok(DibClipboardBytes::new(DibClipboardFormat::DibV5, vec![1])),
            close_ok: true,
        };
        assert_eq!(
            read_capture_payload_with_backend(&mut backend, Some(35), policy()),
            Err(ClipboardReadError::ClipboardFormatRegistrationFailed)
        );
    }

    /// 中英文、换行和空内容都必须在关闭剪贴板后仍可作为拥有型 payload 使用。
    #[test]
    fn 读取_unicode_和空文本() {
        let mut backend = FakeBackend {
            opens: VecDeque::from([true]),
            sequences: VecDeque::from([7]),
            last_sequence: 7,
            text: Ok(ClipboardPayload::from_text("中文\nline")),
            png: Err(ClipboardReadError::RegisteredPngUnavailable),
            dib: Err(ClipboardReadError::DibUnavailable),
            close_ok: true,
        };
        let result = read_text_with_backend(&mut backend, Some(7), policy()).expect("读取应成功");
        assert_eq!(result.as_text(), "中文\nline");

        let mut text_units: Vec<u16> = "中文\nline".encode_utf16().collect();
        text_units.push(0);
        assert_eq!(
            parse_utf16_text(&text_units, MAX_TEXT_BYTES)
                .unwrap()
                .as_text(),
            "中文\nline"
        );

        let units = [0_u16];
        assert_eq!(
            parse_utf16_text(&units, MAX_TEXT_BYTES).unwrap().as_text(),
            ""
        );
    }

    /// 剪贴板忙碌时先失败若干次、在预算内成功，不能直接把第一次失败当永久错误。
    #[test]
    fn 剪贴板忙碌会在预算内重试() {
        let mut backend = FakeBackend {
            opens: VecDeque::from([false, false, true]),
            sequences: VecDeque::from([3]),
            last_sequence: 3,
            text: Ok(ClipboardPayload::from_text("ok")),
            png: Err(ClipboardReadError::RegisteredPngUnavailable),
            dib: Err(ClipboardReadError::DibUnavailable),
            close_ok: true,
        };
        let policy = RetryPolicy {
            total_timeout: Duration::from_millis(20),
            retry_interval: Duration::ZERO,
        };
        assert_eq!(
            read_text_with_backend(&mut backend, Some(3), policy)
                .expect("第三次尝试应成功")
                .as_text(),
            "ok"
        );
    }

    /// 始终占用时必须在总时长内返回 OpenTimeout，不得无限循环。
    #[test]
    fn 剪贴板持续占用时有界失败() {
        let mut backend = FakeBackend {
            opens: VecDeque::from([false]),
            sequences: VecDeque::from([1]),
            last_sequence: 1,
            text: Ok(ClipboardPayload::from_text("never")),
            png: Err(ClipboardReadError::RegisteredPngUnavailable),
            dib: Err(ClipboardReadError::DibUnavailable),
            close_ok: true,
        };
        assert_eq!(
            read_text_with_backend(&mut backend, Some(1), policy()),
            Err(ClipboardReadError::OpenTimeout)
        );
    }

    /// 打开前和读取后 sequence 不同，结果必须丢弃而不能把新内容归给旧事件。
    #[test]
    fn sequence_失配时丢弃结果() {
        let mut backend = FakeBackend {
            opens: VecDeque::from([true]),
            sequences: VecDeque::from([8, 9]),
            last_sequence: 8,
            text: Ok(ClipboardPayload::from_text("stale")),
            png: Err(ClipboardReadError::RegisteredPngUnavailable),
            dib: Err(ClipboardReadError::DibUnavailable),
            close_ok: true,
        };
        assert_eq!(
            read_text_with_backend(&mut backend, Some(8), policy()),
            Err(ClipboardReadError::SequenceChanged {
                expected: 8,
                observed: 9
            })
        );
    }

    /// 关闭剪贴板失败时宁可返回错误，也不能把可能仍被占用的句柄当成成功读取。
    #[test]
    fn 关闭失败时返回明确错误() {
        let mut backend = FakeBackend {
            opens: VecDeque::from([true]),
            sequences: VecDeque::from([10]),
            last_sequence: 10,
            text: Ok(ClipboardPayload::from_text("close")),
            png: Err(ClipboardReadError::RegisteredPngUnavailable),
            dib: Err(ClipboardReadError::DibUnavailable),
            close_ok: false,
        };
        assert_eq!(
            read_text_with_backend(&mut backend, Some(10), policy()),
            Err(ClipboardReadError::CloseFailed)
        );
    }

    /// 超过 5 MiB 的 UTF-8 文本必须被拒绝，避免 worker 分配无界正文。
    #[test]
    fn 超过上限的文本被拒绝() {
        let units = vec![b'a' as u16; MAX_TEXT_BYTES / 2 + 2];
        assert!(matches!(
            parse_utf16_text(&units, MAX_TEXT_BYTES),
            Err(ClipboardReadError::MalformedUnicodeText)
        ));

        let mut utf8_units = vec![b'a' as u16; MAX_TEXT_BYTES + 1];
        utf8_units.push(0);
        assert!(matches!(
            parse_utf16_text(&utf8_units, MAX_TEXT_BYTES),
            Err(ClipboardReadError::TextTooLarge)
        ));
    }

    /// UTF-16 使用两个字节存储 ASCII，但正文上限按 UTF-8 字节计算；合法 3 MiB ASCII 不得被误截断。
    #[test]
    fn 三_mib_ascii_正文可以读取() {
        let expected_length = 3 * 1024 * 1024;
        let mut units = vec![b'a' as u16; expected_length];
        units.push(0);
        let payload = parse_utf16_text(&units, MAX_TEXT_BYTES).expect("3 MiB ASCII 应在上限内");
        assert_eq!(payload.as_text().len(), expected_length);
    }

    /// 多字节 Unicode 在增量解码时超过预算应立即失败，不先构造超限 String。
    #[test]
    fn 超限多字节正文提前拒绝() {
        let character_count = MAX_TEXT_BYTES / "中".len() + 1;
        let mut units = Vec::with_capacity(character_count + 1);
        for _ in 0..character_count {
            units.push('中' as u16);
        }
        units.push(0);
        assert!(matches!(
            parse_utf16_text(&units, MAX_TEXT_BYTES),
            Err(ClipboardReadError::TextTooLarge)
        ));
    }

    /// 非法 UTF-16 和缺失终止 NUL 必须返回有限错误，不允许替换字符静默损坏正文。
    #[test]
    fn 畸形_unicode_返回明确错误() {
        assert_eq!(
            parse_utf16_text(&[0xD800, 0], MAX_TEXT_BYTES),
            Err(ClipboardReadError::InvalidUtf16)
        );
        assert_eq!(
            parse_utf16_text(&[b'a' as u16], MAX_TEXT_BYTES),
            Err(ClipboardReadError::MalformedUnicodeText)
        );
    }

    /// 构造专用于注册 PNG 读取协议的假后端，文本结果不会被该路径消费。
    fn png_backend(
        opens: impl Into<VecDeque<bool>>,
        sequences: impl Into<VecDeque<u32>>,
        png: Result<Vec<u8>, ClipboardReadError>,
        close_ok: bool,
    ) -> FakeBackend {
        let sequences = sequences.into();
        let last_sequence = sequences.front().copied().unwrap_or_default();
        FakeBackend {
            opens: opens.into(),
            sequences,
            last_sequence,
            text: Err(ClipboardReadError::UnicodeTextUnavailable),
            png,
            dib: Err(ClipboardReadError::DibUnavailable),
            close_ok,
        }
    }

    /// 成功结果必须是关闭剪贴板后仍然有效的拥有型字节。
    #[test]
    fn 注册_png_字节在关闭后仍可使用() {
        let expected = vec![0x89, b'P', b'N', b'G'];
        let mut backend = png_backend([true], [21], Ok(expected.clone()), true);

        let actual =
            read_registered_png_bytes_with_backend(&mut backend, Some(21), policy()).unwrap();

        assert_eq!(actual, expected);
    }

    /// 打开前和读取后的 sequence 失配都必须拒绝旧事件结果。
    #[test]
    fn 注册_png_sequence_失配时丢弃结果() {
        let mut before = png_backend([true], [31], Ok(vec![1]), true);
        assert_eq!(
            read_registered_png_bytes_with_backend(&mut before, Some(30), policy()),
            Err(ClipboardReadError::SequenceChanged {
                expected: 30,
                observed: 31,
            })
        );

        let mut after = png_backend([true], [40, 41], Ok(vec![1]), true);
        assert_eq!(
            read_registered_png_bytes_with_backend(&mut after, Some(40), policy()),
            Err(ClipboardReadError::SequenceChanged {
                expected: 40,
                observed: 41,
            })
        );
    }

    /// 剪贴板忙碌、格式缺失和编码超限必须返回各自稳定错误。
    #[test]
    fn 注册_png_忙碌缺失和超限均明确失败() {
        let mut busy = png_backend([false], [50], Ok(vec![1]), true);
        assert_eq!(
            read_registered_png_bytes_with_backend(&mut busy, Some(50), policy()),
            Err(ClipboardReadError::OpenTimeout)
        );

        let mut unavailable = png_backend(
            [true],
            [51],
            Err(ClipboardReadError::RegisteredPngUnavailable),
            true,
        );
        assert_eq!(
            read_registered_png_bytes_with_backend(&mut unavailable, Some(51), policy()),
            Err(ClipboardReadError::RegisteredPngUnavailable)
        );

        let mut too_large = png_backend([true], [52], Ok(vec![0; MAX_PNG_ENCODED_BYTES + 1]), true);
        assert_eq!(
            read_registered_png_bytes_with_backend(&mut too_large, Some(52), policy()),
            Err(ClipboardReadError::PngEncodedTooLarge)
        );
    }

    /// 关闭失败必须覆盖读取错误，保持与文本读取相同的所有权优先级。
    #[test]
    fn 注册_png_关闭失败优先返回() {
        let mut backend = png_backend(
            [true],
            [60],
            Err(ClipboardReadError::RegisteredPngUnavailable),
            false,
        );
        assert_eq!(
            read_registered_png_bytes_with_backend(&mut backend, Some(60), policy()),
            Err(ClipboardReadError::CloseFailed)
        );
    }

    /// 构造专用于 DIB 读取协议的假后端，其他格式不会被该路径消费。
    fn dib_backend(
        opens: impl Into<VecDeque<bool>>,
        sequences: impl Into<VecDeque<u32>>,
        dib: Result<DibClipboardBytes, ClipboardReadError>,
        close_ok: bool,
    ) -> FakeBackend {
        let sequences = sequences.into();
        let last_sequence = sequences.front().copied().unwrap_or_default();
        FakeBackend {
            opens: opens.into(),
            sequences,
            last_sequence,
            text: Err(ClipboardReadError::UnicodeTextUnavailable),
            png: Err(ClipboardReadError::RegisteredPngUnavailable),
            dib,
            close_ok,
        }
    }

    /// 格式选择必须只在 V5 缺失时回退 DIB，并在两者缺失时返回空。
    #[test]
    fn dib_格式选择优先_v5_再回退_dib() {
        let mut both_calls = Vec::new();
        let both = select_dib_clipboard_format(|format| {
            both_calls.push(format);
            true
        });
        assert_eq!(both, Some(DibClipboardFormat::DibV5));
        assert_eq!(both_calls, vec![CF_DIBV5_FORMAT]);

        let mut fallback_calls = Vec::new();
        let fallback = select_dib_clipboard_format(|format| {
            fallback_calls.push(format);
            format == CF_DIB_FORMAT
        });
        assert_eq!(fallback, Some(DibClipboardFormat::Dib));
        assert_eq!(fallback_calls, vec![CF_DIBV5_FORMAT, CF_DIB_FORMAT]);
        assert_eq!(select_dib_clipboard_format(|_| false), None);
    }

    /// 成功结果必须保留实际格式，并在关闭剪贴板后继续拥有字节。
    #[test]
    fn dib_字节在关闭后仍保留格式和内容() {
        let expected = vec![40, 0, 0, 0];
        let result = DibClipboardBytes::new(DibClipboardFormat::DibV5, expected.clone());
        let mut backend = dib_backend([true], [70], Ok(result), true);

        let actual = read_dib_bytes_with_backend(&mut backend, Some(70), policy()).unwrap();

        assert_eq!(actual.format(), DibClipboardFormat::DibV5);
        assert_eq!(actual.as_bytes(), expected);
        assert_eq!(actual.into_bytes(), expected);
    }

    /// 打开前和读取后的 sequence 失配都必须拒绝 DIB 结果。
    #[test]
    fn dib_sequence_失配时丢弃结果() {
        let result = || DibClipboardBytes::new(DibClipboardFormat::Dib, vec![1]);
        let mut before = dib_backend([true], [81], Ok(result()), true);
        assert_eq!(
            read_dib_bytes_with_backend(&mut before, Some(80), policy()),
            Err(ClipboardReadError::SequenceChanged {
                expected: 80,
                observed: 81,
            })
        );

        let mut after = dib_backend([true], [90, 91], Ok(result()), true);
        assert_eq!(
            read_dib_bytes_with_backend(&mut after, Some(90), policy()),
            Err(ClipboardReadError::SequenceChanged {
                expected: 90,
                observed: 91,
            })
        );
    }

    /// 忙碌、格式缺失和输入超限必须返回各自稳定错误。
    #[test]
    fn dib_忙碌缺失和超限均明确失败() {
        let result = || DibClipboardBytes::new(DibClipboardFormat::Dib, vec![1]);
        let mut busy = dib_backend([false], [100], Ok(result()), true);
        assert_eq!(
            read_dib_bytes_with_backend(&mut busy, Some(100), policy()),
            Err(ClipboardReadError::OpenTimeout)
        );

        let mut unavailable =
            dib_backend([true], [101], Err(ClipboardReadError::DibUnavailable), true);
        assert_eq!(
            read_dib_bytes_with_backend(&mut unavailable, Some(101), policy()),
            Err(ClipboardReadError::DibUnavailable)
        );

        let oversized = DibClipboardBytes::new(
            DibClipboardFormat::DibV5,
            vec![0; MAX_DIB_ENCODED_BYTES + 1],
        );
        let mut too_large = dib_backend([true], [102], Ok(oversized), true);
        assert_eq!(
            read_dib_bytes_with_backend(&mut too_large, Some(102), policy()),
            Err(ClipboardReadError::DibEncodedTooLarge)
        );
    }

    /// 关闭失败必须覆盖 DIB 读取错误。
    #[test]
    fn dib_关闭失败优先返回() {
        let mut backend = dib_backend(
            [true],
            [110],
            Err(ClipboardReadError::DibUnavailable),
            false,
        );
        assert_eq!(
            read_dib_bytes_with_backend(&mut backend, Some(110), policy()),
            Err(ClipboardReadError::CloseFailed)
        );
    }
}

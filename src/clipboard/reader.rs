//! 此模块定义 ClipboardIO 的读取算法、重试边界和 Win32 `CF_UNICODETEXT` 适配器。
//!
//! 算法先记录 sequence，再在有界时长内打开剪贴板，读取函数必须在返回前复制自有数据，
//! 最后关闭剪贴板并复核 sequence。测试通过 `ClipboardBackend` 注入假后端，不需要修改系统
//! 剪贴板状态；生产适配器只使用有限权限的系统 API，不把 HGLOBAL 句柄泄漏到领域层。

use std::thread;
use std::time::{Duration, Instant};

use crate::domain::ClipboardPayload;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    OpenClipboard,
};
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

/// 文本正文的默认 UTF-8 字节上限；超过上限必须在 worker 内丢弃。
pub const MAX_TEXT_BYTES: usize = 5 * 1024 * 1024;

/// Windows 预定义 Unicode 文本格式编号；windows-sys 不在 DataExchange 模块导出该常量。
const CF_UNICODETEXT_FORMAT: u32 = 13;

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
    /// 剪贴板返回的 HGLOBAL 无法读取。
    GlobalMemoryUnavailable,
    /// 文本缺少终止 NUL，或内存边界在上限前无法确认正文结束。
    MalformedUnicodeText,
    /// UTF-16 数据无法转换为有效 Unicode。
    InvalidUtf16,
    /// 转换后的 UTF-8 正文超过固定上限。
    TextTooLarge,
    /// 读取完成后关闭剪贴板失败；调用方不能继续假设状态一致。
    CloseFailed,
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

#[cfg(test)]
mod tests {
    //! 此测试模块通过假后端验证 Unicode、空内容、重试、sequence 和文本上限边界。

    use super::{
        parse_utf16_text, read_text_with_backend, ClipboardBackend, ClipboardReadError,
        RetryPolicy, MAX_TEXT_BYTES,
    };
    use crate::domain::ClipboardPayload;
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
    }

    fn policy() -> RetryPolicy {
        RetryPolicy {
            total_timeout: Duration::ZERO,
            retry_interval: Duration::ZERO,
        }
    }

    /// 中英文、换行和空内容都必须在关闭剪贴板后仍可作为拥有型 payload 使用。
    #[test]
    fn 读取_unicode_和空文本() {
        let mut backend = FakeBackend {
            opens: VecDeque::from([true]),
            sequences: VecDeque::from([7]),
            last_sequence: 7,
            text: Ok(ClipboardPayload::from_text("中文\nline")),
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
}

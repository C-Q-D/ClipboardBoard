//! 此模块负责把历史文本写回 Windows 剪贴板，并维护一次性的自身写回预期事务。
//!
//! 写入事务先登记内容哈希和格式，再写入 `CF_UNICODETEXT`，成功后绑定精确剪贴板序号。
//! 捕获 worker 使用同一预期存储消费匹配事件，避免应用自身写回再次进入历史。

use std::collections::VecDeque;
use std::fmt::{Display, Formatter};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardSequenceNumber, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

use crate::image_copy::PreparedImageClipboard;

/// Windows 预定义 Unicode 文本格式编号；与读取端保持同一格式契约。
pub const CF_UNICODETEXT_FORMAT: u32 = 13;
/// Windows 预定义 DIBV5 格式编号；内存必须从 `BITMAPV5HEADER` 开始。
pub const CF_DIBV5_FORMAT: u32 = 17;

/// 应用主动写回的系统剪贴板格式身份。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardWriteFormat {
    /// Windows `CF_UNICODETEXT` 格式。
    UnicodeText,
    /// Windows `CF_DIBV5` 格式。
    DibV5,
}

/// 一次自身写回事务的匹配键；sequence 在写入成功后绑定，消费前必须精确匹配。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipboardWriteExpectation {
    /// 用于区分快速连续写回的稳定事务令牌。
    token: ClipboardWriteToken,
    /// 写回正文的规范 BLAKE3 哈希。
    pub content_hash: [u8; 32],
    /// 写回时使用的系统剪贴板格式。
    pub format: ClipboardWriteFormat,
    /// 写入成功后观察到的精确剪贴板序号；绑定前由写入串行锁阻止捕获线程消费。
    pub sequence: Option<u32>,
    /// 登记时间，用于回收 ClipboardIO 永久未返回的异常事务。
    armed_at: Instant,
}

/// 区分快速连续写回事务的令牌；令牌不携带正文或系统句柄。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipboardWriteToken(u64);

/// 同时等待 ClipboardIO 捕获的自身写回数量上限；超限时拒绝写入而不是放弃抑制。
pub const MAX_PENDING_WRITE_EXPECTATIONS: usize = 32;

/// 单次自身写回预期的最长保留时间；超时后按真实用户复制处理，避免后台异常导致永久阻塞。
pub const WRITE_EXPECTATION_TTL: Duration = Duration::from_secs(5);

/// 跨 UI、写回线程和 ClipboardIO worker 共享的有界一次性预期存储。
#[derive(Clone)]
pub struct ClipboardWriteExpectationStore {
    /// 锁内按写入顺序保存多个预期，避免旧捕获被后续应用写回覆盖。
    state: Arc<Mutex<ExpectationState>>,
    /// 写入事务从登记到绑定 sequence 持有该锁，禁止捕获在线程竞态窗口消费未绑定预期。
    write_lock: Arc<Mutex<()>>,
}

/// 预期队列的内部状态；令牌单调递增，回绕时仍只用于当前队列内的身份区分。
#[derive(Default)]
struct ExpectationState {
    /// 尚未消费的自身写回预期，数量受 `MAX_PENDING_WRITE_EXPECTATIONS` 限制。
    pending: VecDeque<ClipboardWriteExpectation>,
    /// 下一个写回事务令牌。
    next_token: u64,
}

impl Default for ClipboardWriteExpectationStore {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(ExpectationState::default())),
            write_lock: Arc::new(Mutex::new(())),
        }
    }
}

impl ClipboardWriteExpectationStore {
    /// 创建空的自身写回预期。
    pub fn new() -> Self {
        Self::default()
    }

    /// 在触碰系统剪贴板前登记哈希和格式；队列满时返回 `None` 并拒绝本次写入。
    pub fn arm(
        &self,
        content_hash: [u8; 32],
        format: ClipboardWriteFormat,
    ) -> Option<ClipboardWriteToken> {
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        let now = Instant::now();
        state.pending.retain(|expectation| {
            now.checked_duration_since(expectation.armed_at)
                .is_some_and(|age| age < WRITE_EXPECTATION_TTL)
        });
        if state.pending.len() >= MAX_PENDING_WRITE_EXPECTATIONS {
            return None;
        }
        let token = ClipboardWriteToken(state.next_token);
        state.next_token = state.next_token.wrapping_add(1);
        state.pending.push_back(ClipboardWriteExpectation {
            token,
            content_hash,
            format,
            sequence: None,
            armed_at: now,
        });
        Some(token)
    }

    /// 写入成功后为对应令牌绑定精确序号；令牌不存在时安全地忽略迟到绑定。
    pub fn bind_sequence(&self, token: ClipboardWriteToken, sequence: u32) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(expectation) = state
                .pending
                .iter_mut()
                .find(|expectation| expectation.token == token)
            {
                expectation.sequence = Some(sequence);
            }
        }
    }

    /// 写入失败时只撤销对应令牌，保留其他尚未消费的自身写回预期。
    pub fn cancel(&self, token: ClipboardWriteToken) {
        if let Ok(mut state) = self.state.lock() {
            state
                .pending
                .retain(|expectation| expectation.token != token);
        }
    }

    /// 串行化一次写回的登记、Win32 调用和 sequence 绑定，消除未绑定窗口的同哈希竞态。
    fn lock_write(&self) -> MutexGuard<'_, ()> {
        self.write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 按精确 sequence、哈希和格式一次性消费匹配事件；超时预期会被淘汰。
    pub fn consume_if_matches(
        &self,
        sequence: u32,
        content_hash: [u8; 32],
        format: ClipboardWriteFormat,
    ) -> bool {
        // 写入线程持有同一把锁直到 sequence 绑定，故这里永远不会消费未绑定预期。
        let _write_guard = self.lock_write();
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let now = Instant::now();
        state.pending.retain(|expectation| {
            now.checked_duration_since(expectation.armed_at)
                .is_some_and(|age| age < WRITE_EXPECTATION_TTL)
        });

        let mut index = 0;
        while index < state.pending.len() {
            let expectation = state.pending[index];
            let Some(expected_sequence) = expectation.sequence else {
                index += 1;
                continue;
            };
            if expected_sequence == sequence
                && expectation.content_hash == content_hash
                && expectation.format == format
            {
                state.pending.remove(index);
                return true;
            }
            // 事件可能乱序到达，不能仅因观察到更晚 sequence 就删除更早的自身预期；
            // 由上面的 TTL 统一回收没有等到精确事件的异常事务。
            index += 1;
        }
        false
    }

    /// 查询是否存在精确 sequence/format 候选，不消费也不暴露哈希或队列内容。
    ///
    /// 图片捕获只有在本方法返回 true 时才需要提前解码以比较规范像素哈希，避免普通
    /// 图片捕获为了抑制检查重复解码。
    pub fn has_candidate(&self, sequence: u32, format: ClipboardWriteFormat) -> bool {
        let _write_guard = self.lock_write();
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let now = Instant::now();
        state.pending.retain(|expectation| {
            now.checked_duration_since(expectation.armed_at)
                .is_some_and(|age| age < WRITE_EXPECTATION_TTL)
        });
        state.pending.iter().any(|expectation| {
            expectation.sequence == Some(sequence) && expectation.format == format
        })
    }
}

/// 写回系统剪贴板时可能发生的有限错误；不把正文或原生句柄带出模块。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardWriteError {
    /// 无法在当前线程打开系统剪贴板。
    OpenFailed,
    /// Windows 无法清空旧剪贴板内容。
    EmptyFailed,
    /// 无法分配可转移所有权的全局内存。
    AllocationFailed,
    /// 无法锁定刚分配的全局内存。
    LockFailed,
    /// Windows 拒绝接收当前系统剪贴板格式句柄。
    SetFailed,
    /// 写入结束后无法关闭系统剪贴板。
    CloseFailed,
    /// 等待 ClipboardIO 消费的自身写回过多，本次拒绝写入以避免无法抑制的历史回捕获。
    ExpectationLimitReached,
}

impl Display for ClipboardWriteError {
    /// 将写回失败转换为不包含剪贴板正文的稳定描述。
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::OpenFailed => "无法打开系统剪贴板",
            Self::EmptyFailed => "无法清空系统剪贴板",
            Self::AllocationFailed => "无法分配剪贴板全局内存",
            Self::LockFailed => "无法锁定剪贴板全局内存",
            Self::SetFailed => "无法写入系统剪贴板格式",
            Self::CloseFailed => "无法关闭系统剪贴板",
            Self::ExpectationLimitReached => "自身剪贴板写回过多，已拒绝本次复制",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ClipboardWriteError {}

/// 负责执行文本或图片写回；预期存储由调用方和捕获 worker 共享。
pub struct ClipboardWriter;

/// `SetClipboardData` 已转移 HGLOBAL 所有权后的写入结果；关闭失败不能撤销已发生的写回。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RawWriteSuccess {
    /// 在仍持有剪贴板打开锁时读取的本次写回事务序号，避免关闭后被外部进程抢先改变。
    sequence: u32,
    /// `CloseClipboard` 是否成功；失败时仍必须保留自身写回预期以抑制后续捕获。
    close_succeeded: bool,
}

impl ClipboardWriter {
    /// 登记预期、写入 `CF_UNICODETEXT` 并绑定成功后的精确序号。
    pub fn write_unicode_text(
        text: &str,
        content_hash: [u8; 32],
        expectations: &ClipboardWriteExpectationStore,
    ) -> Result<u32, ClipboardWriteError> {
        // 整个事务持锁，确保捕获 worker 只能看到已经绑定 sequence 的预期。
        let _write_guard = expectations.lock_write();
        let token = expectations
            .arm(content_hash, ClipboardWriteFormat::UnicodeText)
            .ok_or(ClipboardWriteError::ExpectationLimitReached)?;
        let write_result = match write_unicode_text_raw(text) {
            Ok(result) => result,
            Err(error) => {
                // 只有所有权未转移的失败才撤销预期；已转移的 Close 失败由下方保留预期。
                expectations.cancel(token);
                return Err(error);
            }
        };

        finish_transferred_write(
            expectations,
            token,
            write_result.sequence,
            write_result.close_succeeded,
        )
    }

    /// 登记图片预期、写入 `CF_DIBV5` 并绑定成功后的精确序号。
    pub fn write_dib_v5(
        image: &PreparedImageClipboard,
        expectations: &ClipboardWriteExpectationStore,
    ) -> Result<u32, ClipboardWriteError> {
        let _write_guard = expectations.lock_write();
        let token = expectations
            .arm(*image.content_hash(), ClipboardWriteFormat::DibV5)
            .ok_or(ClipboardWriteError::ExpectationLimitReached)?;
        let write_result = match write_clipboard_bytes_raw(image.dib_v5_bytes(), CF_DIBV5_FORMAT) {
            Ok(result) => result,
            Err(error) => {
                expectations.cancel(token);
                return Err(error);
            }
        };

        finish_transferred_write(
            expectations,
            token,
            write_result.sequence,
            write_result.close_succeeded,
        )
    }
}

/// 将 Rust 字符串编码为以 NUL 结尾的 UTF-16 单元；该纯函数便于验证边界而不触碰系统剪贴板。
fn utf16_with_nul(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 在当前线程执行一次最小 Win32 Unicode 文本写入，并严格处理 HGLOBAL 所有权转移。
fn write_unicode_text_raw(text: &str) -> Result<RawWriteSuccess, ClipboardWriteError> {
    let utf16 = utf16_with_nul(text);
    let byte_count = utf16
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or(ClipboardWriteError::AllocationFailed)?;
    // Windows UTF-16 使用小端字节；切片只在 `utf16` 存活期间交给同步写入函数。
    let bytes = unsafe { std::slice::from_raw_parts(utf16.as_ptr().cast::<u8>(), byte_count) };
    write_clipboard_bytes_raw(bytes, CF_UNICODETEXT_FORMAT)
}

/// 把拥有型调用方字节复制到可转移 HGLOBAL，并执行最小 Win32 剪贴板事务。
fn write_clipboard_bytes_raw(
    bytes: &[u8],
    clipboard_format: u32,
) -> Result<RawWriteSuccess, ClipboardWriteError> {
    write_clipboard_bytes_with_backend(&mut Win32ClipboardBackend, bytes, clipboard_format)
}

/// 把 Win32 剪贴板副作用压缩成可注入边界，便于证明 HGLOBAL 所有权转移。
trait ClipboardWriteBackend {
    /// 后端使用的可复制内存句柄。
    type Memory: Copy;

    /// 分配尚归应用所有的可移动内存。
    fn allocate(&mut self, byte_count: usize) -> Option<Self::Memory>;
    /// 锁定内存、复制全部字节并解锁。
    fn copy_into(&mut self, memory: Self::Memory, bytes: &[u8]) -> Result<(), ClipboardWriteError>;
    /// 释放尚未转移给系统的内存。
    fn free(&mut self, memory: Self::Memory);
    /// 打开当前线程的系统剪贴板事务。
    fn open(&mut self) -> bool;
    /// 清空旧内容。
    fn empty(&mut self) -> bool;
    /// 提交指定格式并在成功时转移内存所有权。
    fn set(&mut self, format: u32, memory: Self::Memory) -> bool;
    /// 在仍持有打开锁时取得剪贴板序号。
    fn sequence(&mut self) -> u32;
    /// 关闭系统剪贴板事务。
    fn close(&mut self) -> bool;
}

/// 使用可注入后端执行所有权状态机；只有 `set` 成功后不再调用 `free`。
fn write_clipboard_bytes_with_backend<B: ClipboardWriteBackend>(
    backend: &mut B,
    bytes: &[u8],
    clipboard_format: u32,
) -> Result<RawWriteSuccess, ClipboardWriteError> {
    let memory = backend
        .allocate(bytes.len())
        .ok_or(ClipboardWriteError::AllocationFailed)?;
    if let Err(error) = backend.copy_into(memory, bytes) {
        backend.free(memory);
        return Err(error);
    }
    if !backend.open() {
        backend.free(memory);
        return Err(ClipboardWriteError::OpenFailed);
    }

    let result = if !backend.empty() {
        Err(ClipboardWriteError::EmptyFailed)
    } else if !backend.set(clipboard_format, memory) {
        Err(ClipboardWriteError::SetFailed)
    } else {
        // 仍持有 OpenClipboard 锁时读取序号；外部进程在 CloseClipboard 前无法替换内容。
        Ok(backend.sequence())
    };
    let close_succeeded = backend.close();

    match result {
        Ok(sequence) => Ok(RawWriteSuccess {
            sequence,
            close_succeeded,
        }),
        Err(error) => {
            backend.free(memory);
            Err(error)
        }
    }
}

/// 生产 Win32 后端；所有 unsafe 调用都限制在本实现内。
struct Win32ClipboardBackend;

impl ClipboardWriteBackend for Win32ClipboardBackend {
    type Memory = HGLOBAL;

    fn allocate(&mut self, byte_count: usize) -> Option<Self::Memory> {
        let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_count) };
        (!memory.is_null()).then_some(memory)
    }

    fn copy_into(&mut self, memory: Self::Memory, bytes: &[u8]) -> Result<(), ClipboardWriteError> {
        let locked = unsafe { GlobalLock(memory) };
        if locked.is_null() {
            return Err(ClipboardWriteError::LockFailed);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), locked.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(memory);
        }
        Ok(())
    }

    fn free(&mut self, memory: Self::Memory) {
        unsafe {
            let _ = GlobalFree(memory);
        }
    }

    fn open(&mut self) -> bool {
        (unsafe { OpenClipboard(std::ptr::null_mut()) }) != 0
    }

    fn empty(&mut self) -> bool {
        (unsafe { EmptyClipboard() }) != 0
    }

    fn set(&mut self, format: u32, memory: Self::Memory) -> bool {
        !unsafe { SetClipboardData(format, memory as HANDLE) }.is_null()
    }

    fn sequence(&mut self) -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }

    fn close(&mut self) -> bool {
        (unsafe { CloseClipboard() }) != 0
    }
}

/// 绑定已发生写回的 sequence；Close 失败只影响返回值，不得撤销自身事件抑制预期。
fn finish_transferred_write(
    expectations: &ClipboardWriteExpectationStore,
    token: ClipboardWriteToken,
    sequence: u32,
    close_succeeded: bool,
) -> Result<u32, ClipboardWriteError> {
    expectations.bind_sequence(token, sequence);
    if close_succeeded {
        Ok(sequence)
    } else {
        Err(ClipboardWriteError::CloseFailed)
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证预期事务的一次性、序号绑定和 UTF-16 终止规则，不改写真实剪贴板。

    use super::{
        finish_transferred_write, utf16_with_nul, write_clipboard_bytes_with_backend,
        ClipboardWriteBackend, ClipboardWriteError, ClipboardWriteExpectationStore,
        ClipboardWriteFormat, RawWriteSuccess, MAX_PENDING_WRITE_EXPECTATIONS,
        WRITE_EXPECTATION_TTL,
    };
    use std::time::{Duration, Instant};

    /// 可注入失败阶段，用于逐分支验证内存释放和所有权转移。
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FailStage {
        /// 分配全局内存失败。
        Allocate,
        /// 锁定或复制全局内存失败。
        Copy,
        /// 打开剪贴板失败。
        Open,
        /// 清空剪贴板失败。
        Empty,
        /// SetClipboardData 失败且所有权未转移。
        Set,
        /// Set 已转移所有权，但关闭剪贴板失败。
        Close,
    }

    /// 记录所有权状态机副作用的纯测试后端，不调用真实 Win32。
    #[derive(Default)]
    struct FakeBackend {
        /// 当前注入的失败阶段；空表示成功。
        fail: Option<FailStage>,
        /// 释放尚未转移内存的次数。
        free_count: usize,
        /// Set 调用次数。
        set_count: usize,
        /// Close 调用次数。
        close_count: usize,
        /// 实际提交的格式。
        format: Option<u32>,
        /// 实际复制的字节。
        copied: Vec<u8>,
    }

    impl FakeBackend {
        /// 创建注入指定失败阶段的后端。
        fn failing(stage: FailStage) -> Self {
            Self {
                fail: Some(stage),
                ..Self::default()
            }
        }
    }

    impl ClipboardWriteBackend for FakeBackend {
        type Memory = usize;

        fn allocate(&mut self, _byte_count: usize) -> Option<Self::Memory> {
            (self.fail != Some(FailStage::Allocate)).then_some(1)
        }

        fn copy_into(
            &mut self,
            _memory: Self::Memory,
            bytes: &[u8],
        ) -> Result<(), ClipboardWriteError> {
            if self.fail == Some(FailStage::Copy) {
                return Err(ClipboardWriteError::LockFailed);
            }
            self.copied.extend_from_slice(bytes);
            Ok(())
        }

        fn free(&mut self, _memory: Self::Memory) {
            self.free_count += 1;
        }

        fn open(&mut self) -> bool {
            self.fail != Some(FailStage::Open)
        }

        fn empty(&mut self) -> bool {
            self.fail != Some(FailStage::Empty)
        }

        fn set(&mut self, format: u32, _memory: Self::Memory) -> bool {
            self.set_count += 1;
            self.format = Some(format);
            self.fail != Some(FailStage::Set)
        }

        fn sequence(&mut self) -> u32 {
            91
        }

        fn close(&mut self) -> bool {
            self.close_count += 1;
            self.fail != Some(FailStage::Close)
        }
    }

    /// 写入成功后必须绑定序号，错误序号和错误哈希都不能消费预期。
    #[test]
    fn 预期事务按哈希格式和序号一次性消费() {
        let store = ClipboardWriteExpectationStore::new();
        let token = store
            .arm([7; 32], ClipboardWriteFormat::UnicodeText)
            .expect("预期队列应接受事务");
        assert!(!store.consume_if_matches(1, [8; 32], ClipboardWriteFormat::UnicodeText));
        store.bind_sequence(token, 9);
        assert!(!store.consume_if_matches(8, [7; 32], ClipboardWriteFormat::UnicodeText));
        assert!(store.consume_if_matches(9, [7; 32], ClipboardWriteFormat::UnicodeText));
        assert!(!store.consume_if_matches(9, [7; 32], ClipboardWriteFormat::UnicodeText));
    }

    /// 图片候选查询必须精确匹配 sequence/format，且查询本身不消费预期。
    #[test]
    fn 图片候选查询不消费且格式精确() {
        let store = ClipboardWriteExpectationStore::new();
        let token = store
            .arm([10; 32], ClipboardWriteFormat::DibV5)
            .expect("图片预期应登记");
        store.bind_sequence(token, 81);

        assert!(!store.has_candidate(80, ClipboardWriteFormat::DibV5));
        assert!(!store.has_candidate(81, ClipboardWriteFormat::UnicodeText));
        assert!(store.has_candidate(81, ClipboardWriteFormat::DibV5));
        assert!(store.has_candidate(81, ClipboardWriteFormat::DibV5));
        assert!(store.consume_if_matches(81, [10; 32], ClipboardWriteFormat::DibV5));
        assert!(!store.has_candidate(81, ClipboardWriteFormat::DibV5));
    }

    /// 相同 sequence 的不同格式和错误哈希都不能删除其他精确预期。
    #[test]
    fn 文本和图片预期按格式独立消费() {
        let store = ClipboardWriteExpectationStore::new();
        let text = store
            .arm([11; 32], ClipboardWriteFormat::UnicodeText)
            .expect("文本预期应登记");
        let image = store
            .arm([12; 32], ClipboardWriteFormat::DibV5)
            .expect("图片预期应登记");
        store.bind_sequence(text, 83);
        store.bind_sequence(image, 83);

        assert!(!store.consume_if_matches(83, [13; 32], ClipboardWriteFormat::DibV5));
        assert!(store.has_candidate(83, ClipboardWriteFormat::DibV5));
        assert!(store.consume_if_matches(83, [12; 32], ClipboardWriteFormat::DibV5));
        assert!(store.consume_if_matches(83, [11; 32], ClipboardWriteFormat::UnicodeText));
    }

    /// Set 前所有失败必须释放一次；Set 成功后即使 Close 失败也不得释放。
    #[test]
    fn 所有权状态机按_set_结果决定释放() {
        let bytes = [1, 2, 3, 4];

        let mut allocation = FakeBackend::failing(FailStage::Allocate);
        assert_eq!(
            write_clipboard_bytes_with_backend(&mut allocation, &bytes, 17),
            Err(ClipboardWriteError::AllocationFailed)
        );
        assert_eq!(allocation.free_count, 0);

        for (stage, expected_error) in [
            (FailStage::Copy, ClipboardWriteError::LockFailed),
            (FailStage::Open, ClipboardWriteError::OpenFailed),
            (FailStage::Empty, ClipboardWriteError::EmptyFailed),
            (FailStage::Set, ClipboardWriteError::SetFailed),
        ] {
            let mut backend = FakeBackend::failing(stage);
            assert_eq!(
                write_clipboard_bytes_with_backend(&mut backend, &bytes, 17),
                Err(expected_error)
            );
            assert_eq!(backend.free_count, 1, "失败阶段 {stage:?}");
        }

        let mut close = FakeBackend::failing(FailStage::Close);
        assert_eq!(
            write_clipboard_bytes_with_backend(&mut close, &bytes, 17),
            Ok(RawWriteSuccess {
                sequence: 91,
                close_succeeded: false,
            })
        );
        assert_eq!(close.free_count, 0);
        assert_eq!(close.close_count, 1);

        let mut success = FakeBackend::default();
        assert_eq!(
            write_clipboard_bytes_with_backend(&mut success, &bytes, 17),
            Ok(RawWriteSuccess {
                sequence: 91,
                close_succeeded: true,
            })
        );
        assert_eq!(success.free_count, 0);
        assert_eq!(success.set_count, 1);
        assert_eq!(success.close_count, 1);
        assert_eq!(success.format, Some(17));
        assert_eq!(success.copied, bytes);
    }

    /// 未绑定阶段不能消费任何事件，避免同哈希的真实用户复制被误吞；绑定后才按序号消费。
    #[test]
    fn 未绑定序号不会消费事件() {
        let store = ClipboardWriteExpectationStore::new();
        let token = store
            .arm([3; 32], ClipboardWriteFormat::UnicodeText)
            .expect("预期队列应接受事务");
        assert!(!store.consume_if_matches(12, [3; 32], ClipboardWriteFormat::UnicodeText));
        store.bind_sequence(token, 12);
        assert!(store.consume_if_matches(12, [3; 32], ClipboardWriteFormat::UnicodeText));
        assert!(!store.consume_if_matches(12, [3; 32], ClipboardWriteFormat::UnicodeText));
    }

    /// SetClipboardData 成功但 CloseClipboard 失败时，返回错误仍必须保留一次性抑制预期。
    #[test]
    fn 关闭失败仍保留已转移写回预期() {
        let store = ClipboardWriteExpectationStore::new();
        let token = store
            .arm([5; 32], ClipboardWriteFormat::UnicodeText)
            .expect("预期队列应接受事务");
        assert_eq!(
            finish_transferred_write(&store, token, 21, false),
            Err(ClipboardWriteError::CloseFailed)
        );
        assert!(store.consume_if_matches(21, [5; 32], ClipboardWriteFormat::UnicodeText));
        assert!(!store.consume_if_matches(21, [5; 32], ClipboardWriteFormat::UnicodeText));
    }

    /// 快速连续写回必须保留不同令牌和 sequence，旧捕获不能因新写回而落入历史。
    #[test]
    fn 连续不同哈希按精确序号分别消费() {
        let store = ClipboardWriteExpectationStore::new();
        let first = store
            .arm([1; 32], ClipboardWriteFormat::UnicodeText)
            .expect("第一条预期应登记");
        let second = store
            .arm([2; 32], ClipboardWriteFormat::UnicodeText)
            .expect("第二条预期应登记");
        store.bind_sequence(first, 30);
        store.bind_sequence(second, 31);

        assert!(store.consume_if_matches(30, [1; 32], ClipboardWriteFormat::UnicodeText));
        assert!(store.consume_if_matches(31, [2; 32], ClipboardWriteFormat::UnicodeText));
        assert!(!store.consume_if_matches(30, [1; 32], ClipboardWriteFormat::UnicodeText));
    }

    /// 同哈希的连续写回也必须由 sequence 区分，不能把真实事件误认为旧事务的重复消费。
    #[test]
    fn 同哈希连续写回按序号分别消费() {
        let store = ClipboardWriteExpectationStore::new();
        let first = store
            .arm([4; 32], ClipboardWriteFormat::UnicodeText)
            .expect("第一条预期应登记");
        let second = store
            .arm([4; 32], ClipboardWriteFormat::UnicodeText)
            .expect("第二条预期应登记");
        store.bind_sequence(first, 40);
        store.bind_sequence(second, 41);

        assert!(store.consume_if_matches(41, [4; 32], ClipboardWriteFormat::UnicodeText));
        assert!(store.consume_if_matches(40, [4; 32], ClipboardWriteFormat::UnicodeText));
    }

    /// 事件乱序时更晚 sequence 不能淘汰尚未到达的旧预期，否则旧自身写回会进入历史。
    #[test]
    fn 更晚序号不淘汰旧预期() {
        let store = ClipboardWriteExpectationStore::new();
        let old = store
            .arm([6; 32], ClipboardWriteFormat::UnicodeText)
            .expect("旧预期应登记");
        store.bind_sequence(old, 50);
        assert!(!store.consume_if_matches(51, [9; 32], ClipboardWriteFormat::UnicodeText));
        assert!(store.consume_if_matches(50, [6; 32], ClipboardWriteFormat::UnicodeText));
    }

    /// ClipboardIO 长时间没有返回时，过期预期必须释放容量，避免永久阻塞后续复制。
    #[test]
    fn 超时预期释放队列容量() {
        let store = ClipboardWriteExpectationStore::new();
        let token = store
            .arm([7; 32], ClipboardWriteFormat::UnicodeText)
            .expect("预期应登记");
        {
            let mut state = store.state.lock().expect("测试应取得预期状态锁");
            let expectation = state
                .pending
                .iter_mut()
                .find(|expectation| expectation.token == token)
                .expect("预期应存在");
            expectation.armed_at = Instant::now() - WRITE_EXPECTATION_TTL - Duration::from_secs(1);
        }
        assert!(!store.consume_if_matches(70, [7; 32], ClipboardWriteFormat::UnicodeText));
        assert!(store
            .arm([8; 32], ClipboardWriteFormat::UnicodeText)
            .is_some());
    }

    /// 预期队列满时必须拒绝新写回，而不是覆盖旧事务后留下无法抑制的自身事件。
    #[test]
    fn 预期队列满时拒绝新增事务() {
        let store = ClipboardWriteExpectationStore::new();
        for index in 0..MAX_PENDING_WRITE_EXPECTATIONS {
            assert!(store
                .arm([index as u8; 32], ClipboardWriteFormat::UnicodeText)
                .is_some());
        }
        assert!(store
            .arm([255; 32], ClipboardWriteFormat::UnicodeText)
            .is_none());
    }

    /// UTF-16 写入缓冲必须保留代理对并且恰好以一个 NUL 终止。
    #[test]
    fn 文本编码为带终止符的_utf16() {
        assert_eq!(utf16_with_nul("A😀"), vec![65, 0xD83D, 0xDE00, 0]);
        assert_eq!(utf16_with_nul(""), vec![0]);
    }
}

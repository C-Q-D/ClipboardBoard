//! 此模块负责把历史文本写回 Windows 剪贴板，并维护一次性的自身写回预期事务。
//!
//! 写入事务先登记内容哈希和格式，再写入 `CF_UNICODETEXT`，成功后绑定精确剪贴板序号。
//! 捕获 worker 使用同一预期存储消费匹配事件，避免 Ctrl+Enter 自身写回再次进入历史。

use std::collections::VecDeque;
use std::fmt::{Display, Formatter};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardSequenceNumber, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

/// Windows 预定义 Unicode 文本格式编号；与读取端保持同一格式契约。
pub const CF_UNICODETEXT_FORMAT: u32 = 13;

/// 当前原子只允许写回 Unicode 文本；图片和富文本由后续原子扩展。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardWriteFormat {
    /// Windows `CF_UNICODETEXT` 格式。
    UnicodeText,
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
    /// 锁内按写入顺序保存多个预期，避免旧捕获被后续 Ctrl+Enter 覆盖。
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
            if expected_sequence == sequence {
                if expectation.content_hash == content_hash && expectation.format == format {
                    state.pending.remove(index);
                    return true;
                }
                // sequence 已经被其他格式/正文占用，不能让这条预期永久阻塞后续事件。
                state.pending.remove(index);
                continue;
            }
            // 事件可能乱序到达，不能仅因观察到更晚 sequence 就删除更早的自身预期；
            // 由上面的 TTL 统一回收没有等到精确事件的异常事务。
            index += 1;
        }
        false
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
    /// Windows 拒绝接收 Unicode 文本句柄。
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
            Self::SetFailed => "无法写入 Unicode 文本格式",
            Self::CloseFailed => "无法关闭系统剪贴板",
            Self::ExpectationLimitReached => "自身剪贴板写回过多，已拒绝本次复制",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ClipboardWriteError {}

/// 负责执行一次 Unicode 文本写回；预期存储由调用方和捕获 worker 共享。
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
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_count) };
    if memory.is_null() {
        return Err(ClipboardWriteError::AllocationFailed);
    }

    let locked = unsafe { GlobalLock(memory) };
    if locked.is_null() {
        unsafe {
            let _ = GlobalFree(memory);
        }
        return Err(ClipboardWriteError::LockFailed);
    }
    unsafe {
        std::ptr::copy_nonoverlapping(utf16.as_ptr().cast::<u8>(), locked.cast::<u8>(), byte_count);
        let _ = GlobalUnlock(memory);
    }

    if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
        unsafe {
            let _ = GlobalFree(memory);
        }
        return Err(ClipboardWriteError::OpenFailed);
    }

    let mut transferred = false;
    let result = if unsafe { EmptyClipboard() } == 0 {
        Err(ClipboardWriteError::EmptyFailed)
    } else if unsafe { SetClipboardData(CF_UNICODETEXT_FORMAT, memory as HANDLE).is_null() } {
        Err(ClipboardWriteError::SetFailed)
    } else {
        transferred = true;
        // 仍持有 OpenClipboard 锁时读取序号；外部进程在 CloseClipboard 前无法替换内容。
        Ok(unsafe { GetClipboardSequenceNumber() })
    };
    let close_succeeded = unsafe { CloseClipboard() } != 0;

    let sequence = match result {
        Ok(sequence) => sequence,
        Err(error) => {
            if !transferred {
                unsafe {
                    let _ = GlobalFree(memory);
                }
            }
            return Err(error);
        }
    };
    if !transferred {
        unsafe {
            let _ = GlobalFree(memory);
        }
        unreachable!("成功结果必须已经转移剪贴板内存所有权");
    }
    if !close_succeeded {
        // SetClipboardData 已成功转移内存，即使关闭失败也不能把这次写回当成未发生。
        return Ok(RawWriteSuccess {
            sequence,
            close_succeeded: false,
        });
    }
    Ok(RawWriteSuccess {
        sequence,
        close_succeeded: true,
    })
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
        finish_transferred_write, utf16_with_nul, ClipboardWriteError,
        ClipboardWriteExpectationStore, ClipboardWriteFormat, MAX_PENDING_WRITE_EXPECTATIONS,
        WRITE_EXPECTATION_TTL,
    };
    use std::time::{Duration, Instant};

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

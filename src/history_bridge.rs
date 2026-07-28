//! 此模块把 Windows 剪贴板捕获结果桥接到 SQLite 文本历史和 UI 事件，并处理按 ID 仅复制。
//!
//! 捕获处理顺序固定为“构造 upsert 输入 → 等待事务结果 → 转换持久化快照 → 投递 UI”，
//! 因此数据库提交失败或 DTO 不可转换时不会产生幽灵卡片；仅复制则先按 ID 读取完整正文，
//! 校验 UI 哈希后登记自身写回预期，避免重放旧选择或把写回事件重新记入历史。

use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    clipboard::{
        ClipboardCaptureResult, ClipboardCopyRequest, ClipboardWriteError,
        ClipboardWriteExpectationStore, ClipboardWriter,
    },
    command::{UiClipboardItem, UiEvent},
    domain::ClipboardPayload,
    storage::{HistoryPayload, StorageError, StorageExecutor, TextUpsertInput, TextUpsertResult},
};

/// 捕获处理结果的泵控制状态；只有 UI 已停止时才允许结束泵线程。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureProcessOutcome {
    /// 持久化成功且 UI sink 接收了事件，泵应继续等待下一条捕获。
    Posted,
    /// UI sink 返回 false，事件循环已经停止，泵应退出并释放执行器。
    UiClosed,
    /// 未来非 UI 捕获类型的保留状态；当前有效文本捕获不得返回该状态。
    Skipped,
}

/// 捕获处理过程中不会携带正文的错误边界。
#[derive(Debug)]
pub enum CaptureProcessError {
    /// SQLite upsert 或执行器生命周期错误；泵记录后继续等待后续捕获。
    Storage(StorageError),
    /// 持久化事务返回了不能安全转换为 UI 卡片的 DTO。
    InvalidPersistedRecord,
}

impl fmt::Display for CaptureProcessError {
    /// 输出不含剪贴板正文的错误描述，供结果泵诊断使用。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "捕获持久化失败：{error}"),
            Self::InvalidPersistedRecord => write!(formatter, "持久化结果无法转换为 UI 卡片"),
        }
    }
}

impl std::error::Error for CaptureProcessError {}

impl From<StorageError> for CaptureProcessError {
    /// 将存储层错误包裹到捕获桥的业务错误边界。
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// 仅复制处理过程中不会携带正文的错误边界。
#[derive(Debug)]
pub enum CopyProcessError {
    /// 按 ID 读取 payload 时发生存储线程错误。
    Storage(StorageError),
    /// 请求的历史 ID 已被删除或不存在。
    NotFound { id: u64 },
    /// payload 不是当前原子支持的 text 类型。
    UnsupportedType,
    /// text 记录缺少完整正文。
    MissingText,
    /// 数据库主键、哈希长度或 CF_UNICODETEXT 可表示性不满足写回契约。
    InvalidPayload,
    /// UI 卡片哈希与按 ID 读取的数据库哈希不一致，拒绝旧选择写回。
    HashMismatch,
    /// Win32 Unicode 文本写回失败。
    Write(ClipboardWriteError),
}

impl fmt::Display for CopyProcessError {
    /// 输出不含剪贴板正文的仅复制失败描述。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "仅复制读取存储失败：{error}"),
            Self::NotFound { id } => write!(formatter, "仅复制目标历史不存在：{id}"),
            Self::UnsupportedType => formatter.write_str("仅复制目标不是文本记录"),
            Self::MissingText => formatter.write_str("仅复制目标缺少文本正文"),
            Self::InvalidPayload => formatter.write_str("仅复制目标 payload 不满足写回契约"),
            Self::HashMismatch => formatter.write_str("仅复制目标哈希已变化"),
            Self::Write(error) => write!(formatter, "仅复制写回失败：{error}"),
        }
    }
}

impl std::error::Error for CopyProcessError {}

impl From<StorageError> for CopyProcessError {
    /// 将存储线程错误包裹到仅复制业务边界。
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<ClipboardWriteError> for CopyProcessError {
    /// 将 Win32 写回错误包裹到仅复制业务边界。
    fn from(error: ClipboardWriteError) -> Self {
        Self::Write(error)
    }
}

/// 将当前系统时间转换为不会溢出 `i64` 的 Unix 毫秒时间戳。
pub fn unix_millis_now() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// 先事务性 upsert，再通过可注入 sink 投递唯一 UI 事件。
pub fn process_capture<F>(
    storage: &mut StorageExecutor,
    capture: ClipboardCaptureResult,
    copied_at: i64,
    emit: F,
) -> Result<CaptureProcessOutcome, CaptureProcessError>
where
    F: FnMut(UiEvent) -> bool,
{
    process_capture_with_upsert(capture, copied_at, |input| storage.upsert_text(input), emit)
}

/// 按 UI 提供的 ID 读取完整文本，登记自身写回预期后写入系统剪贴板。
pub fn process_copy_request(
    storage: &mut StorageExecutor,
    request: ClipboardCopyRequest,
    expectations: &ClipboardWriteExpectationStore,
) -> Result<u32, CopyProcessError> {
    let id = i64::try_from(request.id).map_err(|_| CopyProcessError::InvalidPayload)?;
    let payload = storage
        .get_history_payload(id)?
        .ok_or(CopyProcessError::NotFound { id: request.id })?;
    process_copy_payload(&payload, request.content_hash, |text, content_hash| {
        ClipboardWriter::write_unicode_text(text, content_hash, expectations)
    })
}

/// 校验按 ID 读取的 payload 并调用注入的 writer；抽出纯接缝便于不改写真实剪贴板的测试。
fn process_copy_payload<F>(
    payload: &HistoryPayload,
    expected_hash: [u8; 32],
    write: F,
) -> Result<u32, CopyProcessError>
where
    F: FnOnce(&str, [u8; 32]) -> Result<u32, ClipboardWriteError>,
{
    if payload.id <= 0 || payload.item_type != "text" {
        return Err(if payload.item_type == "text" {
            CopyProcessError::InvalidPayload
        } else {
            CopyProcessError::UnsupportedType
        });
    }
    let text = payload
        .text_content
        .as_deref()
        .ok_or(CopyProcessError::MissingText)?;
    // CF_UNICODETEXT 以第一个 NUL 作为终止符，内部 NUL 无法无损写回；必须在触碰系统
    // 剪贴板前拒绝，避免写回截断正文后生成与历史哈希不同的自身捕获。
    if text.contains('\0') {
        return Err(CopyProcessError::InvalidPayload);
    }
    let content_hash = <[u8; 32]>::try_from(payload.content_hash.as_slice())
        .map_err(|_| CopyProcessError::InvalidPayload)?;
    let recomputed_hash = ClipboardPayload::from_text(text).summary().content_hash;
    if content_hash != expected_hash || content_hash != recomputed_hash {
        return Err(CopyProcessError::HashMismatch);
    }
    write(text, content_hash).map_err(CopyProcessError::Write)
}

/// 将捕获内容转换为 upsert 输入并消费持久化结果；抽出闭包便于注入 DTO 错误测试。
fn process_capture_with_upsert<F, U>(
    capture: ClipboardCaptureResult,
    copied_at: i64,
    mut upsert: U,
    mut emit: F,
) -> Result<CaptureProcessOutcome, CaptureProcessError>
where
    F: FnMut(UiEvent) -> bool,
    U: FnMut(TextUpsertInput) -> Result<TextUpsertResult, StorageError>,
{
    let summary = capture.payload.summary();
    let input = TextUpsertInput {
        content_hash: summary.content_hash,
        text_content: capture.payload.as_text().to_owned(),
        preview_text: summary.preview,
        source_exe: capture
            .source
            .as_ref()
            .map(|source| source.executable.clone()),
        source_app: capture
            .source
            .as_ref()
            .map(|source| source.display_name.clone()),
        copied_at,
    };

    let result = upsert(input)?;
    let item = UiClipboardItem::from_persisted_result(&result)
        .ok_or(CaptureProcessError::InvalidPersistedRecord)?;
    if emit(UiEvent::ClipboardCaptured(item)) {
        Ok(CaptureProcessOutcome::Posted)
    } else {
        Ok(CaptureProcessOutcome::UiClosed)
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块覆盖捕获提交顺序、存储失败续处理、DTO 防伪和 UI sink 生命周期。

    use std::sync::atomic::{AtomicUsize, Ordering};

    use rusqlite::{params, Connection};

    use super::{
        process_capture, process_capture_with_upsert, process_copy_payload, CaptureProcessError,
        CaptureProcessOutcome, CopyProcessError,
    };
    use crate::{
        clipboard::{ClipboardCaptureResult, ClipboardWriteError},
        command::UiEvent,
        domain::ClipboardPayload,
        platform::windows::ProcessSource,
        storage::{HistoryPayload, StorageExecutor, TextUpsertResult},
    };

    /// 创建独立临时目录，避免并行测试共享生产数据库或互相污染触发器。
    fn test_directory(label: &str) -> std::path::PathBuf {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!("clipboard-board-18a-{label}-{id}"));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("创建捕获测试目录失败");
        directory
    }

    /// 构造带来源的文本捕获，统一测试正文、来源和哈希的一致性。
    fn capture(sequence: u32, text: &str) -> ClipboardCaptureResult {
        ClipboardCaptureResult {
            sequence,
            source: Some(ProcessSource {
                executable: "editor.exe".to_owned(),
                display_name: "编辑器".to_owned(),
                process_id: 10,
            }),
            payload: ClipboardPayload::from_text(text),
        }
    }

    /// 成功路径必须先观察数据库记录，再收到带持久化 ID 的 UI 事件。
    #[test]
    fn 成功路径先提交再投递() {
        let directory = test_directory("success");
        let mut storage = StorageExecutor::open_at(&directory).expect("启动测试存储失败");
        let mut events = Vec::new();

        let outcome = process_capture(&mut storage, capture(1, "首条文本"), 123, |event| {
            events.push(event);
            true
        })
        .expect("成功捕获应完成处理");

        assert_eq!(outcome, CaptureProcessOutcome::Posted);
        assert_eq!(events.len(), 1);
        let UiEvent::ClipboardCaptured(item) = &events[0] else {
            panic!("成功捕获必须投递 ClipboardCaptured");
        };
        let payload = storage
            .get_history_payload(item.id as i64)
            .expect("成功 upsert 后应可读取记录")
            .expect("成功 upsert 后 ID 必须存在");
        assert_eq!(payload.text_content.as_deref(), Some("首条文本"));
        assert_eq!(item.id, 1);
        assert_eq!(item.source, "编辑器");
        assert_eq!(item.preview, "首条文本");
        assert_eq!(item.copy_count, 1);
        assert!(!item.is_pinned);
        assert_eq!(item.relative_time, "刚刚");
    }

    /// upsert 错误不能投递 UI；同一个执行器随后仍必须处理新的哈希。
    #[test]
    fn 存储失败后同一执行器继续处理新哈希() {
        let directory = test_directory("storage-error");
        let mut storage = StorageExecutor::open_at(&directory).expect("启动测试存储失败");
        let hash = ClipboardPayload::from_text("旧正文").summary().content_hash;
        {
            let connection = Connection::open(storage.database_path()).expect("打开注入连接失败");
            connection
                .execute(
                    "INSERT INTO clipboard_items (item_type, text_content, preview_text, content_hash, source_exe, source_app, copy_count, is_pinned, created_at, copied_at, last_used_at) VALUES ('text', '旧正文', '旧预览', ?1, 'old.exe', '旧应用', 1, 0, 1, 1, NULL)",
                    params![hash.as_slice()],
                )
                .expect("预置重复记录失败");
            connection
                .execute(
                    "CREATE TRIGGER fail_capture_update BEFORE UPDATE OF copied_at ON clipboard_items BEGIN SELECT RAISE(ABORT, 'capture update blocked'); END",
                    [],
                )
                .expect("创建失败触发器失败");
        }

        let mut events = Vec::new();
        let failed = process_capture(&mut storage, capture(2, "旧正文"), 2, |event| {
            events.push(event);
            true
        });
        assert!(matches!(failed, Err(CaptureProcessError::Storage(_))));
        assert!(events.is_empty());
        assert!(storage.status().is_ok());

        let succeeded = process_capture(&mut storage, capture(3, "新正文"), 3, |event| {
            events.push(event);
            true
        })
        .expect("新哈希应绕过失败触发器");
        assert_eq!(succeeded, CaptureProcessOutcome::Posted);
        assert_eq!(events.len(), 1);
    }

    /// 不可转换的持久化 DTO 必须返回专门错误且不调用 sink。
    #[test]
    fn dto_转换失败不投递事件() {
        let mut events = Vec::new();
        let result = process_capture_with_upsert(
            capture(4, "DTO 错误"),
            4,
            |_input| {
                Ok(TextUpsertResult {
                    id: -1,
                    content_hash: [2; 32],
                    preview_text: "DTO 错误".to_owned(),
                    source_exe: None,
                    source_app: None,
                    copy_count: 1,
                    is_pinned: false,
                    created_at: 4,
                    copied_at: 4,
                    last_used_at: None,
                })
            },
            |event| {
                events.push(event);
                true
            },
        );

        assert!(matches!(
            result,
            Err(CaptureProcessError::InvalidPersistedRecord)
        ));
        assert!(events.is_empty());
    }

    /// sink 返回 false 时必须返回 UiClosed，调用方据此退出泵并释放执行器。
    #[test]
    fn sink_关闭返回_ui_closed() {
        let directory = test_directory("ui-closed");
        let mut storage = StorageExecutor::open_at(&directory).expect("启动测试存储失败");
        let outcome = process_capture(&mut storage, capture(5, "UI 关闭"), 5, |_event| false)
            .expect("sink 关闭不是存储错误");

        assert_eq!(outcome, CaptureProcessOutcome::UiClosed);
    }

    /// 当前输入域只有有效文本，成功处理不得返回未来保留的 Skipped 状态。
    #[test]
    fn 有效文本不会返回_skipped() {
        let directory = test_directory("not-skipped");
        let mut storage = StorageExecutor::open_at(&directory).expect("启动测试存储失败");
        let outcome = process_capture(&mut storage, capture(6, "不可跳过"), 6, |_event| true)
            .expect("有效文本应成功处理");

        assert_eq!(outcome, CaptureProcessOutcome::Posted);
        assert_ne!(outcome, CaptureProcessOutcome::Skipped);
    }

    /// 构造可写回的文本 payload；测试 writer 会记录正文和哈希而不触碰系统剪贴板。
    fn copy_payload(text: &str) -> HistoryPayload {
        HistoryPayload {
            id: 9,
            item_type: "text".to_owned(),
            text_content: Some(text.to_owned()),
            preview_text: text.to_owned(),
            content_hash: ClipboardPayload::from_text(text)
                .summary()
                .content_hash
                .to_vec(),
            source_exe: None,
            source_app: None,
            copy_count: 2,
            is_pinned: false,
            created_at: 1,
            copied_at: 2,
            last_used_at: None,
        }
    }

    /// 仅复制必须把按 ID 读取的完整正文和哈希原样交给 writer，不能使用 UI 预览替代正文。
    #[test]
    fn 仅复制校验后写回完整正文() {
        let payload = copy_payload("完整正文\n第二行");
        let expected_hash = <[u8; 32]>::try_from(payload.content_hash.as_slice()).unwrap();
        let mut observed = None;
        let sequence = process_copy_payload(&payload, expected_hash, |text, hash| {
            observed = Some((text.to_owned(), hash));
            Ok::<u32, ClipboardWriteError>(77)
        })
        .expect("有效文本 payload 应允许写回");

        assert_eq!(sequence, 77);
        assert_eq!(
            observed,
            Some(("完整正文\n第二行".to_owned(), expected_hash))
        );
    }

    /// stale UI 哈希、非 text、缺正文和坏哈希都必须在写回前拒绝。
    #[test]
    fn 仅复制拒绝不一致或不完整_payload() {
        let payload = copy_payload("稳定文本");
        let mut writes = 0;
        let hash = <[u8; 32]>::try_from(payload.content_hash.as_slice()).unwrap();
        assert!(matches!(
            process_copy_payload(&payload, [1; 32], |_, _| {
                writes += 1;
                Ok::<u32, ClipboardWriteError>(1)
            }),
            Err(CopyProcessError::HashMismatch)
        ));

        let mut non_text = payload.clone();
        non_text.item_type = "image".to_owned();
        assert!(matches!(
            process_copy_payload(&non_text, hash, |_, _| {
                writes += 1;
                Ok::<u32, ClipboardWriteError>(1)
            }),
            Err(CopyProcessError::UnsupportedType)
        ));

        let mut missing_text = payload.clone();
        missing_text.text_content = None;
        assert!(matches!(
            process_copy_payload(&missing_text, hash, |_, _| {
                writes += 1;
                Ok::<u32, ClipboardWriteError>(1)
            }),
            Err(CopyProcessError::MissingText)
        ));

        let mut bad_hash = payload;
        bad_hash.content_hash = vec![1, 2, 3];
        assert!(matches!(
            process_copy_payload(&bad_hash, hash, |_, _| {
                writes += 1;
                Ok::<u32, ClipboardWriteError>(1)
            }),
            Err(CopyProcessError::InvalidPayload)
        ));

        // 数据库哈希被篡改为合法长度时，仍必须用完整正文重算并拒绝写回，不能让 UI
        // 同步携带的错误哈希把自身写回事件伪装成真实用户复制。
        let mut tampered_hash = copy_payload("稳定文本");
        tampered_hash.content_hash = vec![9; 32];
        assert!(matches!(
            process_copy_payload(&tampered_hash, [9; 32], |_, _| {
                writes += 1;
                Ok::<u32, ClipboardWriteError>(1)
            }),
            Err(CopyProcessError::HashMismatch)
        ));

        // CF_UNICODETEXT 无法无损表示内部 NUL，必须在打开系统剪贴板前拒绝该 payload。
        let mut embedded_nul = copy_payload("前\0后");
        embedded_nul.content_hash = ClipboardPayload::from_text("前\0后")
            .summary()
            .content_hash
            .to_vec();
        assert!(matches!(
            process_copy_payload(
                &embedded_nul,
                <[u8; 32]>::try_from(embedded_nul.content_hash.as_slice()).unwrap(),
                |_, _| {
                    writes += 1;
                    Ok::<u32, ClipboardWriteError>(1)
                }
            ),
            Err(CopyProcessError::InvalidPayload)
        ));
        assert_eq!(writes, 0);
    }
}

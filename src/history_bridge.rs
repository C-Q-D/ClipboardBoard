//! 此模块把 Windows 剪贴板捕获结果桥接到 SQLite 文本历史和 UI 事件。
//!
//! 处理顺序固定为“构造 upsert 输入 → 等待事务结果 → 转换持久化快照 → 投递 UI”，
//! 因此数据库提交失败或 DTO 不可转换时不会产生幽灵卡片；有效文本捕获不会返回
//! `Skipped`，该状态仅为未来非 UI 输入预留。

use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    clipboard::ClipboardCaptureResult,
    command::{UiClipboardItem, UiEvent},
    storage::{StorageError, StorageExecutor, TextUpsertInput, TextUpsertResult},
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
        process_capture, process_capture_with_upsert, CaptureProcessError, CaptureProcessOutcome,
    };
    use crate::{
        clipboard::ClipboardCaptureResult,
        command::UiEvent,
        domain::ClipboardPayload,
        platform::windows::ProcessSource,
        storage::{StorageExecutor, TextUpsertResult},
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
}

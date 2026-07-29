//! 此模块负责在启动热键和剪贴板监听前从 SQLite 恢复有限的 UI 历史快照。
//!
//! 恢复只读取首页最多 100 条摘要，并按 ID 读取 payload 以验证类型、正文存在性和哈希；
//! 完整正文只在当前函数的短生命周期内用于兼容性校验，不会进入 `UiClipboardItem` 或
//! 长期缓存。任何未知或不兼容记录都会让启动阶段失败，禁止伪造一张不完整卡片。

use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    command::{UiClipboardItem, UiSnapshot},
    storage::{HistoryPayload, HistorySummary, StorageError, StorageExecutor},
};

/// 启动恢复固定读取的摘要上限；该值同时是 UI 内存历史的上限。
pub const STARTUP_HISTORY_LIMIT: u32 = 100;

/// 启动恢复过程中可观察的错误；错误描述不携带剪贴板正文。
#[derive(Debug)]
pub enum StartupRestoreError {
    /// SQLite 查询或执行器生命周期错误。
    Storage(StorageError),
    /// 摘要对应的 payload 在读取期间不存在。
    MissingPayload { id: i64 },
    /// 数据库记录类型不是当前原子支持的 text。
    UnsupportedItemType { id: i64, item_type: String },
    /// text 类型缺少可重新粘贴的正文。
    MissingText { id: i64 },
    /// 摘要与 payload 的主键不一致，不能安全合并。
    MismatchedId { summary_id: i64, payload_id: i64 },
    /// ID 不能安全转换为 UI 的无符号标识。
    InvalidId { id: i64 },
    /// 复制计数不能安全转换为 UI 的无符号计数。
    InvalidCopyCount { id: i64, copy_count: i64 },
    /// payload 哈希不是当前文本域要求的 32 字节 BLAKE3 值。
    InvalidHashLength { id: i64, length: usize },
    /// 摘要哈希与按 ID 读取的 payload 哈希不同，不能安全建立复制身份。
    MismatchedHash { id: i64 },
}

impl fmt::Display for StartupRestoreError {
    /// 将恢复错误格式化为不泄露正文的诊断文本。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "启动恢复存储失败：{error}"),
            Self::MissingPayload { id } => write!(formatter, "历史记录 {id} 缺少 payload"),
            Self::UnsupportedItemType { id, item_type } => {
                write!(formatter, "历史记录 {id} 类型不支持：{item_type}")
            }
            Self::MissingText { id } => write!(formatter, "文本历史记录 {id} 缺少正文"),
            Self::MismatchedId {
                summary_id,
                payload_id,
            } => write!(
                formatter,
                "历史摘要 ID {summary_id} 与 payload ID {payload_id} 不一致"
            ),
            Self::InvalidId { id } => write!(formatter, "历史记录 ID 无法转换：{id}"),
            Self::InvalidCopyCount { id, copy_count } => {
                write!(formatter, "历史记录 {id} 计数无效：{copy_count}")
            }
            Self::InvalidHashLength { id, length } => {
                write!(formatter, "历史记录 {id} 哈希长度无效：{length}")
            }
            Self::MismatchedHash { id } => {
                write!(formatter, "历史记录 {id} 的摘要哈希与 payload 不一致")
            }
        }
    }
}

impl std::error::Error for StartupRestoreError {}

impl From<StorageError> for StartupRestoreError {
    /// 将存储层错误纳入启动恢复错误边界。
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// 在启动阶段读取最多 100 条摘要并转换为单次 `ReplaceSnapshot` 所需的 UI 快照。
pub fn load_startup_snapshot(
    storage: &mut StorageExecutor,
) -> Result<UiSnapshot, StartupRestoreError> {
    let page = storage.query_history_summaries(crate::storage::HistoryQuery {
        item_type: Some("text".to_owned()),
        limit: STARTUP_HISTORY_LIMIT,
        ..crate::storage::HistoryQuery::default()
    })?;
    let now = unix_millis_now();
    let mut items = Vec::with_capacity(page.items.len());

    for summary in page.items {
        let payload = storage
            .get_history_payload(summary.id)?
            .ok_or(StartupRestoreError::MissingPayload { id: summary.id })?;
        items.push(build_ui_item(&summary, &payload, now)?);
    }

    Ok(UiSnapshot {
        items,
        // 首次恢复不擅自改变选择策略；ATOM-19 再定义打开面板后的默认选中项。
        selected_index: None,
    })
}

/// 将摘要和按 ID 取出的 payload 合成为不含正文的 UI 卡片。
fn build_ui_item(
    summary: &HistorySummary,
    payload: &HistoryPayload,
    now: i64,
) -> Result<UiClipboardItem, StartupRestoreError> {
    if summary.id != payload.id {
        return Err(StartupRestoreError::MismatchedId {
            summary_id: summary.id,
            payload_id: payload.id,
        });
    }
    if payload.item_type != "text" {
        return Err(StartupRestoreError::UnsupportedItemType {
            id: summary.id,
            item_type: payload.item_type.clone(),
        });
    }
    // 只检查正文存在性；函数返回后 payload 会被释放，正文不会复制到 UI DTO。
    if payload.text_content.is_none() {
        return Err(StartupRestoreError::MissingText { id: summary.id });
    }

    let id =
        u64::try_from(summary.id).map_err(|_| StartupRestoreError::InvalidId { id: summary.id })?;
    if id == 0 {
        return Err(StartupRestoreError::InvalidId { id: summary.id });
    }
    let copy_count =
        u64::try_from(summary.copy_count).map_err(|_| StartupRestoreError::InvalidCopyCount {
            id: summary.id,
            copy_count: summary.copy_count,
        })?;
    if copy_count == 0 {
        return Err(StartupRestoreError::InvalidCopyCount {
            id: summary.id,
            copy_count: summary.copy_count,
        });
    }
    let content_hash: [u8; 32] = payload.content_hash.as_slice().try_into().map_err(|_| {
        StartupRestoreError::InvalidHashLength {
            id: summary.id,
            length: payload.content_hash.len(),
        }
    })?;
    if content_hash != summary.content_hash {
        return Err(StartupRestoreError::MismatchedHash { id: summary.id });
    }

    let source = summary
        .source_app
        .as_deref()
        .filter(|source| !source.is_empty())
        .or_else(|| {
            summary
                .source_exe
                .as_deref()
                .filter(|source| !source.is_empty())
        })
        .unwrap_or("未知来源")
        .to_owned();

    Ok(UiClipboardItem {
        id,
        preview: summary.preview_text.clone(),
        source,
        relative_time: relative_time(summary.copied_at, now),
        content_hash: summary.content_hash,
        copy_count,
        is_pinned: summary.is_pinned,
    })
}

/// 将复制时间转换为启动恢复列表使用的短相对时间文案。
fn relative_time(copied_at: i64, now: i64) -> String {
    let age = now.saturating_sub(copied_at).max(0) as u64;
    if age < 60_000 {
        return "刚刚".to_owned();
    }
    if age < 3_600_000 {
        return format!("{}分钟前", age / 60_000);
    }
    format!("{}小时前", age / 3_600_000)
}

/// 返回不会溢出 `i64` 的当前 Unix 毫秒时间戳。
fn unix_millis_now() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    //! 此测试模块覆盖启动恢复上限、字段转换、正文隔离和不兼容记录失败路径。

    use std::sync::atomic::{AtomicUsize, Ordering};

    use rusqlite::{params, Connection};

    use super::{load_startup_snapshot, StartupRestoreError, STARTUP_HISTORY_LIMIT};
    use crate::storage::{StorageExecutor, TextUpsertInput};
    use crate::{domain::hash::hash_text, storage::HistoryPayload};

    /// 创建隔离恢复测试目录，避免并行测试读取同一个 SQLite 文件。
    fn test_directory(label: &str) -> std::path::PathBuf {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!("clipboard-board-18b-{label}-{id}"));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("创建恢复测试目录失败");
        directory
    }

    /// 写入唯一文本，返回数据库最终结果以便后续断言 ID 和计数。
    fn insert_text(storage: &mut StorageExecutor, text: &str, copied_at: i64) {
        let hash = hash_text(text);
        storage
            .upsert_text(TextUpsertInput {
                content_hash: hash,
                text_content: text.to_owned(),
                preview_text: text.to_owned(),
                source_exe: Some("editor.exe".to_owned()),
                source_app: Some("编辑器".to_owned()),
                copied_at,
            })
            .expect("预置文本失败");
    }

    /// 恢复摘要必须按时间倒序生成 UI 卡片，且不携带完整正文字段。
    #[test]
    fn 恢复文本摘要并丢弃正文() {
        let directory = test_directory("basic");
        let mut storage = StorageExecutor::open_at(&directory).expect("启动恢复存储失败");
        insert_text(&mut storage, "较早", 10);
        insert_text(&mut storage, "较新", 20);

        let snapshot = load_startup_snapshot(&mut storage).expect("恢复快照失败");
        assert_eq!(snapshot.items.len(), 2);
        assert_eq!(snapshot.selected_index, None);
        assert_eq!(snapshot.items[0].preview, "较新");
        assert_eq!(snapshot.items[0].source, "编辑器");
        assert_eq!(snapshot.items[0].copy_count, 1);
        assert_eq!(snapshot.items[0].content_hash, hash_text("较新"));
    }

    /// 首页读取必须有界；超过 100 条时不得把后续记录带进启动快照。
    #[test]
    fn 恢复快照最多一百条() {
        let directory = test_directory("limit");
        let mut storage = StorageExecutor::open_at(&directory).expect("启动恢复存储失败");
        for index in 0..(STARTUP_HISTORY_LIMIT + 20) {
            insert_text(&mut storage, &format!("文本-{index}"), i64::from(index));
        }

        let snapshot = load_startup_snapshot(&mut storage).expect("恢复快照失败");
        assert_eq!(snapshot.items.len(), STARTUP_HISTORY_LIMIT as usize);
        assert_eq!(snapshot.items[0].preview, "文本-119");
    }

    /// 混合数据库中的图片不能进入文本 UI，但也不能阻止文本启动恢复。
    #[test]
    fn 启动恢复在查询边界忽略非文本记录() {
        let directory = test_directory("unsupported");
        let mut storage = StorageExecutor::open_at(&directory).expect("启动恢复存储失败");
        insert_text(&mut storage, "可恢复文本", 2);
        let connection = Connection::open(storage.database_path()).expect("打开注入连接失败");
        connection
            .execute(
                "INSERT INTO clipboard_items (item_type, text_content, preview_text, content_hash, source_exe, source_app, copy_count, is_pinned, created_at, copied_at, last_used_at) VALUES ('image', NULL, '图片', ?1, NULL, NULL, 1, 0, 1, 1, NULL)",
                params![vec![8_u8; 32]],
            )
            .expect("预置不兼容记录失败");
        drop(connection);

        let snapshot = load_startup_snapshot(&mut storage).expect("混合类型不应阻止文本恢复");
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.items[0].preview, "可恢复文本");
        let connection = Connection::open(storage.database_path()).expect("重新打开混合数据库失败");
        let image_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM clipboard_items WHERE item_type = 'image'",
                [],
                |row| row.get(0),
            )
            .expect("查询保留图片失败");
        assert_eq!(image_count, 1);
    }

    /// 哈希长度不正确时必须拒绝 payload，不能截断或补零制造身份。
    #[test]
    fn 错误哈希长度阻止恢复() {
        let summary = crate::storage::HistorySummary {
            id: 1,
            item_type: "text".to_owned(),
            preview_text: "文本".to_owned(),
            content_hash: [1; 32],
            source_exe: None,
            source_app: None,
            copy_count: 1,
            is_pinned: false,
            created_at: 1,
            copied_at: 1,
            last_used_at: None,
        };
        let payload = HistoryPayload {
            id: 1,
            item_type: "text".to_owned(),
            text_content: Some("正文".to_owned()),
            preview_text: "文本".to_owned(),
            content_hash: vec![1, 2, 3],
            source_exe: None,
            source_app: None,
            copy_count: 1,
            is_pinned: false,
            created_at: 1,
            copied_at: 1,
            last_used_at: None,
        };
        let result = super::build_ui_item(&summary, &payload, 1_000);
        assert!(matches!(
            result,
            Err(StartupRestoreError::InvalidHashLength { length: 3, .. })
        ));
    }

    /// 摘要与 payload 各自长度正确但内容不同，也必须阻止启动恢复。
    #[test]
    fn 摘要与_payload_哈希不一致阻止恢复() {
        let summary = crate::storage::HistorySummary {
            id: 1,
            item_type: "text".to_owned(),
            preview_text: "文本".to_owned(),
            content_hash: [2; 32],
            source_exe: None,
            source_app: None,
            copy_count: 1,
            is_pinned: false,
            created_at: 1,
            copied_at: 1,
            last_used_at: None,
        };
        let payload = HistoryPayload {
            id: 1,
            item_type: "text".to_owned(),
            text_content: Some("正文".to_owned()),
            preview_text: "文本".to_owned(),
            content_hash: vec![1; 32],
            source_exe: None,
            source_app: None,
            copy_count: 1,
            is_pinned: false,
            created_at: 1,
            copied_at: 1,
            last_used_at: None,
        };

        assert!(matches!(
            super::build_ui_item(&summary, &payload, 1_000),
            Err(StartupRestoreError::MismatchedHash { id: 1 })
        ));
    }
}

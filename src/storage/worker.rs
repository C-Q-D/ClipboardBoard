//! 此模块实现唯一 SQLite 连接所有者 `StorageExecutor` 及其有界命令协议。
//!
//! 连接只在线程闭包内部创建、迁移和使用；公共 API 只返回不可变状态，不暴露 rusqlite 连接。

use std::{
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{sync_channel, Receiver, SyncSender},
    thread::{self, JoinHandle, ThreadId},
    time::Duration,
};

use rusqlite::{params, Connection, OptionalExtension, Row, Statement};

use super::{migration, StorageError};

/// 存储线程命令队列的容量；探针、upsert、查询和关闭命令共用有界队列，避免无限堆积。
const COMMAND_QUEUE_CAPACITY: usize = 4;

/// 单次摘要查询允许返回的最大记录数，防止调用方绕过虚拟列表加载无界数据。
const MAX_HISTORY_PAGE_SIZE: u32 = 100;

/// 文本历史 upsert 的固定 SQL；重复记录只更新最近复制时间和饱和计数。
const UPSERT_TEXT_SQL: &str = r#"
INSERT INTO clipboard_items
    (item_type, text_content, preview_text, content_hash, source_exe, source_app,
     copy_count, is_pinned, created_at, copied_at, last_used_at)
VALUES ('text', ?1, ?2, ?3, ?4, ?5, 1, 0, ?6, ?6, NULL)
ON CONFLICT(content_hash) DO UPDATE SET
    copied_at = excluded.copied_at,
    copy_count = CASE
        WHEN copy_count >= 9223372036854775807 THEN 9223372036854775807
        WHEN copy_count <= 0 THEN 2
        ELSE copy_count + 1
    END
"#;

/// 带关键词、来源、类型和收藏筛选的摘要查询；所有筛选参数都通过绑定值传入。
///
/// `?1` 和 `?2` 是已经转义并包裹 `%` 的 LIKE 模式；游标条件位于筛选条件之后，
/// 因而下一页只会在同一筛选集合中继续，不会因为未匹配记录改变分页边界。
const HISTORY_QUERY_SQL: &str = r#"
SELECT id, item_type, preview_text, source_exe, source_app, copy_count,
       is_pinned, created_at, copied_at, last_used_at
FROM clipboard_items
WHERE (?1 IS NULL OR text_content LIKE ?1 ESCAPE '\'
                    OR preview_text LIKE ?1 ESCAPE '\')
  AND (?2 IS NULL OR source_app LIKE ?2 ESCAPE '\'
                   OR source_exe LIKE ?2 ESCAPE '\')
  AND (?3 IS NULL OR item_type = ?3)
  AND (?4 IS NULL OR is_pinned = ?4)
  AND (?5 IS NULL OR copied_at < ?5
                  OR (copied_at = ?5 AND id < ?6))
ORDER BY copied_at DESC, id DESC
LIMIT ?7
"#;

/// 按主键读取完整 payload；可空正文和原始哈希字节保持数据库语义。
const HISTORY_PAYLOAD_SQL: &str = r#"
SELECT id, item_type, text_content, preview_text, content_hash,
       source_exe, source_app, copy_count, is_pinned, created_at,
       copied_at, last_used_at
FROM clipboard_items
WHERE id = ?1
"#;

/// 返回给调用方的只读存储状态和真实连接线程探针。
#[derive(Debug)]
pub struct StorageStatus {
    /// 已经提交并通过校验的 schema 版本。
    pub schema_version: i64,
    /// 承载命令循环的线程 ID。
    pub worker_thread_id: ThreadId,
    /// `Connection::open` 返回后记录的连接所有者线程 ID。
    pub connection_thread_id: ThreadId,
    /// 执行 `SELECT 1` 探针时的线程 ID。
    pub probe_thread_id: ThreadId,
    /// 实际连接执行 `SELECT 1` 返回的值，必须为 1。
    pub probe_result: i64,
    /// v1 表中现有记录数，用于验证迁移不会清除预置数据。
    pub clipboard_item_count: i64,
}

/// 文本历史写入的最小输入；调用方只提交已经完成哈希和预览裁剪的值。
#[derive(Debug, Eq, PartialEq)]
pub struct TextUpsertInput {
    /// 文本内容的固定 BLAKE3 哈希，作为历史记录的唯一键。
    pub content_hash: [u8; 32],
    /// 必须原样保存、供后续写回剪贴板的完整文本。
    pub text_content: String,
    /// 列表卡片使用的短预览，不参与去重判定。
    pub preview_text: String,
    /// 复制发生时的源可执行文件名；未知时为空。
    pub source_exe: Option<String>,
    /// 复制发生时的源应用显示名；未知时为空。
    pub source_app: Option<String>,
    /// 复制事件发生的 Unix 毫秒时间戳。
    pub copied_at: i64,
}

/// 文本历史事务写入后返回的稳定快照，不暴露 SQLite 连接或游标。
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TextUpsertResult {
    /// 历史记录的数据库主键。
    pub id: i64,
    /// 本次写入使用的内容哈希。
    pub content_hash: [u8; 32],
    /// 数据库中最终保留的预览文本。
    pub preview_text: String,
    /// 数据库中最终保留的源可执行文件名。
    pub source_exe: Option<String>,
    /// 数据库中最终保留的源应用显示名。
    pub source_app: Option<String>,
    /// 数据库中最终保留的复制次数，重复写入时饱和递增。
    pub copy_count: i64,
    /// 数据库中最终保留的收藏状态。
    pub is_pinned: bool,
    /// 首次创建时间；重复写入不得改变它。
    pub created_at: i64,
    /// 最近一次复制时间。
    pub copied_at: i64,
    /// 最近一次被用户使用的时间；写入操作不得覆盖它。
    pub last_used_at: Option<i64>,
}

/// 稳定分页游标；同一毫秒内用自增 ID 作为第二排序键。
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct HistoryCursor {
    /// 游标锚点的最近复制时间。
    pub copied_at: i64,
    /// 游标锚点的数据库 ID。
    pub id: i64,
}

/// 历史摘要查询的拥有型筛选和分页请求；不携带 SQLite 连接或借用字符串。
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct HistoryQuery {
    /// 在正文或预览中执行包含匹配；空字符串等同于未提供。
    pub keyword: Option<String>,
    /// 在来源应用名或可执行文件名中执行包含匹配；空字符串等同于未提供。
    pub source: Option<String>,
    /// 按 `item_type` 精确筛选；空字符串等同于未提供。
    pub item_type: Option<String>,
    /// 按收藏状态精确筛选；`None` 表示全部状态。
    pub is_pinned: Option<bool>,
    /// 同一筛选集合内的复合游标；`None` 表示从最新记录开始。
    pub cursor: Option<HistoryCursor>,
    /// 本次最多返回的摘要数量；仍受存储层固定上限约束。
    pub limit: u32,
}

/// 历史列表摘要；不携带完整正文，适合放入有界 UI 模型。
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HistorySummary {
    /// 历史记录数据库 ID。
    pub id: i64,
    /// 内容类型，例如 v1 文本记录的 `text`。
    pub item_type: String,
    /// 列表卡片使用的预览文本。
    pub preview_text: String,
    /// 来源可执行文件名；数据库允许为空。
    pub source_exe: Option<String>,
    /// 来源应用显示名；数据库允许为空。
    pub source_app: Option<String>,
    /// 复制次数。
    pub copy_count: i64,
    /// 是否已收藏。
    pub is_pinned: bool,
    /// 首次创建时间。
    pub created_at: i64,
    /// 最近复制时间。
    pub copied_at: i64,
    /// 最近使用时间；从未使用时为空。
    pub last_used_at: Option<i64>,
}

/// 一页历史摘要及其下一页游标。
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HistoryPage {
    /// 本页实际返回的摘要，最多等于调用方请求的 limit。
    pub items: Vec<HistorySummary>,
    /// 仅在数据库还有未返回记录时指向本页最后一条摘要。
    pub next_cursor: Option<HistoryCursor>,
}

/// 按 ID 返回的完整历史 payload；字段类型与 v1 表的可空语义一致。
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HistoryPayload {
    /// 历史记录数据库 ID。
    pub id: i64,
    /// 内容类型，例如 `text`。
    pub item_type: String,
    /// 完整正文；图片或未来无正文类型可以为空。
    pub text_content: Option<String>,
    /// 列表预览文本。
    pub preview_text: String,
    /// 数据库中原样保存的内容哈希字节。
    pub content_hash: Vec<u8>,
    /// 来源可执行文件名；数据库允许为空。
    pub source_exe: Option<String>,
    /// 来源应用显示名；数据库允许为空。
    pub source_app: Option<String>,
    /// 复制次数。
    pub copy_count: i64,
    /// 是否已收藏。
    pub is_pinned: bool,
    /// 首次创建时间。
    pub created_at: i64,
    /// 最近复制时间。
    pub copied_at: i64,
    /// 最近使用时间；从未使用时为空。
    pub last_used_at: Option<i64>,
}

/// 可发送给存储线程的内部命令；不对外暴露连接、Statement 或 SQL 句柄。
enum StorageCommand {
    /// 在实际连接上执行只读线程归属探针。
    Inspect {
        /// 返回探针结果的有界通道。
        reply: SyncSender<Result<StorageStatus, StorageError>>,
    },
    /// 在 worker 的唯一连接上原子插入或更新一条文本历史。
    UpsertText {
        /// 已校验的文本写入输入；完整文本只在 worker 内部使用。
        input: TextUpsertInput,
        /// 返回同一事务中读取的最终稳定快照。
        reply: SyncSender<Result<TextUpsertResult, StorageError>>,
    },
    /// 在 worker 的唯一连接上读取一页历史摘要。
    ListHistory {
        /// 已通过页大小校验的筛选和分页请求。
        query: HistoryQuery,
        /// 返回摘要页和下一页游标。
        reply: SyncSender<Result<HistoryPage, StorageError>>,
    },
    /// 在 worker 的唯一连接上按 ID 读取完整 payload。
    GetHistoryPayload {
        /// 目标历史记录 ID。
        id: i64,
        /// 返回 payload 或不存在标记。
        reply: SyncSender<Result<Option<HistoryPayload>, StorageError>>,
    },
    /// 请求 worker 先关闭连接，再结束命令循环。
    Shutdown {
        /// 返回关闭命令是否已经被 worker 接收。
        reply: SyncSender<Result<(), StorageError>>,
    },
}

/// 在线程内部持有 SQLite 连接和迁移后的不可变元数据。
struct StorageState {
    /// 唯一 SQLite 连接；该字段不会离开存储线程闭包。
    connection: Connection,
    /// 创建连接时的线程 ID，用于和真实查询线程比较。
    connection_thread_id: ThreadId,
    /// 迁移提交后固定的 schema 版本。
    schema_version: i64,
}

/// 本地 SQLite 单线程执行器；实例不可 Clone，连接所有权始终留在 worker。
pub struct StorageExecutor {
    /// 唯一命令发送端；不复制该发送端，避免绕过生命周期管理。
    command_sender: SyncSender<StorageCommand>,
    /// worker 的 RAII 句柄；关闭时必须先发送 Shutdown 再 join。
    worker: Option<JoinHandle<()>>,
    /// 解析后的数据库路径，仅用于诊断和测试，不代表打开了第二个连接。
    database_path: PathBuf,
}

impl StorageExecutor {
    /// 按默认 `%LOCALAPPDATA%\ClipboardBoard\data\clipboard.db` 路径启动并等待迁移就绪。
    pub fn open() -> Result<Self, StorageError> {
        let data_directory = super::default_data_directory()?;
        Self::open_at(data_directory)
    }

    /// 在调用方指定的数据目录启动执行器；测试用临时目录，生产调用方使用默认路径。
    pub fn open_at(data_directory: impl AsRef<Path>) -> Result<Self, StorageError> {
        let data_directory = data_directory.as_ref().to_path_buf();
        let database_path = data_directory.join("clipboard.db");
        let (command_sender, command_receiver) = sync_channel(COMMAND_QUEUE_CAPACITY);
        let (ready_sender, ready_receiver) = sync_channel(1);
        let worker_database_path = database_path.clone();

        let worker = thread::Builder::new()
            .name("clipboard-board-storage".to_owned())
            .spawn(move || storage_thread(worker_database_path, command_receiver, ready_sender))?;

        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                command_sender,
                worker: Some(worker),
                database_path,
            }),
            Ok(Err(error)) => {
                if worker.join().is_err() {
                    Err(StorageError::WorkerPanicked)
                } else {
                    Err(error)
                }
            }
            Err(_) => {
                let join_result = worker.join();
                if join_result.is_err() {
                    Err(StorageError::WorkerPanicked)
                } else {
                    Err(StorageError::InitializationChannelClosed)
                }
            }
        }
    }

    /// 返回当前数据库路径，不访问 SQLite，也不创建新的连接。
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// 在存储线程的实际连接上执行只读探针并等待结果。
    pub fn status(&mut self) -> Result<StorageStatus, StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        if self
            .command_sender
            .send(StorageCommand::Inspect {
                reply: reply_sender,
            })
            .is_err()
        {
            return Err(self.worker_failure_error());
        }

        match reply_receiver.recv() {
            Ok(result) => result,
            Err(_) => Err(self.worker_failure_error()),
        }
    }

    /// 在 worker 的实际连接上执行文本历史的事务性插入或去重更新。
    pub fn upsert_text(
        &mut self,
        input: TextUpsertInput,
    ) -> Result<TextUpsertResult, StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        if self
            .command_sender
            .send(StorageCommand::UpsertText {
                input,
                reply: reply_sender,
            })
            .is_err()
        {
            return Err(self.worker_failure_error());
        }

        match reply_receiver.recv() {
            Ok(result) => result,
            Err(_) => Err(self.worker_failure_error()),
        }
    }

    /// 使用复合游标读取一页历史摘要；页大小在发送命令前固定校验。
    pub fn list_history_summaries(
        &mut self,
        cursor: Option<HistoryCursor>,
        limit: u32,
    ) -> Result<HistoryPage, StorageError> {
        self.query_history_summaries(HistoryQuery {
            cursor,
            limit,
            ..HistoryQuery::default()
        })
    }

    /// 使用关键词、来源、类型、收藏和复合游标查询摘要；正文始终留在 SQLite worker 内。
    pub fn query_history_summaries(
        &mut self,
        query: HistoryQuery,
    ) -> Result<HistoryPage, StorageError> {
        let limit = query.limit;
        if limit > MAX_HISTORY_PAGE_SIZE {
            return Err(StorageError::InvalidPageSize {
                requested: limit,
                max: MAX_HISTORY_PAGE_SIZE,
            });
        }

        let (reply_sender, reply_receiver) = sync_channel(1);
        if self
            .command_sender
            .send(StorageCommand::ListHistory {
                query,
                reply: reply_sender,
            })
            .is_err()
        {
            return Err(self.worker_failure_error());
        }

        match reply_receiver.recv() {
            Ok(result) => result,
            Err(_) => Err(self.worker_failure_error()),
        }
    }

    /// 按 ID 读取完整历史 payload；找不到记录时返回 `Ok(None)`。
    pub fn get_history_payload(&mut self, id: i64) -> Result<Option<HistoryPayload>, StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        if self
            .command_sender
            .send(StorageCommand::GetHistoryPayload {
                id,
                reply: reply_sender,
            })
            .is_err()
        {
            return Err(self.worker_failure_error());
        }

        match reply_receiver.recv() {
            Ok(result) => result,
            Err(_) => Err(self.worker_failure_error()),
        }
    }

    /// 请求 worker 关闭连接并等待线程结束；消费 self 后无法再次发送命令。
    pub fn shutdown(mut self) -> Result<(), StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        let command_result = match self.command_sender.send(StorageCommand::Shutdown {
            reply: reply_sender,
        }) {
            Ok(()) => reply_receiver
                .recv()
                .unwrap_or(Err(StorageError::ChannelClosed)),
            Err(_) => Err(StorageError::ChannelClosed),
        };
        let join_result = self.join_worker();

        match (command_result, join_result) {
            (_, Err(error)) => Err(error),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    /// 回收 worker 句柄并将 panic 映射为可观察的存储错误。
    fn join_worker(&mut self) -> Result<(), StorageError> {
        self.worker
            .take()
            .ok_or(StorageError::WorkerPanicked)?
            .join()
            .map_err(|_| StorageError::WorkerPanicked)
    }

    /// 将命令或回执通道断开统一映射为可诊断的线程生命周期错误。
    fn worker_failure_error(&mut self) -> StorageError {
        if self.join_worker().is_err() {
            StorageError::WorkerPanicked
        } else {
            StorageError::ChannelClosed
        }
    }
}

impl Drop for StorageExecutor {
    /// 在异常路径也先通知 worker 关闭，再等待其释放 SQLite 连接。
    fn drop(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };

        let (reply_sender, reply_receiver) = sync_channel(1);
        if self
            .command_sender
            .send(StorageCommand::Shutdown {
                reply: reply_sender,
            })
            .is_ok()
        {
            let _ = reply_receiver.recv();
        }
        let _ = worker.join();
    }
}

/// 存储线程启动入口：目录创建、连接打开和迁移成功后才通知调用方 ready。
fn storage_thread(
    database_path: PathBuf,
    command_receiver: Receiver<StorageCommand>,
    ready_sender: SyncSender<Result<(), StorageError>>,
) {
    let connection_thread_id = thread::current().id();
    let mut state = match initialize_connection(&database_path, connection_thread_id) {
        Ok(state) => state,
        Err(error) => {
            let _ = ready_sender.send(Err(error));
            return;
        }
    };

    if ready_sender.send(Ok(())).is_err() {
        return;
    }

    while let Ok(command) = command_receiver.recv() {
        match command {
            StorageCommand::Inspect { reply } => {
                let _ = reply.send(inspect_state(&mut state));
            }
            StorageCommand::UpsertText { input, reply } => {
                let _ = reply.send(upsert_text(&mut state.connection, input));
            }
            StorageCommand::ListHistory { query, reply } => {
                let _ = reply.send(query_history_summaries(&state.connection, query));
            }
            StorageCommand::GetHistoryPayload { id, reply } => {
                let _ = reply.send(get_history_payload(&state.connection, id));
            }
            StorageCommand::Shutdown { reply } => {
                let _ = reply.send(Ok(()));
                break;
            }
        }
    }
}

/// 在线程内部创建目录和唯一连接，并在 ready 之前提交完整迁移。
fn initialize_connection(
    database_path: &Path,
    connection_thread_id: ThreadId,
) -> Result<StorageState, StorageError> {
    if let Some(parent) = database_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut connection = Connection::open(database_path)?;
    connection.busy_timeout(Duration::from_millis(250))?;
    let schema_version = migration::migrate(&mut connection)?;

    Ok(StorageState {
        connection,
        connection_thread_id,
        schema_version,
    })
}

/// 在实际连接上执行 `SELECT 1` 和记录数查询，绑定线程证据与连接所有权。
fn inspect_state(state: &mut StorageState) -> Result<StorageStatus, StorageError> {
    let probe_thread_id = thread::current().id();
    let probe_result = state
        .connection
        .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))?;
    let clipboard_item_count =
        state
            .connection
            .query_row("SELECT COUNT(*) FROM clipboard_items", [], |row| {
                row.get::<_, i64>(0)
            })?;

    Ok(StorageStatus {
        schema_version: state.schema_version,
        worker_thread_id: probe_thread_id,
        connection_thread_id: state.connection_thread_id,
        probe_thread_id,
        probe_result,
        clipboard_item_count,
    })
}

/// 使用同一 SQLite 事务完成文本历史 upsert，并在提交前读取最终快照。
fn upsert_text(
    connection: &mut Connection,
    input: TextUpsertInput,
) -> Result<TextUpsertResult, StorageError> {
    let TextUpsertInput {
        content_hash,
        text_content,
        preview_text,
        source_exe,
        source_app,
        copied_at,
    } = input;
    let content_hash_blob = content_hash.to_vec();
    let transaction = connection.transaction()?;

    transaction.execute(
        UPSERT_TEXT_SQL,
        params![
            &text_content,
            &preview_text,
            content_hash_blob.as_slice(),
            &source_exe,
            &source_app,
            copied_at,
        ],
    )?;

    let result = transaction.query_row(
        r#"
SELECT id, preview_text, source_exe, source_app, copy_count, is_pinned,
       created_at, copied_at, last_used_at
FROM clipboard_items
WHERE content_hash = ?1
"#,
        params![content_hash_blob.as_slice()],
        |row| {
            Ok(TextUpsertResult {
                id: row.get(0)?,
                content_hash,
                preview_text: row.get(1)?,
                source_exe: row.get(2)?,
                source_app: row.get(3)?,
                copy_count: row.get(4)?,
                is_pinned: row.get::<_, i64>(5)? != 0,
                created_at: row.get(6)?,
                copied_at: row.get(7)?,
                last_used_at: row.get(8)?,
            })
        },
    )?;

    transaction.commit()?;
    Ok(result)
}

/// 从 worker 的唯一连接执行筛选摘要查询，并用多取一行决定 next_cursor。
fn query_history_summaries(
    connection: &Connection,
    query: HistoryQuery,
) -> Result<HistoryPage, StorageError> {
    let HistoryQuery {
        keyword,
        source,
        item_type,
        is_pinned,
        cursor,
        limit,
    } = query;
    if limit == 0 {
        return Ok(HistoryPage {
            items: Vec::new(),
            next_cursor: None,
        });
    }

    let query_limit = i64::from(limit) + 1;
    let keyword_pattern = contains_like_pattern(keyword.as_deref());
    let source_pattern = contains_like_pattern(source.as_deref());
    let item_type = non_empty_filter(item_type.as_deref());
    let pinned_value = is_pinned.map(|value| if value { 1_i64 } else { 0_i64 });
    let cursor_time = cursor.map(|value| value.copied_at);
    let cursor_id = cursor.map(|value| value.id);

    let mut statement = connection.prepare(HISTORY_QUERY_SQL)?;
    let mut summaries = collect_history_summaries(
        &mut statement,
        params![
            keyword_pattern.as_deref(),
            source_pattern.as_deref(),
            item_type,
            pinned_value,
            cursor_time,
            cursor_id,
            query_limit,
        ],
    )?;

    let has_more = summaries.len() > limit as usize;
    if has_more {
        summaries.truncate(limit as usize);
    }
    let next_cursor = if has_more {
        summaries.last().map(|summary| HistoryCursor {
            copied_at: summary.copied_at,
            id: summary.id,
        })
    } else {
        None
    };

    Ok(HistoryPage {
        items: summaries,
        next_cursor,
    })
}

/// 将用户输入转换为包含匹配模式；空字符串不生成无意义的 `%%` 条件。
fn contains_like_pattern(value: Option<&str>) -> Option<String> {
    let value = non_empty_filter(value)?;
    let escaped = escape_like_pattern(value);
    Some(format!("%{escaped}%"))
}

/// 将空字符串统一当作未提供筛选，同时保留用户输入中的空白和符号。
fn non_empty_filter(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

/// 按 SQLite ESCAPE 约定转义反斜杠、百分号和下划线，防止用户输入改变通配语义。
fn escape_like_pattern(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// 将 SQLite 行映射为不携带正文的历史摘要，并集中维护列顺序。
fn history_summary_from_row(row: &Row<'_>) -> rusqlite::Result<HistorySummary> {
    Ok(HistorySummary {
        id: row.get(0)?,
        item_type: row.get(1)?,
        preview_text: row.get(2)?,
        source_exe: row.get(3)?,
        source_app: row.get(4)?,
        copy_count: row.get(5)?,
        is_pinned: row.get::<_, i64>(6)? != 0,
        created_at: row.get(7)?,
        copied_at: row.get(8)?,
        last_used_at: row.get(9)?,
    })
}

/// 使用泛型参数承接首页和后续页两种绑定，避免复制行映射逻辑。
fn collect_history_summaries<P>(
    statement: &mut Statement<'_>,
    parameters: P,
) -> Result<Vec<HistorySummary>, StorageError>
where
    P: rusqlite::Params,
{
    let rows = statement.query_map(parameters, history_summary_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// 按 ID 读取完整 payload；OptionalExtension 将不存在映射为稳定的 None。
fn get_history_payload(
    connection: &Connection,
    id: i64,
) -> Result<Option<HistoryPayload>, StorageError> {
    let payload = connection
        .query_row(HISTORY_PAYLOAD_SQL, params![id], |row| {
            Ok(HistoryPayload {
                id: row.get(0)?,
                item_type: row.get(1)?,
                text_content: row.get(2)?,
                preview_text: row.get(3)?,
                content_hash: row.get(4)?,
                source_exe: row.get(5)?,
                source_app: row.get(6)?,
                copy_count: row.get(7)?,
                is_pinned: row.get::<_, i64>(8)? != 0,
                created_at: row.get(9)?,
                copied_at: row.get(10)?,
                last_used_at: row.get(11)?,
            })
        })
        .optional()?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证线程执行器、文本事务规则、游标查询、迁移隔离和错误回滚语义。

    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        thread,
    };

    use rusqlite::{params, Connection};

    use super::{HistoryCursor, HistoryQuery, StorageExecutor, TextUpsertInput};

    static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    /// 生成只供当前测试使用的临时目录，避免并行测试共享用户数据库。
    fn temporary_directory() -> PathBuf {
        let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "clipboard-board-atom17-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("创建存储测试目录失败");
        directory
    }

    /// 释放当前测试自己创建的目录；调用方必须先丢弃执行器以释放 SQLite 文件句柄。
    fn remove_directory(directory: &Path) {
        fs::remove_dir_all(directory).expect("清理存储测试目录失败");
    }

    /// 生成固定长度哈希，测试只关心冲突键是否稳定，不模拟真实 BLAKE3 算法。
    fn test_hash(value: u8) -> [u8; 32] {
        [value; 32]
    }

    /// 生成一条带来源信息的文本 upsert 输入，便于测试重复写入和字段保留规则。
    fn text_input(hash_value: u8, text: &str, preview: &str, copied_at: i64) -> TextUpsertInput {
        TextUpsertInput {
            content_hash: test_hash(hash_value),
            text_content: text.to_owned(),
            preview_text: preview.to_owned(),
            source_exe: Some(format!("old-{hash_value}.exe")),
            source_app: Some(format!("Old App {hash_value}")),
            copied_at,
        }
    }

    /// 验证打开执行器只有在 v1 迁移提交后才返回，并且重复打开保持幂等。
    #[test]
    fn executor_migrates_and_reopens_idempotently() {
        let directory = temporary_directory();
        {
            let executor = StorageExecutor::open_at(&directory).expect("首次启动存储线程失败");
            let mut executor = executor;
            let status = executor.status().expect("读取首次存储状态失败");
            assert_eq!(status.schema_version, 1);
            assert_eq!(status.probe_result, 1);
            assert_eq!(status.clipboard_item_count, 0);
        }
        {
            let mut executor = StorageExecutor::open_at(&directory).expect("重复启动存储线程失败");
            assert_eq!(
                executor
                    .status()
                    .expect("读取重复存储状态失败")
                    .schema_version,
                1
            );
        }
        remove_directory(&directory);
    }

    /// 验证实际连接的创建线程和 SELECT 探针线程相同，且不同于调用方线程。
    #[test]
    fn connection_probe_stays_on_storage_thread() {
        let directory = temporary_directory();
        let mut executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let caller_thread_id = thread::current().id();
        let status = executor.status().expect("执行连接探针失败");

        assert_ne!(caller_thread_id, status.worker_thread_id);
        assert_eq!(status.worker_thread_id, status.connection_thread_id);
        assert_eq!(status.connection_thread_id, status.probe_thread_id);
        drop(executor);
        remove_directory(&directory);
    }

    /// 验证显式 shutdown 会先收到 worker 回执，再释放连接并允许目录清理。
    #[test]
    fn explicit_shutdown_joins_worker_before_return() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        executor.shutdown().expect("显式关闭存储线程失败");
        remove_directory(&directory);
    }

    /// 验证已有哨兵记录在 executor 重启和 v1 重复迁移后仍然存在。
    #[test]
    fn preexisting_sentinel_survives_reopen() {
        let directory = temporary_directory();
        let database_path = directory.join("clipboard.db");
        {
            let mut connection = Connection::open(&database_path).expect("创建预置数据库失败");
            crate::storage::migration::migrate(&mut connection).expect("预置数据库迁移失败");
            connection
                .execute(
                    "INSERT INTO clipboard_items (item_type, preview_text, content_hash, created_at, copied_at) VALUES ('text', 'sentinel', X'02', 1, 1)",
                    [],
                )
                .expect("写入预置哨兵失败");
        }

        let mut executor = StorageExecutor::open_at(&directory).expect("打开预置数据库失败");
        assert_eq!(
            executor
                .status()
                .expect("读取预置数据库状态失败")
                .clipboard_item_count,
            1
        );
        drop(executor);
        remove_directory(&directory);
    }

    /// 验证首次插入、同毫秒重复写入、收藏字段保留和单行去重都在稳定公共接缝上成立。
    #[test]
    fn text_upsert_deduplicates_and_preserves_old_fields() {
        let directory = temporary_directory();
        let first = {
            let mut executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
            let first = executor
                .upsert_text(text_input(11, "old body", "old preview", 100))
                .expect("首次文本 upsert 失败");
            assert_eq!(first.id, 1);
            assert_eq!(first.content_hash, test_hash(11));
            assert_eq!(first.preview_text, "old preview");
            assert_eq!(first.copy_count, 1);
            assert!(!first.is_pinned);
            assert_eq!(first.created_at, 100);
            assert_eq!(first.copied_at, 100);
            assert_eq!(first.last_used_at, None);
            first
        };

        {
            let database_path = directory.join("clipboard.db");
            let connection = Connection::open(&database_path).expect("打开文本数据库失败");
            connection
                .execute(
                    "UPDATE clipboard_items SET is_pinned = 1, last_used_at = 777 WHERE id = ?1",
                    params![first.id],
                )
                .expect("预置收藏字段失败");
        }

        let mut executor = StorageExecutor::open_at(&directory).expect("重新启动存储线程失败");
        let duplicate = executor
            .upsert_text(TextUpsertInput {
                content_hash: test_hash(11),
                text_content: "new body must be ignored".to_owned(),
                preview_text: "new preview must be ignored".to_owned(),
                source_exe: Some("new.exe".to_owned()),
                source_app: Some("New App".to_owned()),
                copied_at: 100,
            })
            .expect("重复文本 upsert 失败");

        assert_eq!(duplicate.id, first.id);
        assert_eq!(duplicate.preview_text, "old preview");
        assert_eq!(duplicate.source_exe.as_deref(), Some("old-11.exe"));
        assert_eq!(duplicate.source_app.as_deref(), Some("Old App 11"));
        assert_eq!(duplicate.copy_count, 2);
        assert!(duplicate.is_pinned);
        assert_eq!(duplicate.created_at, 100);
        assert_eq!(duplicate.copied_at, 100);
        assert_eq!(duplicate.last_used_at, Some(777));
        assert_eq!(
            executor
                .status()
                .expect("读取去重后的状态失败")
                .clipboard_item_count,
            1
        );
        drop(executor);

        let connection =
            Connection::open(directory.join("clipboard.db")).expect("重新打开文本数据库失败");
        let stored_text: String = connection
            .query_row(
                "SELECT text_content FROM clipboard_items WHERE id = ?1",
                params![first.id],
                |row| row.get(0),
            )
            .expect("读取原始文本失败");
        assert_eq!(stored_text, "old body");
        drop(connection);
        remove_directory(&directory);
    }

    /// 验证最大值饱和递增以及异常的零值、负值都不会溢出或产生无效计数。
    #[test]
    fn text_upsert_normalizes_and_saturates_copy_count() {
        let directory = temporary_directory();
        let database_path = directory.join("clipboard.db");
        {
            let mut connection = Connection::open(&database_path).expect("创建计数数据库失败");
            crate::storage::migration::migrate(&mut connection).expect("计数数据库迁移失败");
            for (hash_value, copy_count) in [(21_u8, i64::MAX), (22_u8, 0), (23_u8, -1)] {
                connection
                    .execute(
                        "INSERT INTO clipboard_items (item_type, text_content, preview_text, content_hash, copy_count, created_at, copied_at) VALUES ('text', 'old', 'old', ?1, ?2, 1, 1)",
                        params![test_hash(hash_value).as_slice(), copy_count],
                    )
                    .expect("预置复制计数失败");
            }
        }

        let mut executor = StorageExecutor::open_at(&directory).expect("打开计数数据库失败");
        let max_result = executor
            .upsert_text(text_input(21, "new", "new", 2))
            .expect("最大计数 upsert 失败");
        let zero_result = executor
            .upsert_text(text_input(22, "new", "new", 2))
            .expect("零计数 upsert 失败");
        let negative_result = executor
            .upsert_text(text_input(23, "new", "new", 2))
            .expect("负计数 upsert 失败");

        assert_eq!(max_result.copy_count, i64::MAX);
        assert_eq!(zero_result.copy_count, 2);
        assert_eq!(negative_result.copy_count, 2);
        drop(executor);
        remove_directory(&directory);
    }

    /// 验证触发器使更新事务整体回滚，且失败后 worker 仍可继续执行状态探针。
    #[test]
    fn text_upsert_rolls_back_on_update_error_and_keeps_worker_alive() {
        let directory = temporary_directory();
        let database_path = directory.join("clipboard.db");
        {
            let mut connection = Connection::open(&database_path).expect("创建回滚数据库失败");
            crate::storage::migration::migrate(&mut connection).expect("回滚数据库迁移失败");
            connection
                .execute(
                    "INSERT INTO clipboard_items (item_type, text_content, preview_text, content_hash, source_exe, source_app, copy_count, is_pinned, created_at, copied_at, last_used_at) VALUES ('text', 'old body', 'old preview', ?1, 'old.exe', 'Old App', 4, 1, 10, 20, 30)",
                    params![test_hash(31).as_slice()],
                )
                .expect("预置回滚记录失败");
            connection
                .execute_batch(
                    "CREATE TRIGGER fail_text_update BEFORE UPDATE OF copied_at ON clipboard_items FOR EACH ROW BEGIN SELECT RAISE(ABORT, 'forced upsert failure'); END;",
                )
                .expect("创建回滚触发器失败");
        }

        let mut executor = StorageExecutor::open_at(&directory).expect("打开回滚数据库失败");
        let result = executor.upsert_text(TextUpsertInput {
            content_hash: test_hash(31),
            text_content: "new body".to_owned(),
            preview_text: "new preview".to_owned(),
            source_exe: Some("new.exe".to_owned()),
            source_app: Some("New App".to_owned()),
            copied_at: 99,
        });
        assert!(matches!(
            result,
            Err(crate::storage::StorageError::Sqlite(_))
        ));
        assert_eq!(
            executor
                .status()
                .expect("回滚后 worker 探针失败")
                .clipboard_item_count,
            1
        );
        drop(executor);

        let connection = Connection::open(&database_path).expect("回滚后重新打开数据库失败");
        let stored = connection
            .query_row(
                "SELECT text_content, preview_text, source_exe, source_app, copy_count, is_pinned, created_at, copied_at, last_used_at FROM clipboard_items WHERE content_hash = ?1",
                params![test_hash(31).as_slice()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, i64>(8)?,
                    ))
                },
            )
            .expect("读取回滚记录失败");
        assert_eq!(
            stored,
            (
                "old body".to_owned(),
                "old preview".to_owned(),
                "old.exe".to_owned(),
                "Old App".to_owned(),
                4,
                1,
                10,
                20,
                30,
            )
        );
        drop(connection);
        remove_directory(&directory);
    }

    /// 验证同毫秒记录按 ID 倒序分页，游标跨页后既不重复也不遗漏。
    #[test]
    fn history_cursor_pages_are_stable_at_same_timestamp_boundary() {
        let directory = temporary_directory();
        let mut executor = StorageExecutor::open_at(&directory).expect("启动查询存储线程失败");
        for (hash_value, copied_at) in [(41_u8, 100_i64), (42, 100), (43, 100), (44, 99), (45, 98)]
        {
            executor
                .upsert_text(text_input(
                    hash_value,
                    &format!("body-{hash_value}"),
                    &format!("preview-{hash_value}"),
                    copied_at,
                ))
                .expect("写入分页测试记录失败");
        }

        let first_page = executor
            .list_history_summaries(None, 2)
            .expect("读取首页摘要失败");
        assert_eq!(
            first_page
                .items
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![3, 2]
        );
        assert_eq!(
            first_page.next_cursor,
            Some(HistoryCursor {
                copied_at: 100,
                id: 2
            })
        );

        let second_page = executor
            .list_history_summaries(first_page.next_cursor, 2)
            .expect("读取第二页摘要失败");
        assert_eq!(
            second_page
                .items
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![1, 4]
        );
        assert_eq!(
            second_page.next_cursor,
            Some(HistoryCursor {
                copied_at: 99,
                id: 4
            })
        );

        let third_page = executor
            .list_history_summaries(second_page.next_cursor, 2)
            .expect("读取尾页摘要失败");
        assert_eq!(
            third_page
                .items
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![5]
        );
        assert_eq!(third_page.next_cursor, None);

        let all_ids = [first_page, second_page, third_page]
            .into_iter()
            .flat_map(|page| page.items.into_iter().map(|item| item.id))
            .collect::<Vec<_>>();
        assert_eq!(all_ids, vec![3, 2, 1, 4, 5]);
        drop(executor);
        remove_directory(&directory);
    }

    /// 验证空库、零页大小、尾部游标和页大小上限都具有明确结果。
    #[test]
    fn history_cursor_boundaries_are_explicit() {
        let directory = temporary_directory();
        let mut executor = StorageExecutor::open_at(&directory).expect("启动边界查询线程失败");

        let empty_page = executor
            .list_history_summaries(None, 50)
            .expect("读取空库摘要失败");
        assert!(empty_page.items.is_empty());
        assert_eq!(empty_page.next_cursor, None);

        let zero_page = executor
            .list_history_summaries(
                Some(HistoryCursor {
                    copied_at: i64::MAX,
                    id: i64::MAX,
                }),
                0,
            )
            .expect("读取零大小摘要失败");
        assert!(zero_page.items.is_empty());
        assert_eq!(zero_page.next_cursor, None);

        let invalid = executor.list_history_summaries(None, 101);
        assert!(matches!(
            invalid,
            Err(crate::storage::StorageError::InvalidPageSize {
                requested: 101,
                max: 100
            })
        ));

        executor
            .upsert_text(text_input(46, "tail body", "tail preview", 1))
            .expect("写入尾部边界记录失败");
        let after_tail = executor
            .list_history_summaries(
                Some(HistoryCursor {
                    copied_at: 1,
                    id: 1,
                }),
                50,
            )
            .expect("读取尾部之后摘要失败");
        assert!(after_tail.items.is_empty());
        assert_eq!(after_tail.next_cursor, None);
        drop(executor);
        remove_directory(&directory);
    }

    /// 验证关键词、来源、类型和收藏筛选均在 SQLite worker 内生效，并按字面处理通配符。
    #[test]
    fn history_query_filters_and_escapes_literals() {
        let directory = temporary_directory();
        {
            let mut executor = StorageExecutor::open_at(&directory).expect("启动筛选查询线程失败");
            executor
                .upsert_text(TextUpsertInput {
                    content_hash: test_hash(51),
                    text_content: r"中文 100%_ \done".to_owned(),
                    preview_text: "中文预览".to_owned(),
                    source_exe: Some("code_100.exe".to_owned()),
                    source_app: Some("Visual Studio Code".to_owned()),
                    copied_at: 100,
                })
                .expect("写入通配符测试记录失败");
            executor
                .upsert_text(TextUpsertInput {
                    content_hash: test_hash(52),
                    text_content: "GitHub issue".to_owned(),
                    preview_text: "GitHub 预览".to_owned(),
                    source_exe: Some("chrome.exe".to_owned()),
                    source_app: Some("Google Chrome".to_owned()),
                    copied_at: 200,
                })
                .expect("写入来源测试记录失败");
            executor
                .upsert_text(TextUpsertInput {
                    content_hash: test_hash(53),
                    text_content: "普通内容".to_owned(),
                    preview_text: "普通预览".to_owned(),
                    source_exe: Some("wechat.exe".to_owned()),
                    source_app: Some("微信".to_owned()),
                    copied_at: 150,
                })
                .expect("写入无匹配测试记录失败");
        }

        {
            let connection =
                Connection::open(directory.join("clipboard.db")).expect("打开筛选测试数据库失败");
            connection
                .execute(
                    "UPDATE clipboard_items SET is_pinned = 1 WHERE content_hash = ?1",
                    params![test_hash(51).as_slice()],
                )
                .expect("设置文本收藏状态失败");
            connection
                .execute(
                    "INSERT INTO clipboard_items (item_type, preview_text, content_hash, copy_count, is_pinned, created_at, copied_at) VALUES ('image', '截图', X'54', 1, 1, 250, 250)",
                    [],
                )
                .expect("写入图片筛选记录失败");
        }

        let mut executor = StorageExecutor::open_at(&directory).expect("重新打开筛选查询线程失败");
        let literal = executor
            .query_history_summaries(HistoryQuery {
                keyword: Some(r"100%_ \done".to_owned()),
                limit: 10,
                ..HistoryQuery::default()
            })
            .expect("字面通配符查询失败");
        assert_eq!(
            literal.items.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![1]
        );

        let source = executor
            .query_history_summaries(HistoryQuery {
                source: Some("chrome".to_owned()),
                limit: 10,
                ..HistoryQuery::default()
            })
            .expect("来源查询失败");
        assert_eq!(
            source.items.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![2]
        );

        let image = executor
            .query_history_summaries(HistoryQuery {
                item_type: Some("image".to_owned()),
                limit: 10,
                ..HistoryQuery::default()
            })
            .expect("类型查询失败");
        assert_eq!(
            image.items.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![4]
        );

        let pinned = executor
            .query_history_summaries(HistoryQuery {
                is_pinned: Some(true),
                limit: 10,
                ..HistoryQuery::default()
            })
            .expect("收藏查询失败");
        assert_eq!(
            pinned.items.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![4, 1]
        );

        let combined = executor
            .query_history_summaries(HistoryQuery {
                keyword: Some("GitHub".to_owned()),
                source: Some("chrome.exe".to_owned()),
                item_type: Some("text".to_owned()),
                is_pinned: Some(false),
                limit: 10,
                ..HistoryQuery::default()
            })
            .expect("组合筛选查询失败");
        assert_eq!(
            combined
                .items
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![2]
        );

        let empty = executor
            .query_history_summaries(HistoryQuery {
                keyword: Some("不存在".to_owned()),
                limit: 10,
                ..HistoryQuery::default()
            })
            .expect("无结果查询失败");
        assert!(empty.items.is_empty());
        drop(executor);
        remove_directory(&directory);
    }

    /// 验证筛选集合内部仍按同毫秒复合游标分页，并保持零页和超限页语义。
    #[test]
    fn history_query_cursor_pages_are_filter_stable() {
        let directory = temporary_directory();
        let mut executor = StorageExecutor::open_at(&directory).expect("启动筛选分页线程失败");
        for (hash_value, copied_at, text) in [
            (61_u8, 100_i64, "分页-61"),
            (62_u8, 100_i64, "分页-62"),
            (63_u8, 100_i64, "分页-63"),
            (64_u8, 100_i64, "其他-64"),
            (65_u8, 99_i64, "分页-65"),
        ] {
            executor
                .upsert_text(TextUpsertInput {
                    content_hash: test_hash(hash_value),
                    text_content: text.to_owned(),
                    preview_text: text.to_owned(),
                    source_exe: Some("pager.exe".to_owned()),
                    source_app: Some("分页测试".to_owned()),
                    copied_at,
                })
                .expect("写入筛选分页记录失败");
        }

        let first_page = executor
            .query_history_summaries(HistoryQuery {
                keyword: Some("分页".to_owned()),
                limit: 2,
                ..HistoryQuery::default()
            })
            .expect("读取筛选首页失败");
        assert_eq!(
            first_page
                .items
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![3, 2]
        );
        assert_eq!(
            first_page.next_cursor,
            Some(HistoryCursor {
                copied_at: 100,
                id: 2,
            })
        );

        let second_page = executor
            .query_history_summaries(HistoryQuery {
                keyword: Some("分页".to_owned()),
                cursor: first_page.next_cursor,
                limit: 2,
                ..HistoryQuery::default()
            })
            .expect("读取筛选第二页失败");
        assert_eq!(
            second_page
                .items
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![1, 5]
        );
        assert_eq!(second_page.next_cursor, None);

        let all_ids = [first_page, second_page]
            .into_iter()
            .flat_map(|page| page.items.into_iter().map(|item| item.id))
            .collect::<Vec<_>>();
        assert_eq!(all_ids, vec![3, 2, 1, 5]);

        let zero_page = executor
            .query_history_summaries(HistoryQuery {
                keyword: Some("分页".to_owned()),
                limit: 0,
                ..HistoryQuery::default()
            })
            .expect("读取零页筛选结果失败");
        assert!(zero_page.items.is_empty());
        assert!(matches!(
            executor.query_history_summaries(HistoryQuery {
                limit: 101,
                ..HistoryQuery::default()
            }),
            Err(crate::storage::StorageError::InvalidPageSize {
                requested: 101,
                max: 100
            })
        ));
        drop(executor);
        remove_directory(&directory);
    }

    /// 验证按 ID 读取完整 payload 保留正文、可空字段、来源和数据库原始哈希。
    #[test]
    fn history_payload_by_id_returns_full_row_or_none() {
        let directory = temporary_directory();
        let mut executor = StorageExecutor::open_at(&directory).expect("启动 payload 查询线程失败");
        let inserted = executor
            .upsert_text(TextUpsertInput {
                content_hash: test_hash(47),
                text_content: "完整正文".to_owned(),
                preview_text: "正文预览".to_owned(),
                source_exe: Some("payload.exe".to_owned()),
                source_app: Some("Payload App".to_owned()),
                copied_at: 123,
            })
            .expect("写入 payload 测试记录失败");

        let payload = executor
            .get_history_payload(inserted.id)
            .expect("读取完整 payload 失败")
            .expect("已写入记录却未返回 payload");
        assert_eq!(payload.id, inserted.id);
        assert_eq!(payload.item_type, "text");
        assert_eq!(payload.text_content.as_deref(), Some("完整正文"));
        assert_eq!(payload.preview_text, "正文预览");
        assert_eq!(payload.content_hash, test_hash(47).to_vec());
        assert_eq!(payload.source_exe.as_deref(), Some("payload.exe"));
        assert_eq!(payload.source_app.as_deref(), Some("Payload App"));
        assert_eq!(payload.copy_count, 1);
        assert!(!payload.is_pinned);
        assert_eq!(payload.created_at, 123);
        assert_eq!(payload.copied_at, 123);
        assert_eq!(payload.last_used_at, None);
        assert_eq!(
            executor
                .get_history_payload(i64::MAX)
                .expect("读取未知 payload 失败"),
            None
        );
        drop(executor);
        remove_directory(&directory);
    }

    /// 验证查询不假设未来类型为 text，并保留 v1 允许为空的正文与来源字段。
    #[test]
    fn history_payload_preserves_nullable_non_text_row() {
        let directory = temporary_directory();
        let database_path = directory.join("clipboard.db");
        {
            let mut connection = Connection::open(&database_path).expect("创建非文本数据库失败");
            crate::storage::migration::migrate(&mut connection).expect("非文本数据库迁移失败");
            connection
                .execute(
                    "INSERT INTO clipboard_items (item_type, text_content, preview_text, content_hash, copy_count, is_pinned, created_at, copied_at) VALUES ('image', NULL, 'image preview', X'01', 1, 1, 10, 20)",
                    [],
                )
                .expect("写入非文本测试记录失败");
        }

        let mut executor = StorageExecutor::open_at(&directory).expect("打开非文本查询线程失败");
        let summaries = executor
            .list_history_summaries(None, 10)
            .expect("读取非文本摘要失败");
        assert_eq!(summaries.items.len(), 1);
        assert_eq!(summaries.items[0].item_type, "image");
        assert!(summaries.items[0].is_pinned);

        let payload = executor
            .get_history_payload(1)
            .expect("读取非文本 payload 失败")
            .expect("非文本记录不存在");
        assert_eq!(payload.item_type, "image");
        assert_eq!(payload.text_content, None);
        assert_eq!(payload.content_hash, vec![1]);
        assert_eq!(payload.source_exe, None);
        assert_eq!(payload.source_app, None);
        assert_eq!(payload.last_used_at, None);
        drop(executor);
        remove_directory(&directory);
    }

    /// 验证未来 schema 版本会在 ready 前传播为启动错误，不会降级数据库。
    #[test]
    fn future_schema_version_prevents_startup() {
        let directory = temporary_directory();
        let database_path = directory.join("clipboard.db");
        {
            let connection = Connection::open(&database_path).expect("创建未来版本数据库失败");
            connection
                .pragma_update(None, "user_version", 2)
                .expect("写入未来版本失败");
        }

        let result = StorageExecutor::open_at(&directory);
        assert!(matches!(
            result,
            Err(crate::storage::StorageError::UnsupportedSchemaVersion(2))
        ));
        remove_directory(&directory);
    }

    /// 验证坏字段结构拒绝启动且迁移事务不留下索引或错误版本。
    #[test]
    fn incompatible_schema_does_not_leave_partial_migration() {
        let directory = temporary_directory();
        let database_path = directory.join("clipboard.db");
        {
            let connection = Connection::open(&database_path).expect("创建坏 schema 数据库失败");
            connection
                .execute_batch(
                    "CREATE TABLE clipboard_items (id INTEGER PRIMARY KEY, item_type TEXT NOT NULL, text_content TEXT, preview_text TEXT NOT NULL, content_hash TEXT NOT NULL, source_exe TEXT, source_app TEXT, copy_count INTEGER NOT NULL, is_pinned INTEGER NOT NULL, created_at INTEGER NOT NULL, copied_at INTEGER NOT NULL, last_used_at INTEGER);",
                )
                .expect("写入坏 schema 失败");
        }

        let result = StorageExecutor::open_at(&directory);
        assert!(matches!(
            result,
            Err(crate::storage::StorageError::IncompatibleSchema(_))
        ));
        let connection = Connection::open(&database_path).expect("重新打开坏 schema 数据库失败");
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("读取坏 schema 版本失败");
        let index_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND tbl_name = 'clipboard_items'",
                [],
                |row| row.get(0),
            )
            .expect("读取坏 schema 索引数量失败");
        assert_eq!(version, 0);
        assert_eq!(index_count, 0);
        drop(connection);
        remove_directory(&directory);
    }
}

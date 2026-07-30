//! 此模块实现唯一 SQLite 连接所有者 `StorageExecutor` 及其有界命令协议。
//!
//! 连接只在线程闭包内部创建、迁移和使用；公共 API 只返回不可变状态，不暴露 rusqlite 连接。

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle, ThreadId},
    time::Duration,
};

use rusqlite::{params, Connection, OptionalExtension, Row, Statement};

use super::{migration, StorageError};
use crate::{
    domain::{ImageAssetRootId, ImageMetadata},
    image_storage::ImageStorageRootKind,
};

/// 存储线程命令队列的容量；探针、写入、删除、查询和关闭命令共用有界队列，避免无限堆积。
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

/// 图片历史插入 SQL；冲突不改行，由调用方根据影响行数进入明确的重复更新分支。
const UPSERT_IMAGE_SQL: &str = r#"
INSERT INTO clipboard_items
    (item_type, text_content, preview_text, content_hash, source_exe, source_app,
     copy_count, is_pinned, created_at, copied_at, last_used_at,
     image_root_id, image_path, thumbnail_path, image_width, image_height,
     image_format, content_size)
VALUES ('image', NULL, ?1, ?2, ?3, ?4, 1, 0, ?5, ?5, NULL,
        ?6, ?7, ?8, ?9, ?10, ?11, ?12)
ON CONFLICT(content_hash) DO NOTHING
"#;

/// 重复图片只刷新来源、最近复制时间和饱和计数，禁止替换既有资产身份。
const UPDATE_DUPLICATE_IMAGE_SQL: &str = r#"
UPDATE clipboard_items
SET source_exe = ?1,
    source_app = ?2,
    copied_at = ?4,
    copy_count = CASE
        WHEN copy_count >= 9223372036854775807 THEN 9223372036854775807
        WHEN copy_count <= 0 THEN 2
        ELSE copy_count + 1
    END
WHERE content_hash = ?3 AND item_type = 'image'
"#;

/// 带关键词、来源、类型和收藏筛选的摘要查询；所有筛选参数都通过绑定值传入。
///
/// `?1` 和 `?2` 是已经转义并包裹 `%` 的 LIKE 模式；游标条件位于筛选条件之后，
/// 因而下一页只会在同一筛选集合中继续，不会因为未匹配记录改变分页边界。
const HISTORY_QUERY_SQL: &str = r#"
SELECT id, item_type, preview_text, content_hash, source_exe, source_app,
       copy_count, is_pinned, created_at, copied_at, last_used_at
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
    /// 唯一存储线程为本次成功事务分配的进程内单调修订号。
    pub mutation_revision: u64,
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

/// 图片历史写入的拥有型输入；元数据已经通过领域对象建立哈希、路径和尺寸不变量。
#[derive(Debug, Eq, PartialEq)]
pub struct ImageUpsertInput {
    /// 已发布并回读验证的图片元数据。
    pub metadata: ImageMetadata,
    /// 当前资产根的规范绝对路径；只用于根注册，不进入历史卡片。
    pub canonical_root: PathBuf,
    /// 当前根是默认目录还是用户自定义目录。
    pub root_kind: ImageStorageRootKind,
    /// 复制发生时的源可执行文件名；未知时为空。
    pub source_exe: Option<String>,
    /// 复制发生时的源应用显示名；未知时为空。
    pub source_app: Option<String>,
    /// 复制事件发生的 Unix 毫秒时间戳。
    pub copied_at: i64,
}

/// 图片事务提交后的最终数据库快照；协调器据此决定本次文件应 commit 还是 rollback。
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ImageUpsertResult {
    /// 唯一存储线程为成功事务分配的单调修订号。
    pub mutation_revision: u64,
    /// 历史记录数据库主键。
    pub id: i64,
    /// 数据库最终采用的图片元数据，可能来自更早的同哈希记录。
    pub metadata: ImageMetadata,
    /// 数据库最终保留的预览文案。
    pub preview_text: String,
    /// 数据库最终保留的源可执行文件名。
    pub source_exe: Option<String>,
    /// 数据库最终保留的源应用显示名。
    pub source_app: Option<String>,
    /// 饱和后的复制次数。
    pub copy_count: i64,
    /// 数据库最终收藏状态。
    pub is_pinned: bool,
    /// 首次创建时间。
    pub created_at: i64,
    /// 最近复制时间。
    pub copied_at: i64,
    /// 最近使用时间。
    pub last_used_at: Option<i64>,
    /// 最终行是否采用了当前输入的根和资产路径。
    pub adopted_published_assets: bool,
}

/// 收藏状态写入的最小拥有型输入；明确期望状态使重试保持幂等。
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SetPinnedInput {
    /// 历史记录数据库主键。
    pub id: i64,
    /// 与 ID 一起校验的固定内容哈希，防止陈旧卡片修改其他记录。
    pub content_hash: [u8; 32],
    /// 事务提交后的明确收藏状态，禁止使用盲切换语义。
    pub is_pinned: bool,
}

/// 收藏事务提交后返回的有限稳定结果；不携带剪贴板正文。
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SetPinnedResult {
    /// 已成功更新的历史记录 ID。
    pub id: i64,
    /// 已校验的内容哈希，供上层隔离迟到结果。
    pub content_hash: [u8; 32],
    /// 数据库事务最终提交的收藏状态。
    pub is_pinned: bool,
}

/// 单条历史删除的最小拥有型输入；稳定身份由数据库主键和内容哈希共同组成。
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeleteHistoryInput {
    /// 历史记录数据库主键。
    pub id: i64,
    /// 与 ID 一起校验的固定内容哈希，防止陈旧卡片删除错误记录。
    pub content_hash: [u8; 32],
}

/// 删除事务提交后的有限结果；不存在记录也作为幂等成功返回。
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeleteHistoryResult {
    /// 调用方提交并完成校验的历史记录 ID。
    pub id: i64,
    /// 调用方提交的内容哈希，供上层隔离迟到结果。
    pub content_hash: [u8; 32],
    /// 本次事务是否实际删除了一行；`false` 表示目标此前已经不存在。
    pub was_deleted: bool,
}

/// 清空未收藏文本事务提交后的有限结果；不携带任何剪贴板正文或稳定身份。
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ClearUnpinnedTextResult {
    /// 本次事务实际删除的未收藏文本行数；零表示幂等成功。
    pub deleted_count: u64,
    /// 唯一存储线程为本次成功事务分配的进程内单调修订号。
    pub mutation_revision: u64,
}

/// 清空全部历史事务提交后的有限结果；不携带任何剪贴板正文或记录身份。
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ClearAllHistoryResult {
    /// 本次事务实际删除的全部类型和收藏状态记录数量；零表示幂等成功。
    pub deleted_count: u64,
    /// 唯一存储线程为本次成功事务分配的进程内单调修订号。
    pub mutation_revision: u64,
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
    /// 严格校验后的 32 字节内容哈希，分页卡片无需读取正文即可建立复制身份。
    pub content_hash: [u8; 32],
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
    /// 在 worker 的唯一连接上注册根并原子插入或更新一条图片历史。
    UpsertImage {
        /// 已验证图片元数据、当前根注册信息和来源快照。
        input: ImageUpsertInput,
        /// 返回最终数据库快照及本次资产是否被采用。
        reply: SyncSender<Result<ImageUpsertResult, StorageError>>,
    },
    /// 在 worker 的唯一连接上按稳定身份设置明确收藏状态。
    SetPinned {
        /// 不含正文的稳定身份和期望状态。
        input: SetPinnedInput,
        /// 返回事务提交后的有限结果。
        reply: SyncSender<Result<SetPinnedResult, StorageError>>,
    },
    /// 在 worker 的唯一连接上按稳定身份幂等删除一条文本历史。
    DeleteHistory {
        /// 不含正文的稳定身份。
        input: DeleteHistoryInput,
        /// 返回事务提交后的有限删除结果。
        reply: SyncSender<Result<DeleteHistoryResult, StorageError>>,
    },
    /// 在 worker 的唯一连接上用单个事务清空所有未收藏文本。
    ClearUnpinnedText {
        /// 返回删除数量和存储线性化修订号。
        reply: SyncSender<Result<ClearUnpinnedTextResult, StorageError>>,
    },
    /// 在 worker 的唯一连接上用单个事务清空全部历史记录。
    ClearAllHistory {
        /// 返回删除数量和存储线性化修订号。
        reply: SyncSender<Result<ClearAllHistoryResult, StorageError>>,
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
    /// 测试专用栅栏：证明客户端等待回执时不会持有生命周期门禁。
    #[cfg(test)]
    TestBlock {
        /// 通知测试线程 worker 已经开始处理命令。
        entered: SyncSender<()>,
        /// 由测试线程控制 worker 何时继续。
        release: Receiver<()>,
        /// 解除栅栏后返回成功回执。
        reply: SyncSender<Result<(), StorageError>>,
    },
    /// 测试专用故障注入：验证 panic 后所有权和错误优先级。
    #[cfg(test)]
    TestPanic {
        /// panic 会丢弃回执发送端，使等待客户端稳定解除阻塞。
        reply: SyncSender<Result<(), StorageError>>,
    },
    /// 测试专用故障注入：把操作修订号推进到指定值，以验证耗尽前拒绝语义。
    #[cfg(test)]
    TestSetMutationRevision {
        /// 下一条 mutation 将从此值执行 checked_add。
        revision: u64,
        /// 设置完成后的同步回执。
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
    /// 捕获 upsert 与清空事务共享的进程内单调线性化修订号。
    storage_mutation_revision: u64,
}

/// 共享命令入口的生命周期；所有克隆客户端必须经过同一把门禁检查。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StorageLifecycle {
    /// 接受新的业务命令。
    Open,
    /// 已拒绝新命令，所有者正在完成唯一关闭流程。
    Closing,
    /// worker 已被回收，存储执行器不可再使用。
    Closed,
}

/// 所有客户端共享的有界命令入口；SQLite 连接和线程句柄不在此结构中。
struct StorageShared {
    /// 唯一 worker 的有界命令发送端。
    command_sender: SyncSender<StorageCommand>,
    /// 提交门禁同时保护生命周期检查和入队，建立明确的关闭线性化点。
    lifecycle: Mutex<StorageLifecycle>,
    /// 关闭意图先于互斥锁竞争发布，阻止逃逸客户端持续抢占 Open 门禁。
    closing_intent: AtomicBool,
}

/// 可克隆的受控存储客户端；只能提交业务命令，不能关闭或回收 worker。
#[derive(Clone)]
pub struct StorageClient {
    /// 共享入口只包含发送端和生命周期门禁，不包含 SQLite 连接或 join 句柄。
    shared: Arc<StorageShared>,
}

/// 本地 SQLite 单线程执行器；实例不可 Clone，连接所有权始终留在 worker。
pub struct StorageExecutor {
    /// 可向业务线程签发受控客户端的共享入口。
    shared: Arc<StorageShared>,
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
                shared: Arc::new(StorageShared {
                    command_sender,
                    lifecycle: Mutex::new(StorageLifecycle::Open),
                    closing_intent: AtomicBool::new(false),
                }),
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

    /// 签发共享同一 worker 的受控客户端；客户端没有关闭和 join 权限。
    pub fn client(&self) -> StorageClient {
        StorageClient {
            shared: Arc::clone(&self.shared),
        }
    }

    /// 在存储线程的实际连接上执行只读探针并等待结果。
    pub fn status(&self) -> Result<StorageStatus, StorageError> {
        self.client().status()
    }

    /// 在 worker 的实际连接上执行文本历史的事务性插入或去重更新。
    pub fn upsert_text(&self, input: TextUpsertInput) -> Result<TextUpsertResult, StorageError> {
        self.client().upsert_text(input)
    }

    /// 在实际存储线程中注册当前根并事务性写入图片历史。
    pub fn upsert_image(&self, input: ImageUpsertInput) -> Result<ImageUpsertResult, StorageError> {
        self.client().upsert_image(input)
    }

    /// 按 ID 和内容哈希事务性设置明确收藏状态。
    pub fn set_history_pinned(
        &self,
        input: SetPinnedInput,
    ) -> Result<SetPinnedResult, StorageError> {
        self.client().set_history_pinned(input)
    }

    /// 按 ID 和内容哈希事务性删除一条文本历史；目标不存在时返回幂等成功。
    pub fn delete_history(
        &self,
        input: DeleteHistoryInput,
    ) -> Result<DeleteHistoryResult, StorageError> {
        self.client().delete_history(input)
    }

    /// 使用单个事务删除全部未收藏文本；收藏和非文本记录保持不变。
    pub fn clear_unpinned_text(&self) -> Result<ClearUnpinnedTextResult, StorageError> {
        self.client().clear_unpinned_text()
    }

    /// 使用单个事务删除全部类型和收藏状态的历史记录。
    pub fn clear_all_history(&self) -> Result<ClearAllHistoryResult, StorageError> {
        self.client().clear_all_history()
    }

    /// 使用复合游标读取一页历史摘要；页大小在发送命令前固定校验。
    pub fn list_history_summaries(
        &self,
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
        &self,
        query: HistoryQuery,
    ) -> Result<HistoryPage, StorageError> {
        self.client().query_history_summaries(query)
    }

    /// 按 ID 读取完整历史 payload；找不到记录时返回 `Ok(None)`。
    pub fn get_history_payload(&self, id: i64) -> Result<Option<HistoryPayload>, StorageError> {
        self.client().get_history_payload(id)
    }

    /// 建立关闭线性化点；返回后所有现存和逃逸的客户端都稳定拒绝新命令。
    pub fn begin_closing(&mut self) -> Result<(), StorageError> {
        // 先发布准入栅栏，再等待已经进入临界区的单个提交完成，避免普通 Mutex
        // 缺乏公平性时逃逸 clone 持续抢占导致关闭线程饥饿。
        self.shared.closing_intent.store(true, Ordering::Release);
        let mut lifecycle = self
            .shared
            .lifecycle
            .lock()
            .map_err(|_| StorageError::ChannelClosed)?;
        match *lifecycle {
            StorageLifecycle::Open => {
                *lifecycle = StorageLifecycle::Closing;
                Ok(())
            }
            StorageLifecycle::Closing => Err(StorageError::StorageClosing),
            StorageLifecycle::Closed => Err(StorageError::StorageClosed),
        }
    }

    /// 由唯一所有者发送唯一 Shutdown、等待回执并回收 worker。
    ///
    /// 必须先调用 `begin_closing`；即使发送或回执失败，本方法也会继续 join 并最终
    /// 将共享状态置为 Closed，避免逃逸客户端观察到虚假的可用状态。
    pub fn finish_shutdown(&mut self) -> Result<(), StorageError> {
        {
            let lifecycle = self
                .shared
                .lifecycle
                .lock()
                .map_err(|_| StorageError::ChannelClosed)?;
            match *lifecycle {
                StorageLifecycle::Open => return Err(StorageError::ShutdownNotBegun),
                StorageLifecycle::Closing => {}
                StorageLifecycle::Closed => return Err(StorageError::StorageClosed),
            }
        }

        let (reply_sender, reply_receiver) = sync_channel(1);
        let command_result = match self.shared.command_sender.send(StorageCommand::Shutdown {
            reply: reply_sender,
        }) {
            Ok(()) => reply_receiver
                .recv()
                .unwrap_or(Err(StorageError::ChannelClosed)),
            Err(_) => Err(StorageError::ChannelClosed),
        };
        let join_result = self.join_worker();
        if let Ok(mut lifecycle) = self.shared.lifecycle.lock() {
            *lifecycle = StorageLifecycle::Closed;
        }

        match (command_result, join_result) {
            (_, Err(error)) => Err(error),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    /// 兼容单步关闭调用；内部仍严格执行 begin/finish 两阶段协议。
    pub fn shutdown(mut self) -> Result<(), StorageError> {
        self.begin_closing()?;
        self.finish_shutdown()
    }

    /// 回收 worker 句柄并将 panic 映射为可观察的存储错误。
    fn join_worker(&mut self) -> Result<(), StorageError> {
        self.worker
            .take()
            .ok_or(StorageError::WorkerPanicked)?
            .join()
            .map_err(|_| StorageError::WorkerPanicked)
    }
}

impl Drop for StorageExecutor {
    /// 在异常路径保守执行两阶段关闭；任何失败都不能跳过 worker 回收。
    fn drop(&mut self) {
        if self.worker.is_none() {
            return;
        }
        let _ = self.begin_closing();
        let _ = self.finish_shutdown();
    }
}

impl StorageClient {
    /// 在同一门禁内检查 Open 并提交命令；入队后立即释放门禁，绝不持锁等待回执。
    fn submit(&self, command: StorageCommand) -> Result<(), StorageError> {
        if self.shared.closing_intent.load(Ordering::Acquire) {
            return Err(self.lifecycle_error());
        }
        let lifecycle = self
            .shared
            .lifecycle
            .lock()
            .map_err(|_| StorageError::ChannelClosed)?;
        match *lifecycle {
            StorageLifecycle::Open if !self.shared.closing_intent.load(Ordering::Acquire) => self
                .shared
                .command_sender
                .send(command)
                .map_err(|_| StorageError::ChannelClosed),
            StorageLifecycle::Open | StorageLifecycle::Closing => Err(StorageError::StorageClosing),
            StorageLifecycle::Closed => Err(StorageError::StorageClosed),
        }
    }

    /// 根据共享生命周期生成不触碰 transport 的稳定拒绝错误。
    fn lifecycle_error(&self) -> StorageError {
        match self.shared.lifecycle.lock() {
            Ok(lifecycle) if *lifecycle == StorageLifecycle::Closed => StorageError::StorageClosed,
            _ => StorageError::StorageClosing,
        }
    }

    /// 在共享 worker 的实际连接上执行只读线程归属探针。
    pub fn status(&self) -> Result<StorageStatus, StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        self.submit(StorageCommand::Inspect {
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .unwrap_or(Err(StorageError::ChannelClosed))
    }

    /// 在共享 worker 上用单个事务删除全部未收藏文本。
    pub fn clear_unpinned_text(&self) -> Result<ClearUnpinnedTextResult, StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        self.submit(StorageCommand::ClearUnpinnedText {
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .unwrap_or(Err(StorageError::ChannelClosed))
    }

    /// 在共享 worker 上用单个事务删除全部历史记录。
    pub fn clear_all_history(&self) -> Result<ClearAllHistoryResult, StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        self.submit(StorageCommand::ClearAllHistory {
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .unwrap_or(Err(StorageError::ChannelClosed))
    }

    /// 在共享 worker 上事务性插入或更新文本历史。
    pub fn upsert_text(&self, input: TextUpsertInput) -> Result<TextUpsertResult, StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        self.submit(StorageCommand::UpsertText {
            input,
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .unwrap_or(Err(StorageError::ChannelClosed))
    }

    /// 在共享 worker 上注册当前根并事务性插入或更新图片历史。
    pub fn upsert_image(&self, input: ImageUpsertInput) -> Result<ImageUpsertResult, StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        self.submit(StorageCommand::UpsertImage {
            input,
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .unwrap_or(Err(StorageError::ChannelClosed))
    }

    /// 在共享 worker 上按稳定身份事务性设置明确收藏状态。
    pub fn set_history_pinned(
        &self,
        input: SetPinnedInput,
    ) -> Result<SetPinnedResult, StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        self.submit(StorageCommand::SetPinned {
            input,
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .unwrap_or(Err(StorageError::ChannelClosed))
    }

    /// 在共享 worker 上按稳定身份事务性删除一条文本历史。
    pub fn delete_history(
        &self,
        input: DeleteHistoryInput,
    ) -> Result<DeleteHistoryResult, StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        self.submit(StorageCommand::DeleteHistory {
            input,
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .unwrap_or(Err(StorageError::ChannelClosed))
    }

    /// 使用复合游标读取一页历史摘要。
    pub fn list_history_summaries(
        &self,
        cursor: Option<HistoryCursor>,
        limit: u32,
    ) -> Result<HistoryPage, StorageError> {
        self.query_history_summaries(HistoryQuery {
            cursor,
            limit,
            ..HistoryQuery::default()
        })
    }

    /// 提交筛选摘要查询；页大小在进入有界队列前校验。
    pub fn query_history_summaries(
        &self,
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
        self.submit(StorageCommand::ListHistory {
            query,
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .unwrap_or(Err(StorageError::ChannelClosed))
    }

    /// 按 ID 读取完整历史 payload；找不到记录时返回 `Ok(None)`。
    pub fn get_history_payload(&self, id: i64) -> Result<Option<HistoryPayload>, StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        self.submit(StorageCommand::GetHistoryPayload {
            id,
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .unwrap_or(Err(StorageError::ChannelClosed))
    }

    /// 测试专用：阻塞 worker，直到测试释放栅栏。
    #[cfg(test)]
    fn test_block(
        &self,
        entered: SyncSender<()>,
        release: Receiver<()>,
    ) -> Result<(), StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        self.submit(StorageCommand::TestBlock {
            entered,
            release,
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .unwrap_or(Err(StorageError::ChannelClosed))
    }

    /// 测试专用：让 worker 在命令处理中 panic。
    #[cfg(test)]
    fn test_panic_worker(&self) -> Result<(), StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        self.submit(StorageCommand::TestPanic {
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .unwrap_or(Err(StorageError::ChannelClosed))
    }

    /// 测试专用：把 worker 的操作修订号设置为指定值。
    #[cfg(test)]
    fn test_set_mutation_revision(&self, revision: u64) -> Result<(), StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        self.submit(StorageCommand::TestSetMutationRevision {
            revision,
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .unwrap_or(Err(StorageError::ChannelClosed))
    }

    /// 测试专用：暴露“已取得门禁”和“已完成入队”两个确定性时点。
    #[cfg(test)]
    fn test_status_with_admission(
        &self,
        gate_entered: SyncSender<()>,
        admitted: SyncSender<()>,
    ) -> Result<StorageStatus, StorageError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        let lifecycle = self
            .shared
            .lifecycle
            .lock()
            .map_err(|_| StorageError::ChannelClosed)?;
        if *lifecycle != StorageLifecycle::Open {
            return Err(if *lifecycle == StorageLifecycle::Closed {
                StorageError::StorageClosed
            } else {
                StorageError::StorageClosing
            });
        }
        gate_entered
            .send(())
            .map_err(|_| StorageError::ChannelClosed)?;
        self.shared
            .command_sender
            .send(StorageCommand::Inspect {
                reply: reply_sender,
            })
            .map_err(|_| StorageError::ChannelClosed)?;
        drop(lifecycle);
        admitted.send(()).map_err(|_| StorageError::ChannelClosed)?;
        reply_receiver
            .recv()
            .unwrap_or(Err(StorageError::ChannelClosed))
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
                let result = reserve_mutation_revision(&state).and_then(|revision| {
                    upsert_text(&mut state.connection, input, revision).inspect(|_| {
                        // 只有事务已经提交成功，才能公开并安装预留的修订号。
                        state.storage_mutation_revision = revision;
                    })
                });
                let _ = reply.send(result);
            }
            StorageCommand::UpsertImage { input, reply } => {
                let result = reserve_mutation_revision(&state).and_then(|revision| {
                    upsert_image(&mut state.connection, input, revision).inspect(|_| {
                        // 图片根注册和历史行在同一事务提交后，才安装可观察修订号。
                        state.storage_mutation_revision = revision;
                    })
                });
                let _ = reply.send(result);
            }
            StorageCommand::SetPinned { input, reply } => {
                let _ = reply.send(set_history_pinned(&mut state.connection, input));
            }
            StorageCommand::DeleteHistory { input, reply } => {
                let _ = reply.send(delete_history(&mut state.connection, input));
            }
            StorageCommand::ClearUnpinnedText { reply } => {
                let result = reserve_mutation_revision(&state).and_then(|revision| {
                    clear_unpinned_text(&mut state.connection, revision).inspect(|_| {
                        // 删除零条也是成功的线性化事务，仍须安装新修订号。
                        state.storage_mutation_revision = revision;
                    })
                });
                let _ = reply.send(result);
            }
            StorageCommand::ClearAllHistory { reply } => {
                let result = reserve_mutation_revision(&state).and_then(|revision| {
                    clear_all_history(&mut state.connection, revision).inspect(|_| {
                        // 全量删除零条也是成功事务，仍须安装新修订号以隔离旧捕获。
                        state.storage_mutation_revision = revision;
                    })
                });
                let _ = reply.send(result);
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
            #[cfg(test)]
            StorageCommand::TestBlock {
                entered,
                release,
                reply,
            } => {
                let _ = entered.send(());
                let _ = release.recv();
                let _ = reply.send(Ok(()));
            }
            #[cfg(test)]
            StorageCommand::TestPanic { reply } => {
                // 显式丢弃后 panic，等待客户端会因回执通道断开而解除阻塞。
                drop(reply);
                panic!("测试注入的存储 worker panic");
            }
            #[cfg(test)]
            StorageCommand::TestSetMutationRevision { revision, reply } => {
                state.storage_mutation_revision = revision;
                let _ = reply.send(Ok(()));
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
        storage_mutation_revision: 0,
    })
}

/// 在任何 SQL 执行前预留下一操作修订号；耗尽时数据库必须保持不变。
fn reserve_mutation_revision(state: &StorageState) -> Result<u64, StorageError> {
    state
        .storage_mutation_revision
        .checked_add(1)
        .ok_or(StorageError::MutationRevisionExhausted)
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
    mutation_revision: u64,
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
                mutation_revision,
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

/// 在同一 SQLite 事务内更新根注册、图片历史和最终资产采用判定。
fn upsert_image(
    connection: &mut Connection,
    input: ImageUpsertInput,
    mutation_revision: u64,
) -> Result<ImageUpsertResult, StorageError> {
    let ImageUpsertInput {
        metadata,
        canonical_root,
        root_kind,
        source_exe,
        source_app,
        copied_at,
    } = input;
    let canonical_root = canonical_root
        .to_str()
        .ok_or(StorageError::InvalidImageRootPath)?;
    let content_hash = *metadata.content_hash();
    let content_hash_blob = content_hash.to_vec();
    let root_id = metadata.root_id();
    let root_id_blob = root_id.as_bytes().to_vec();
    let image_path = metadata
        .image_path()
        .as_path()
        .to_str()
        .ok_or(StorageError::InvalidImageAssetMetadata)?;
    let thumbnail_path = metadata
        .thumbnail_path()
        .as_path()
        .to_str()
        .ok_or(StorageError::InvalidImageAssetMetadata)?;
    let preview_text = format!("图片 {} × {}", metadata.width(), metadata.height());
    let transaction = connection.transaction()?;

    // 同一稳定 root_id 可以随已验证目录移动更新路径；UNIQUE(root_path) 会拒绝另一
    // root_id 冒用当前路径，整个图片事务随冲突一起回滚。
    transaction.execute(
        r#"
INSERT INTO image_asset_roots (root_id, root_path, root_kind, created_at)
VALUES (?1, ?2, ?3, ?4)
ON CONFLICT(root_id) DO UPDATE SET
    root_path = excluded.root_path,
    root_kind = excluded.root_kind
"#,
        params![
            root_id_blob.as_slice(),
            canonical_root,
            root_kind.as_str(),
            copied_at
        ],
    )?;

    let inserted = transaction.execute(
        UPSERT_IMAGE_SQL,
        params![
            &preview_text,
            content_hash_blob.as_slice(),
            &source_exe,
            &source_app,
            copied_at,
            root_id_blob.as_slice(),
            image_path,
            thumbnail_path,
            i64::from(metadata.width().get()),
            i64::from(metadata.height().get()),
            metadata.format().as_str(),
            metadata.content_size(),
        ],
    )? == 1;
    if !inserted {
        let updated = transaction.execute(
            UPDATE_DUPLICATE_IMAGE_SQL,
            params![
                &source_exe,
                &source_app,
                content_hash_blob.as_slice(),
                copied_at
            ],
        )?;
        if updated != 1 {
            // 唯一哈希若属于非图片行或异常触发器，不能伪装成成功重复图片。
            return Err(StorageError::InvalidImageAssetMetadata);
        }
    }

    let result = transaction.query_row(
        r#"
SELECT id, preview_text, source_exe, source_app, copy_count, is_pinned,
       created_at, copied_at, last_used_at, image_root_id, image_path,
       thumbnail_path, image_width, image_height, content_size
FROM clipboard_items
WHERE content_hash = ?1 AND item_type = 'image'
"#,
        params![content_hash_blob.as_slice()],
        |row| {
            let stored_root_blob: Vec<u8> = row.get(9)?;
            let stored_root_bytes: [u8; 32] =
                stored_root_blob.as_slice().try_into().map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        32,
                        rusqlite::types::Type::Blob,
                        Box::new(StorageError::InvalidImageAssetMetadata),
                    )
                })?;
            let stored_content_size: i64 = row.get(14)?;
            let stored_content_size = u64::try_from(stored_content_size)
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(14, stored_content_size))?;
            let stored_metadata = ImageMetadata::new(
                content_hash,
                ImageAssetRootId::new(stored_root_bytes),
                row.get::<_, String>(10)?,
                row.get::<_, String>(11)?,
                row.get(12)?,
                row.get(13)?,
                stored_content_size,
            )
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    32,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            // 采用判定来自 INSERT 是否真正创建新行，不能用元数据相等替代；同根同路径的
            // 重复捕获也必须让协调器 rollback 本次发布句柄。
            let adopted_published_assets = inserted;
            Ok(ImageUpsertResult {
                mutation_revision,
                id: row.get(0)?,
                metadata: stored_metadata,
                preview_text: row.get(1)?,
                source_exe: row.get(2)?,
                source_app: row.get(3)?,
                copy_count: row.get(4)?,
                is_pinned: row.get::<_, i64>(5)? != 0,
                created_at: row.get(6)?,
                copied_at: row.get(7)?,
                last_used_at: row.get(8)?,
                adopted_published_assets,
            })
        },
    )?;

    transaction.commit()?;
    Ok(result)
}

/// 使用单个 SQLite 事务只删除未收藏文本，并返回有限删除数量与线性化修订号。
fn clear_unpinned_text(
    connection: &mut Connection,
    mutation_revision: u64,
) -> Result<ClearUnpinnedTextResult, StorageError> {
    let transaction = connection.transaction()?;
    let deleted_count = transaction.execute(
        "DELETE FROM clipboard_items WHERE item_type = 'text' AND is_pinned = 0",
        [],
    )?;
    transaction.commit()?;

    Ok(ClearUnpinnedTextResult {
        // rusqlite 的影响行数是 usize；Rust 支持目标的 usize 不宽于 u64。
        deleted_count: deleted_count as u64,
        mutation_revision,
    })
}

/// 使用单个 SQLite 事务删除全部历史行，并返回有限删除数量与线性化修订号。
fn clear_all_history(
    connection: &mut Connection,
    mutation_revision: u64,
) -> Result<ClearAllHistoryResult, StorageError> {
    let transaction = connection.transaction()?;
    let deleted_count = transaction.execute("DELETE FROM clipboard_items", [])?;
    transaction.commit()?;

    Ok(ClearAllHistoryResult {
        // rusqlite 的影响行数是 usize；Rust 支持目标的 usize 不宽于 u64。
        deleted_count: deleted_count as u64,
        mutation_revision,
    })
}

/// 在同一事务内校验稳定身份、写入明确收藏状态并读取最终值。
fn set_history_pinned(
    connection: &mut Connection,
    input: SetPinnedInput,
) -> Result<SetPinnedResult, StorageError> {
    let SetPinnedInput {
        id,
        content_hash,
        is_pinned,
    } = input;
    let content_hash_blob = content_hash.to_vec();
    let pinned_value = if is_pinned { 1_i64 } else { 0_i64 };
    let transaction = connection.transaction()?;

    // UPDATE 的双条件同时承担身份校验；零行意味着卡片已过期或记录已不存在。
    let affected = transaction.execute(
        "UPDATE clipboard_items SET is_pinned = ?1 WHERE id = ?2 AND content_hash = ?3",
        params![pinned_value, id, content_hash_blob.as_slice()],
    )?;
    if affected != 1 {
        return Err(StorageError::HistoryIdentityMismatch { id });
    }

    let final_value = transaction.query_row(
        "SELECT is_pinned FROM clipboard_items WHERE id = ?1 AND content_hash = ?2",
        params![id, content_hash_blob.as_slice()],
        |row| row.get::<_, i64>(0),
    )? != 0;
    transaction.commit()?;

    Ok(SetPinnedResult {
        id,
        content_hash,
        is_pinned: final_value,
    })
}

/// 在同一事务内校验类型与稳定身份，并精确删除一条文本历史。
fn delete_history(
    connection: &mut Connection,
    input: DeleteHistoryInput,
) -> Result<DeleteHistoryResult, StorageError> {
    let DeleteHistoryInput { id, content_hash } = input;
    let content_hash_blob = content_hash.to_vec();
    let transaction = connection.transaction()?;

    // 先按不可复用的 AUTOINCREMENT 主键读取身份；不存在是可重试的幂等成功。
    let stored_identity = transaction
        .query_row(
            "SELECT item_type, content_hash FROM clipboard_items WHERE id = ?1",
            params![id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    let Some((item_type, stored_hash)) = stored_identity else {
        transaction.commit()?;
        return Ok(DeleteHistoryResult {
            id,
            content_hash,
            was_deleted: false,
        });
    };

    // 图片文件尚无删除生命周期契约，因此存储边界只放行文本记录。
    if item_type != "text" {
        return Err(StorageError::HistoryItemNotDeletable { id });
    }
    if stored_hash.len() != 32 {
        return Err(StorageError::InvalidContentHashLength {
            id,
            length: stored_hash.len(),
        });
    }
    if stored_hash.as_slice() != content_hash_blob.as_slice() {
        return Err(StorageError::HistoryIdentityMismatch { id });
    }

    // WHERE 再次绑定全部身份与类型；触发器或并发异常导致零行时必须回滚。
    let affected = transaction.execute(
        "DELETE FROM clipboard_items WHERE id = ?1 AND content_hash = ?2 AND item_type = 'text'",
        params![id, content_hash_blob.as_slice()],
    )?;
    if affected != 1 {
        return Err(StorageError::HistoryDeleteAffectedRows { id, affected });
    }
    transaction.commit()?;

    Ok(DeleteHistoryResult {
        id,
        content_hash,
        was_deleted: true,
    })
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
fn history_summary_from_row(row: &Row<'_>) -> Result<HistorySummary, StorageError> {
    let id = row.get(0)?;
    let content_hash_blob: Vec<u8> = row.get(3)?;
    let content_hash = content_hash_blob.as_slice().try_into().map_err(|_| {
        StorageError::InvalidContentHashLength {
            id,
            length: content_hash_blob.len(),
        }
    })?;
    Ok(HistorySummary {
        id,
        item_type: row.get(1)?,
        preview_text: row.get(2)?,
        content_hash,
        source_exe: row.get(4)?,
        source_app: row.get(5)?,
        copy_count: row.get(6)?,
        is_pinned: row.get::<_, i64>(7)? != 0,
        created_at: row.get(8)?,
        copied_at: row.get(9)?,
        last_used_at: row.get(10)?,
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
    let mut rows = statement.query(parameters)?;
    let mut summaries = Vec::new();
    while let Some(row) = rows.next()? {
        // 任一行哈希损坏都会立即返回错误，禁止跳过坏记录后交付不完整页面。
        summaries.push(history_summary_from_row(row)?);
    }
    Ok(summaries)
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
    //! 此测试模块验证线程执行器、文本写删事务、游标查询、迁移隔离和错误回滚语义。

    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc::sync_channel,
            Arc,
        },
        thread,
    };

    use rusqlite::{params, Connection};

    use super::{
        DeleteHistoryInput, HistoryCursor, HistoryQuery, ImageUpsertInput, SetPinnedInput,
        StorageExecutor, TextUpsertInput, COMMAND_QUEUE_CAPACITY,
    };
    use crate::{
        domain::{ImageAssetRootId, ImageMetadata},
        image_storage::ImageStorageRootKind,
        storage::StorageError,
    };

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

    /// 生成路径与哈希严格绑定的图片输入，供事务和重复资产身份测试复用。
    fn image_input(
        hash_value: u8,
        root_value: u8,
        canonical_root: PathBuf,
        copied_at: i64,
    ) -> ImageUpsertInput {
        let hash = test_hash(hash_value);
        let hash_hex = format!("{hash_value:02x}").repeat(32);
        let metadata = ImageMetadata::new(
            hash,
            ImageAssetRootId::new(test_hash(root_value)),
            format!("{}/{hash_hex}.png", &hash_hex[..2]),
            format!("{}/{hash_hex}.webp", &hash_hex[..2]),
            640,
            480,
            1024,
        )
        .expect("构造图片元数据失败");
        ImageUpsertInput {
            metadata,
            canonical_root,
            root_kind: ImageStorageRootKind::Custom,
            source_exe: Some("screen.exe".to_owned()),
            source_app: Some("截图工具".to_owned()),
            copied_at,
        }
    }

    /// 新图片必须在同一事务注册根并写入完整图片字段。
    #[test]
    fn image_upsert_registers_root_and_persists_complete_metadata() {
        let directory = temporary_directory();
        let root = directory.join("images-a");
        let executor = StorageExecutor::open_at(&directory).expect("启动图片存储线程失败");
        let result = executor
            .upsert_image(image_input(81, 91, root.clone(), 100))
            .expect("写入新图片失败");

        assert!(result.adopted_published_assets);
        assert_eq!(result.copy_count, 1);
        assert_eq!(result.preview_text, "图片 640 × 480");
        assert_eq!(
            result.metadata.root_id(),
            ImageAssetRootId::new(test_hash(91))
        );
        let connection =
            Connection::open(directory.join("clipboard.db")).expect("打开图片测试数据库失败");
        let stored_root: String = connection
            .query_row(
                "SELECT root_path FROM image_asset_roots WHERE root_id = ?1",
                params![test_hash(91).as_slice()],
                |row| row.get(0),
            )
            .expect("读取图片根注册失败");
        assert_eq!(PathBuf::from(stored_root), root);
        let image_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM clipboard_items WHERE item_type = 'image'",
                [],
                |row| row.get(0),
            )
            .expect("读取图片记录数量失败");
        assert_eq!(image_count, 1);

        drop(connection);
        drop(executor);
        remove_directory(&directory);
    }

    /// 重复哈希必须保留旧资产身份，并明确要求协调器回滚本次新根资产。
    #[test]
    fn duplicate_image_preserves_existing_assets_and_reports_not_adopted() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动重复图片存储线程失败");
        let old_root = directory.join("old-root");
        let first = executor
            .upsert_image(image_input(82, 92, old_root.clone(), 100))
            .expect("写入首张图片失败");
        let mut same_assets = image_input(82, 92, old_root, 200);
        same_assets.source_exe = Some("new-screen.exe".to_owned());
        same_assets.source_app = Some("新截图工具".to_owned());
        let duplicate = executor
            .upsert_image(same_assets)
            .expect("重复图片 upsert 失败");

        assert_eq!(duplicate.id, first.id);
        assert_eq!(duplicate.copy_count, 2);
        assert_eq!(duplicate.created_at, first.created_at);
        assert_eq!(duplicate.metadata, first.metadata);
        assert!(!duplicate.adopted_published_assets);
        assert_eq!(duplicate.source_exe.as_deref(), Some("new-screen.exe"));
        assert_eq!(duplicate.source_app.as_deref(), Some("新截图工具"));

        let other_root = executor
            .upsert_image(image_input(82, 93, directory.join("new-root"), 300))
            .expect("不同根的重复图片 upsert 失败");
        assert_eq!(other_root.metadata, first.metadata);
        assert!(!other_root.adopted_published_assets);

        drop(executor);
        remove_directory(&directory);
    }

    /// 稳定根 ID 可以随受管目录移动更新路径；另一根 ID 不得冒用同一路径。
    #[test]
    fn image_root_move_updates_path_but_path_identity_conflict_rolls_back() {
        let directory = temporary_directory();
        let shared_path = directory.join("moved-root");
        let executor = StorageExecutor::open_at(&directory).expect("启动根移动存储线程失败");
        executor
            .upsert_image(image_input(83, 94, directory.join("old-root"), 100))
            .expect("写入移动前图片失败");
        executor
            .upsert_image(image_input(84, 94, shared_path.clone(), 200))
            .expect("同根 ID 更新路径失败");
        let error = executor
            .upsert_image(image_input(85, 95, shared_path.clone(), 300))
            .expect_err("不同根 ID 冒用路径应失败");
        assert!(matches!(error, StorageError::Sqlite(_)));

        let connection =
            Connection::open(directory.join("clipboard.db")).expect("打开根移动数据库失败");
        let stored_root: String = connection
            .query_row(
                "SELECT root_path FROM image_asset_roots WHERE root_id = ?1",
                params![test_hash(94).as_slice()],
                |row| row.get(0),
            )
            .expect("读取移动后根路径失败");
        assert_eq!(PathBuf::from(stored_root), shared_path);
        let rejected_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM clipboard_items WHERE content_hash = ?1",
                params![test_hash(85).as_slice()],
                |row| row.get(0),
            )
            .expect("读取冲突回滚记录失败");
        assert_eq!(rejected_count, 0);

        drop(connection);
        drop(executor);
        remove_directory(&directory);
    }

    /// 验证打开执行器只有在 v2 迁移提交后才返回，并且重复打开保持幂等。
    #[test]
    fn executor_migrates_and_reopens_idempotently() {
        let directory = temporary_directory();
        {
            let executor = StorageExecutor::open_at(&directory).expect("首次启动存储线程失败");
            let executor = executor;
            let status = executor.status().expect("读取首次存储状态失败");
            assert_eq!(status.schema_version, 2);
            assert_eq!(status.probe_result, 1);
            assert_eq!(status.clipboard_item_count, 0);
        }
        {
            let executor = StorageExecutor::open_at(&directory).expect("重复启动存储线程失败");
            assert_eq!(
                executor
                    .status()
                    .expect("读取重复存储状态失败")
                    .schema_version,
                2
            );
        }
        remove_directory(&directory);
    }

    /// 验证实际连接的创建线程和 SELECT 探针线程相同，且不同于调用方线程。
    #[test]
    fn connection_probe_stays_on_storage_thread() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
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

    /// 验证多个客户端只复用同一连接线程，不会各自创建 SQLite 连接。
    #[test]
    fn cloned_clients_share_the_single_storage_worker() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let first = executor.client();
        let second = first.clone();

        let first_status = first.status().expect("首个客户端探针失败");
        let second_status = second.status().expect("克隆客户端探针失败");
        assert_eq!(
            first_status.connection_thread_id,
            second_status.connection_thread_id
        );
        assert_eq!(
            first_status.worker_thread_id,
            second_status.worker_thread_id
        );

        drop(executor);
        remove_directory(&directory);
    }

    /// 验证关闭线性化点会同时约束既有和逃逸克隆，完成关闭后错误稳定转为 Closed。
    #[test]
    fn closing_rejects_all_existing_clients_and_owner_shuts_down_once() {
        let directory = temporary_directory();
        let mut executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let first = executor.client();
        let escaped = first.clone();

        executor.begin_closing().expect("建立关闭线性化点失败");
        assert!(matches!(first.status(), Err(StorageError::StorageClosing)));
        assert!(matches!(
            escaped.status(),
            Err(StorageError::StorageClosing)
        ));
        assert!(matches!(
            executor.begin_closing(),
            Err(StorageError::StorageClosing)
        ));

        executor.finish_shutdown().expect("完成存储关闭失败");
        assert!(matches!(escaped.status(), Err(StorageError::StorageClosed)));
        assert!(matches!(
            executor.finish_shutdown(),
            Err(StorageError::StorageClosed)
        ));
        remove_directory(&directory);
    }

    /// 验证所有者不能跳过 begin 阶段直接发送 Shutdown。
    #[test]
    fn finish_shutdown_requires_begin_closing() {
        let directory = temporary_directory();
        let mut executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        assert!(matches!(
            executor.finish_shutdown(),
            Err(StorageError::ShutdownNotBegun)
        ));
        executor.shutdown().expect("恢复执行正常关闭失败");
        remove_directory(&directory);
    }

    /// 验证等待业务回执的客户端不持有生命周期门禁，所有者可并发进入 Closing。
    #[test]
    fn client_waiting_for_reply_does_not_block_begin_closing() {
        let directory = temporary_directory();
        let mut executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let client = executor.client();
        let (entered_sender, entered_receiver) = sync_channel(1);
        let (release_sender, release_receiver) = sync_channel(1);
        let waiting_client =
            thread::spawn(move || client.test_block(entered_sender, release_receiver));

        entered_receiver.recv().expect("worker 未进入测试栅栏");
        executor.begin_closing().expect("等待回执时无法进入关闭态");
        release_sender.send(()).expect("释放测试栅栏失败");
        waiting_client
            .join()
            .expect("等待客户端线程 panic")
            .expect("已准入命令未完成");
        executor.finish_shutdown().expect("完成存储关闭失败");
        remove_directory(&directory);
    }

    /// 验证容量四队列满载时，已取得门禁的提交先入队，关闭随后完成且不发生死锁。
    #[test]
    fn full_queue_drains_admitted_commands_before_closing() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let (block_entered_sender, block_entered_receiver) = sync_channel(1);
        let (block_release_sender, block_release_receiver) = sync_channel(1);
        let blocking_client = executor.client();
        let blocker = thread::spawn(move || {
            blocking_client.test_block(block_entered_sender, block_release_receiver)
        });
        block_entered_receiver
            .recv()
            .expect("worker 未进入阻塞栅栏");

        let mut queued = Vec::new();
        for _ in 0..COMMAND_QUEUE_CAPACITY {
            let client = executor.client();
            let (gate_sender, gate_receiver) = sync_channel(1);
            let (admitted_sender, admitted_receiver) = sync_channel(1);
            let handle = thread::spawn(move || {
                client.test_status_with_admission(gate_sender, admitted_sender)
            });
            gate_receiver.recv().expect("排队客户端未取得门禁");
            admitted_receiver.recv().expect("排队客户端未完成入队");
            queued.push(handle);
        }

        let extra_client = executor.client();
        let (extra_gate_sender, extra_gate_receiver) = sync_channel(1);
        let (extra_admitted_sender, extra_admitted_receiver) = sync_channel(1);
        let extra = thread::spawn(move || {
            extra_client.test_status_with_admission(extra_gate_sender, extra_admitted_sender)
        });
        // 额外客户端在容量已满时持有门禁并阻塞于 send。
        extra_gate_receiver.recv().expect("额外客户端未取得门禁");

        let shared = Arc::clone(&executor.shared);
        let closing = thread::spawn(move || {
            let mut executor = executor;
            executor.begin_closing().expect("满队列后进入关闭态失败");
            executor
        });
        while !shared.closing_intent.load(Ordering::Acquire) {
            thread::yield_now();
        }

        block_release_sender.send(()).expect("释放 worker 栅栏失败");
        blocker
            .join()
            .expect("阻塞客户端线程 panic")
            .expect("阻塞命令未完成");
        extra_admitted_receiver
            .recv()
            .expect("额外客户端未在关闭前完成已准入提交");
        for handle in queued {
            handle
                .join()
                .expect("排队客户端线程 panic")
                .expect("已排队探针未完成");
        }
        extra
            .join()
            .expect("额外客户端线程 panic")
            .expect("额外已准入探针未完成");

        let mut executor = closing.join().expect("关闭所有者线程 panic");
        executor.finish_shutdown().expect("满队列后关闭失败");
        remove_directory(&directory);
    }

    /// 验证 worker panic 时客户端只观察通道错误，所有者负责 join 并优先报告 panic。
    #[test]
    fn worker_panic_is_joined_only_by_owner_and_leaves_closed_state() {
        let directory = temporary_directory();
        let mut executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let client = executor.client();
        let escaped = client.clone();
        let client_result = thread::spawn(move || client.test_panic_worker())
            .join()
            .expect("故障注入客户端线程 panic");
        assert!(matches!(client_result, Err(StorageError::ChannelClosed)));

        executor.begin_closing().expect("panic 后建立关闭态失败");
        assert!(matches!(
            executor.finish_shutdown(),
            Err(StorageError::WorkerPanicked)
        ));
        assert!(matches!(escaped.status(), Err(StorageError::StorageClosed)));
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

        let executor = StorageExecutor::open_at(&directory).expect("打开预置数据库失败");
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
            let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
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

        let executor = StorageExecutor::open_at(&directory).expect("重新启动存储线程失败");
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

    /// 同一 worker 内重复哈希复用旧 ID，但成功事务修订号仍必须严格递增。
    #[test]
    fn mutation_revision_orders_duplicate_upsert_and_clear() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let first = executor
            .upsert_text(text_input(51, "原正文", "原预览", 100))
            .expect("首次写入失败");
        let duplicate = executor
            .upsert_text(text_input(51, "重复正文", "重复预览", 200))
            .expect("重复写入失败");
        let cleared = executor.clear_unpinned_text().expect("清空未收藏失败");

        assert_eq!(first.id, duplicate.id);
        assert_eq!(first.mutation_revision, 1);
        assert_eq!(duplicate.mutation_revision, 2);
        assert_eq!(cleared.mutation_revision, 3);
        assert_eq!(cleared.deleted_count, 1);

        drop(executor);
        remove_directory(&directory);
    }

    /// 清空事务只删除未收藏文本，收藏文本和非文本记录必须跨重启保留。
    #[test]
    fn clear_unpinned_text_preserves_pinned_and_non_text_rows() {
        let directory = temporary_directory();
        let database_path = directory.join("clipboard.db");
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let unpinned = executor
            .upsert_text(text_input(52, "待清空", "待清空", 100))
            .expect("写入未收藏文本失败");
        let pinned = executor
            .upsert_text(text_input(53, "保留收藏", "保留收藏", 200))
            .expect("写入收藏文本失败");
        executor
            .set_history_pinned(SetPinnedInput {
                id: pinned.id,
                content_hash: pinned.content_hash,
                is_pinned: true,
            })
            .expect("设置收藏失败");
        {
            let connection = Connection::open(&database_path).expect("打开混合类型数据库失败");
            connection
                .execute(
                    "INSERT INTO clipboard_items
                     (item_type, preview_text, content_hash, is_pinned, created_at, copied_at)
                     VALUES ('binary', '非文本预览', ?1, 0, 300, 300)",
                    params![test_hash(54).as_slice()],
                )
                .expect("写入非文本记录失败");
        }

        let result = executor.clear_unpinned_text().expect("清空未收藏失败");
        assert_eq!(result.deleted_count, 1);
        assert!(executor
            .get_history_payload(unpinned.id)
            .expect("查询已删除文本失败")
            .is_none());
        assert!(executor
            .get_history_payload(pinned.id)
            .expect("查询收藏文本失败")
            .is_some());
        drop(executor);

        let connection = Connection::open(&database_path).expect("重启后打开数据库失败");
        let remaining: Vec<String> = {
            let mut statement = connection
                .prepare("SELECT item_type FROM clipboard_items ORDER BY id")
                .expect("准备剩余类型查询失败");
            statement
                .query_map([], |row| row.get(0))
                .expect("查询剩余类型失败")
                .collect::<Result<_, _>>()
                .expect("读取剩余类型失败")
        };
        assert_eq!(remaining, vec!["text".to_owned(), "binary".to_owned()]);
        drop(connection);
        remove_directory(&directory);
    }

    /// 清空 SQL 失败必须回滚全部删除，不推进修订号，并允许 worker 继续处理后续命令。
    #[test]
    fn clear_unpinned_text_rolls_back_and_reuses_reserved_revision_after_failure() {
        let directory = temporary_directory();
        let database_path = directory.join("clipboard.db");
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let inserted = executor
            .upsert_text(text_input(55, "回滚正文", "回滚预览", 100))
            .expect("写入回滚记录失败");
        {
            let connection = Connection::open(&database_path).expect("打开故障注入数据库失败");
            connection
                .execute_batch(
                    "CREATE TRIGGER abort_unpinned_clear
                     BEFORE DELETE ON clipboard_items
                     WHEN OLD.item_type = 'text' AND OLD.is_pinned = 0
                     BEGIN
                         SELECT RAISE(ABORT, 'clear blocked');
                     END;",
                )
                .expect("创建清空故障触发器失败");
        }

        assert!(matches!(
            executor.clear_unpinned_text(),
            Err(StorageError::Sqlite(_))
        ));
        assert!(executor
            .get_history_payload(inserted.id)
            .expect("查询回滚记录失败")
            .is_some());
        {
            let connection = Connection::open(&database_path).expect("重新打开故障注入数据库失败");
            connection
                .execute_batch("DROP TRIGGER abort_unpinned_clear;")
                .expect("删除清空故障触发器失败");
        }
        let retry = executor.clear_unpinned_text().expect("故障后重试清空失败");
        assert_eq!(retry.deleted_count, 1);
        assert_eq!(retry.mutation_revision, inserted.mutation_revision + 1);

        drop(executor);
        remove_directory(&directory);
    }

    /// 修订号耗尽必须在执行 DELETE 前拒绝，数据库内容不能发生部分变化。
    #[test]
    fn clear_unpinned_text_rejects_revision_exhaustion_before_sql() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let inserted = executor
            .upsert_text(text_input(56, "耗尽正文", "耗尽预览", 100))
            .expect("写入耗尽测试记录失败");
        executor
            .client()
            .test_set_mutation_revision(u64::MAX)
            .expect("设置耗尽修订号失败");

        assert!(matches!(
            executor.clear_unpinned_text(),
            Err(StorageError::MutationRevisionExhausted)
        ));
        assert!(executor
            .get_history_payload(inserted.id)
            .expect("查询耗尽测试记录失败")
            .is_some());

        drop(executor);
        remove_directory(&directory);
    }

    /// 没有未收藏文本时重复清空仍是成功事务，并分配新的单调修订号。
    #[test]
    fn clear_unpinned_text_is_idempotent_with_monotonic_revision() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let first = executor.clear_unpinned_text().expect("首次空清理失败");
        let second = executor.clear_unpinned_text().expect("重复空清理失败");

        assert_eq!(first.deleted_count, 0);
        assert_eq!(second.deleted_count, 0);
        assert_eq!(first.mutation_revision, 1);
        assert_eq!(second.mutation_revision, 2);

        drop(executor);
        remove_directory(&directory);
    }

    /// 全量清空必须在一个事务中删除未收藏、收藏、文本和非文本记录。
    #[test]
    fn clear_all_history_removes_all_types_and_pinned_states() {
        let directory = temporary_directory();
        let database_path = directory.join("clipboard.db");
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let unpinned = executor
            .upsert_text(text_input(57, "普通文本", "普通文本", 100))
            .expect("写入普通文本失败");
        let pinned = executor
            .upsert_text(text_input(58, "收藏文本", "收藏文本", 200))
            .expect("写入收藏文本失败");
        executor
            .set_history_pinned(SetPinnedInput {
                id: pinned.id,
                content_hash: pinned.content_hash,
                is_pinned: true,
            })
            .expect("设置收藏文本失败");
        {
            let connection = Connection::open(&database_path).expect("打开混合类型数据库失败");
            connection
                .execute(
                    "INSERT INTO clipboard_items
                     (item_type, preview_text, content_hash, is_pinned, created_at, copied_at)
                     VALUES ('binary', '收藏非文本', ?1, 1, 300, 300)",
                    params![test_hash(59).as_slice()],
                )
                .expect("写入收藏图片测试行失败");
        }

        let result = executor.clear_all_history().expect("清空全部失败");
        assert_eq!(result.deleted_count, 3);
        assert_eq!(result.mutation_revision, 3);
        assert!(executor
            .get_history_payload(unpinned.id)
            .expect("查询普通文本失败")
            .is_none());
        assert!(executor
            .get_history_payload(pinned.id)
            .expect("查询收藏文本失败")
            .is_none());
        assert_eq!(
            executor
                .status()
                .expect("读取全量清空后状态失败")
                .clipboard_item_count,
            0
        );
        drop(executor);

        let connection = Connection::open(&database_path).expect("重启后打开数据库失败");
        let remaining: i64 = connection
            .query_row("SELECT COUNT(*) FROM clipboard_items", [], |row| row.get(0))
            .expect("查询重启后记录数失败");
        assert_eq!(remaining, 0);
        drop(connection);
        remove_directory(&directory);
    }

    /// 全量 DELETE 失败必须回滚所有行，不推进修订号，并允许后续重试复用该值。
    #[test]
    fn clear_all_history_rolls_back_and_reuses_reserved_revision_after_failure() {
        let directory = temporary_directory();
        let database_path = directory.join("clipboard.db");
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let first = executor
            .upsert_text(text_input(60, "第一条", "第一条", 100))
            .expect("写入第一条失败");
        executor
            .upsert_text(text_input(61, "第二条", "第二条", 200))
            .expect("写入第二条失败");
        {
            let connection = Connection::open(&database_path).expect("打开故障注入数据库失败");
            connection
                .execute_batch(
                    "CREATE TRIGGER abort_clear_all
                     BEFORE DELETE ON clipboard_items
                     BEGIN
                         SELECT RAISE(ABORT, 'clear all blocked');
                     END;",
                )
                .expect("创建全量清空故障触发器失败");
        }

        assert!(matches!(
            executor.clear_all_history(),
            Err(StorageError::Sqlite(_))
        ));
        assert_eq!(
            executor
                .status()
                .expect("读取回滚后状态失败")
                .clipboard_item_count,
            2
        );
        assert!(executor
            .get_history_payload(first.id)
            .expect("查询回滚后的第一条失败")
            .is_some());
        {
            let connection = Connection::open(&database_path).expect("重新打开故障数据库失败");
            connection
                .execute_batch("DROP TRIGGER abort_clear_all;")
                .expect("删除全量清空故障触发器失败");
        }

        let retry = executor
            .clear_all_history()
            .expect("故障后重试清空全部失败");
        assert_eq!(retry.deleted_count, 2);
        assert_eq!(retry.mutation_revision, 3);
        drop(executor);
        remove_directory(&directory);
    }

    /// 修订号耗尽必须在全量 DELETE 之前拒绝，收藏和普通记录均不得被部分删除。
    #[test]
    fn clear_all_history_rejects_revision_exhaustion_before_sql() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let inserted = executor
            .upsert_text(text_input(62, "耗尽记录", "耗尽记录", 100))
            .expect("写入耗尽测试记录失败");
        executor
            .client()
            .test_set_mutation_revision(u64::MAX)
            .expect("设置耗尽修订号失败");

        assert!(matches!(
            executor.clear_all_history(),
            Err(StorageError::MutationRevisionExhausted)
        ));
        assert!(executor
            .get_history_payload(inserted.id)
            .expect("查询耗尽测试记录失败")
            .is_some());

        drop(executor);
        remove_directory(&directory);
    }

    /// 空库重复清空全部仍须成功，并为每个线性化事务分配严格递增修订号。
    #[test]
    fn clear_all_history_is_idempotent_with_monotonic_revision() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let first = executor.clear_all_history().expect("首次空库清空失败");
        let second = executor.clear_all_history().expect("重复空库清空失败");

        assert_eq!(first.deleted_count, 0);
        assert_eq!(second.deleted_count, 0);
        assert_eq!(first.mutation_revision, 1);
        assert_eq!(second.mutation_revision, 2);

        drop(executor);
        remove_directory(&directory);
    }

    /// 收藏写入必须提交到磁盘、支持幂等重试，并在重复复制后继续保留。
    #[test]
    fn set_pinned_persists_is_idempotent_and_survives_duplicate_upsert() {
        let directory = temporary_directory();
        let (id, content_hash) = {
            let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
            let inserted = executor
                .upsert_text(text_input(61, "收藏正文", "收藏预览", 100))
                .expect("写入收藏测试记录失败");
            let request = SetPinnedInput {
                id: inserted.id,
                content_hash: inserted.content_hash,
                is_pinned: true,
            };

            let first = executor
                .set_history_pinned(request.clone())
                .expect("首次收藏失败");
            let repeated = executor
                .set_history_pinned(request)
                .expect("幂等收藏重试失败");
            assert!(first.is_pinned);
            assert_eq!(first, repeated);

            let duplicate = executor
                .upsert_text(text_input(61, "新正文不覆盖", "新预览不覆盖", 200))
                .expect("重复复制写入失败");
            assert!(duplicate.is_pinned);
            (inserted.id, inserted.content_hash)
        };

        {
            let reopened = StorageExecutor::open_at(&directory).expect("重启存储线程失败");
            let payload = reopened
                .get_history_payload(id)
                .expect("重启后读取记录失败")
                .expect("重启后收藏记录丢失");
            assert_eq!(payload.content_hash, content_hash);
            assert!(payload.is_pinned);
            let unpinned = reopened
                .set_history_pinned(SetPinnedInput {
                    id,
                    content_hash,
                    is_pinned: false,
                })
                .expect("取消收藏失败");
            assert!(!unpinned.is_pinned);
        }
        let reopened = StorageExecutor::open_at(&directory).expect("取消收藏后重启失败");
        assert!(
            !reopened
                .get_history_payload(id)
                .expect("取消收藏后读取记录失败")
                .expect("取消收藏后记录丢失")
                .is_pinned
        );
        drop(reopened);
        remove_directory(&directory);
    }

    /// 陈旧 ID 或错误哈希都不能修改目标记录，失败结果也不得泄露正文。
    #[test]
    fn set_pinned_rejects_stale_identity_without_mutation() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let inserted = executor
            .upsert_text(text_input(62, "身份正文", "身份预览", 300))
            .expect("写入身份测试记录失败");

        for invalid in [
            SetPinnedInput {
                id: inserted.id + 1,
                content_hash: inserted.content_hash,
                is_pinned: true,
            },
            SetPinnedInput {
                id: inserted.id,
                content_hash: test_hash(99),
                is_pinned: true,
            },
        ] {
            assert!(matches!(
                executor.set_history_pinned(invalid),
                Err(StorageError::HistoryIdentityMismatch { .. })
            ));
        }

        let payload = executor
            .get_history_payload(inserted.id)
            .expect("读取身份测试记录失败")
            .expect("身份测试记录不存在");
        assert!(!payload.is_pinned);
        drop(executor);
        remove_directory(&directory);
    }

    /// 删除成功必须持久化到磁盘，重复请求和重启后的同一请求都返回幂等成功。
    #[test]
    fn delete_history_persists_and_repeated_request_is_idempotent() {
        let directory = temporary_directory();
        let (id, content_hash) = {
            let executor = StorageExecutor::open_at(&directory).expect("启动删除测试存储线程失败");
            let inserted = executor
                .upsert_text(text_input(71, "待删除正文", "待删除预览", 400))
                .expect("写入待删除记录失败");
            let request = DeleteHistoryInput {
                id: inserted.id,
                content_hash: inserted.content_hash,
            };

            let first = executor
                .delete_history(request.clone())
                .expect("首次删除失败");
            let repeated = executor
                .delete_history(request)
                .expect("同进程幂等删除失败");
            assert!(first.was_deleted);
            assert!(!repeated.was_deleted);
            assert_eq!(first.id, repeated.id);
            assert_eq!(first.content_hash, repeated.content_hash);
            (inserted.id, inserted.content_hash)
        };

        let reopened = StorageExecutor::open_at(&directory).expect("删除后重启存储线程失败");
        assert!(reopened
            .get_history_payload(id)
            .expect("重启后查询删除记录失败")
            .is_none());
        let repeated_after_reopen = reopened
            .delete_history(DeleteHistoryInput {
                id,
                // ID 不存在时任意固定长度哈希都必须幂等成功，不能把缺失误判为身份错配。
                content_hash: test_hash(88),
            })
            .expect("重启后幂等删除失败");
        assert!(!repeated_after_reopen.was_deleted);
        assert_ne!(repeated_after_reopen.content_hash, content_hash);
        drop(reopened);
        remove_directory(&directory);
    }

    /// 已存在 ID 的错误哈希必须拒绝删除，并保留原记录全部字段。
    #[test]
    fn delete_history_rejects_hash_mismatch_without_mutation() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动身份测试存储线程失败");
        let inserted = executor
            .upsert_text(text_input(72, "身份正文", "身份预览", 500))
            .expect("写入身份测试记录失败");

        assert!(matches!(
            executor.delete_history(DeleteHistoryInput {
                id: inserted.id,
                content_hash: test_hash(99),
            }),
            Err(StorageError::HistoryIdentityMismatch { .. })
        ));
        let payload = executor
            .get_history_payload(inserted.id)
            .expect("读取身份记录失败")
            .expect("身份错配后记录被意外删除");
        assert_eq!(payload.text_content.as_deref(), Some("身份正文"));
        drop(executor);
        remove_directory(&directory);
    }

    /// 删除 API 必须拒绝非文本记录，避免未来图片元数据与磁盘缓存失去一致性。
    #[test]
    fn delete_history_rejects_non_text_item_without_mutation() {
        let directory = temporary_directory();
        let database_path = directory.join("clipboard.db");
        let id = {
            let mut connection =
                Connection::open(&database_path).expect("创建非文本测试数据库失败");
            crate::storage::migration::migrate(&mut connection).expect("迁移非文本测试数据库失败");
            connection
                .execute(
                    "INSERT INTO clipboard_items \
                     (item_type, preview_text, content_hash, created_at, copied_at) \
                     VALUES ('binary', '非文本预览', ?1, 1, 1)",
                    params![test_hash(73).as_slice()],
                )
                .expect("写入非文本测试记录失败");
            connection.last_insert_rowid()
        };

        let executor = StorageExecutor::open_at(&directory).expect("启动非文本测试存储线程失败");
        assert!(matches!(
            executor.delete_history(DeleteHistoryInput {
                id,
                content_hash: test_hash(73),
            }),
            Err(StorageError::HistoryItemNotDeletable { .. })
        ));
        assert!(executor
            .get_history_payload(id)
            .expect("读取非文本记录失败")
            .is_some());
        drop(executor);
        remove_directory(&directory);
    }

    /// SQLite 删除异常必须回滚，并且同一个 worker 在返回有限错误后仍可继续服务。
    #[test]
    fn delete_history_rolls_back_on_sql_error_and_worker_remains_usable() {
        let directory = temporary_directory();
        let inserted = {
            let executor =
                StorageExecutor::open_at(&directory).expect("启动 SQL 故障测试存储线程失败");
            executor
                .upsert_text(text_input(74, "回滚正文", "回滚预览", 600))
                .expect("写入 SQL 故障测试记录失败")
        };
        {
            let connection =
                Connection::open(directory.join("clipboard.db")).expect("打开 SQL 故障数据库失败");
            connection
                .execute_batch(
                    "CREATE TRIGGER abort_text_delete BEFORE DELETE ON clipboard_items \
                     BEGIN SELECT RAISE(ABORT, 'forced delete failure'); END;",
                )
                .expect("创建删除失败触发器失败");
        }

        let executor = StorageExecutor::open_at(&directory).expect("重启 SQL 故障测试存储线程失败");
        assert!(matches!(
            executor.delete_history(DeleteHistoryInput {
                id: inserted.id,
                content_hash: inserted.content_hash,
            }),
            Err(StorageError::Sqlite(_))
        ));
        assert!(executor
            .get_history_payload(inserted.id)
            .expect("SQL 失败后读取记录失败")
            .is_some());
        assert_eq!(
            executor
                .status()
                .expect("SQL 失败后 worker 不可用")
                .probe_result,
            1
        );
        drop(executor);
        remove_directory(&directory);
    }

    /// 触发器静默忽略 DELETE 时影响行数为零，事务必须报告异常且保留记录。
    #[test]
    fn delete_history_rolls_back_when_delete_affects_zero_rows() {
        let directory = temporary_directory();
        let inserted = {
            let executor =
                StorageExecutor::open_at(&directory).expect("启动零影响测试存储线程失败");
            executor
                .upsert_text(text_input(75, "零影响正文", "零影响预览", 700))
                .expect("写入零影响测试记录失败")
        };
        {
            let connection =
                Connection::open(directory.join("clipboard.db")).expect("打开零影响数据库失败");
            connection
                .execute_batch(
                    "CREATE TRIGGER ignore_text_delete BEFORE DELETE ON clipboard_items \
                     BEGIN SELECT RAISE(IGNORE); END;",
                )
                .expect("创建忽略删除触发器失败");
        }

        let executor = StorageExecutor::open_at(&directory).expect("重启零影响测试存储线程失败");
        assert!(matches!(
            executor.delete_history(DeleteHistoryInput {
                id: inserted.id,
                content_hash: inserted.content_hash,
            }),
            Err(StorageError::HistoryDeleteAffectedRows { affected: 0, .. })
        ));
        assert!(executor
            .get_history_payload(inserted.id)
            .expect("零影响后读取记录失败")
            .is_some());
        drop(executor);
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

        let executor = StorageExecutor::open_at(&directory).expect("打开计数数据库失败");
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

    /// 验证触发器使更新事务整体回滚，且失败不消耗修订号、worker 仍可继续写入。
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

        let executor = StorageExecutor::open_at(&directory).expect("打开回滚数据库失败");
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
        {
            let connection = Connection::open(&database_path).expect("重新打开回滚数据库失败");
            connection
                .execute_batch("DROP TRIGGER fail_text_update;")
                .expect("删除回滚触发器失败");
        }
        let recovered = executor
            .upsert_text(text_input(32, "恢复正文", "恢复预览", 100))
            .expect("回滚后继续写入失败");
        assert_eq!(recovered.mutation_revision, 1);
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

    /// upsert 在修订号耗尽时必须先于 SQL 拒绝，既不插入新行也不更新旧行。
    #[test]
    fn text_upsert_rejects_revision_exhaustion_before_sql() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let inserted = executor
            .upsert_text(text_input(33, "耗尽原文", "耗尽预览", 100))
            .expect("写入耗尽原记录失败");
        executor
            .client()
            .test_set_mutation_revision(u64::MAX)
            .expect("设置耗尽修订号失败");

        assert!(matches!(
            executor.upsert_text(text_input(33, "不得更新", "不得更新", 200)),
            Err(StorageError::MutationRevisionExhausted)
        ));
        let payload = executor
            .get_history_payload(inserted.id)
            .expect("查询耗尽原记录失败")
            .expect("耗尽原记录不应消失");
        assert_eq!(payload.text_content.as_deref(), Some("耗尽原文"));
        assert_eq!(payload.preview_text, "耗尽预览");
        assert_eq!(payload.copy_count, 1);
        assert_eq!(payload.copied_at, 100);

        drop(executor);
        remove_directory(&directory);
    }

    /// 验证同毫秒记录按 ID 倒序分页，游标跨页后既不重复也不遗漏。
    #[test]
    fn history_cursor_pages_are_stable_at_same_timestamp_boundary() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动查询存储线程失败");
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
        assert_eq!(first_page.items[0].content_hash, test_hash(43));
        assert_eq!(first_page.items[1].content_hash, test_hash(42));
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
        let executor = StorageExecutor::open_at(&directory).expect("启动边界查询线程失败");

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
            let executor = StorageExecutor::open_at(&directory).expect("启动筛选查询线程失败");
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
                    "INSERT INTO clipboard_items (item_type, preview_text, content_hash, copy_count, is_pinned, created_at, copied_at) VALUES ('binary', '二进制', ?1, 1, 1, 250, 250)",
                    params![vec![0x54_u8; 32]],
                )
                .expect("写入图片筛选记录失败");
        }

        let executor = StorageExecutor::open_at(&directory).expect("重新打开筛选查询线程失败");
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

        let binary = executor
            .query_history_summaries(HistoryQuery {
                item_type: Some("binary".to_owned()),
                limit: 10,
                ..HistoryQuery::default()
            })
            .expect("类型查询失败");
        assert_eq!(
            binary.items.iter().map(|item| item.id).collect::<Vec<_>>(),
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
        let executor = StorageExecutor::open_at(&directory).expect("启动筛选分页线程失败");
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
        let executor = StorageExecutor::open_at(&directory).expect("启动 payload 查询线程失败");
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
                    "INSERT INTO clipboard_items (item_type, text_content, preview_text, content_hash, copy_count, is_pinned, created_at, copied_at) VALUES ('binary', NULL, 'binary preview', ?1, 1, 1, 10, 20)",
                    params![vec![1_u8; 32]],
                )
                .expect("写入非文本测试记录失败");
        }

        let executor = StorageExecutor::open_at(&directory).expect("打开非文本查询线程失败");
        let summaries = executor
            .list_history_summaries(None, 10)
            .expect("读取非文本摘要失败");
        assert_eq!(summaries.items.len(), 1);
        assert_eq!(summaries.items[0].item_type, "binary");
        assert!(summaries.items[0].is_pinned);

        let payload = executor
            .get_history_payload(1)
            .expect("读取非文本 payload 失败")
            .expect("非文本记录不存在");
        assert_eq!(payload.item_type, "binary");
        assert_eq!(payload.text_content, None);
        assert_eq!(payload.content_hash, vec![1; 32]);
        assert_eq!(payload.source_exe, None);
        assert_eq!(payload.source_app, None);
        assert_eq!(payload.last_used_at, None);
        drop(executor);
        remove_directory(&directory);
    }

    /// 摘要哈希为 31 或 33 字节时整页失败，不能截断、补零或跳过损坏行。
    #[test]
    fn history_summary_rejects_invalid_hash_lengths() {
        for length in [31_usize, 33] {
            let directory = temporary_directory();
            let database_path = directory.join("clipboard.db");
            {
                let mut connection =
                    Connection::open(&database_path).expect("创建损坏哈希数据库失败");
                crate::storage::migration::migrate(&mut connection)
                    .expect("损坏哈希数据库迁移失败");
                connection
                    .execute(
                        "INSERT INTO clipboard_items (item_type, text_content, preview_text, content_hash, copy_count, is_pinned, created_at, copied_at) VALUES ('text', '正文', '预览', ?1, 1, 0, 1, 1)",
                        params![vec![7_u8; length]],
                    )
                    .expect("写入损坏哈希记录失败");
            }

            let executor = StorageExecutor::open_at(&directory).expect("打开损坏哈希查询线程失败");
            assert!(matches!(
                executor.list_history_summaries(None, 10),
                Err(crate::storage::StorageError::InvalidContentHashLength {
                    id: 1,
                    length: actual,
                }) if actual == length
            ));
            drop(executor);
            remove_directory(&directory);
        }
    }

    /// 验证未来 schema 版本会在 ready 前传播为启动错误，不会降级数据库。
    #[test]
    fn future_schema_version_prevents_startup() {
        let directory = temporary_directory();
        let database_path = directory.join("clipboard.db");
        {
            let connection = Connection::open(&database_path).expect("创建未来版本数据库失败");
            connection
                .pragma_update(None, "user_version", 3)
                .expect("写入未来版本失败");
        }

        let result = StorageExecutor::open_at(&directory);
        assert!(matches!(
            result,
            Err(crate::storage::StorageError::UnsupportedSchemaVersion(3))
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

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

use rusqlite::Connection;

use super::{migration, StorageError};

/// 存储线程命令队列的容量；当前原子只有探针和关闭命令，避免无限堆积。
const COMMAND_QUEUE_CAPACITY: usize = 4;

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

/// 可发送给存储线程的内部命令；不对外暴露连接、Statement 或 SQL 句柄。
enum StorageCommand {
    /// 在实际连接上执行只读线程归属探针。
    Inspect {
        /// 返回探针结果的有界通道。
        reply: SyncSender<Result<StorageStatus, StorageError>>,
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
            let join_result = self.join_worker();
            return if join_result.is_err() {
                Err(StorageError::WorkerPanicked)
            } else {
                Err(StorageError::ChannelClosed)
            };
        }

        match reply_receiver.recv() {
            Ok(result) => result,
            Err(_) => {
                let join_result = self.join_worker();
                if join_result.is_err() {
                    Err(StorageError::WorkerPanicked)
                } else {
                    Err(StorageError::ChannelClosed)
                }
            }
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

#[cfg(test)]
mod tests {
    //! 此测试模块验证执行器就绪、迁移隔离、坏 schema 传播和真实连接线程归属。

    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        thread,
    };

    use rusqlite::Connection;

    use super::StorageExecutor;

    static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    /// 生成只供当前测试使用的临时目录，避免并行测试共享用户数据库。
    fn temporary_directory() -> PathBuf {
        let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "clipboard-board-atom15-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("创建存储测试目录失败");
        directory
    }

    /// 释放当前测试自己创建的目录；调用方必须先丢弃执行器以释放 SQLite 文件句柄。
    fn remove_directory(directory: &Path) {
        fs::remove_dir_all(directory).expect("清理存储测试目录失败");
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

//! 此模块定义本地 SQLite 存储层的错误边界和单线程执行器公共接缝。
//!
//! 当前原子负责连接创建、v1 迁移、线程归属探针、文本事务 upsert、筛选摘要查询和复合游标；
//! 清理和 UI 接线由后续原子实现。

use std::{ffi::OsString, fmt, io, path::PathBuf};

mod migration;
mod worker;

pub use worker::{
    HistoryCursor, HistoryPage, HistoryPayload, HistoryQuery, HistorySummary, StorageExecutor,
    StorageStatus, TextUpsertInput, TextUpsertResult,
};

/// 存储层可能向应用层传播的初始化、迁移和线程生命周期错误。
#[derive(Debug)]
pub enum StorageError {
    /// 文件系统无法创建数据库目录或访问数据库文件。
    Io(io::Error),
    /// SQLite 返回了底层数据库错误。
    Sqlite(rusqlite::Error),
    /// 默认路径解析时没有得到用户级本地应用数据目录。
    MissingLocalAppData,
    /// 数据库声明了当前程序尚未支持的 schema 版本。
    UnsupportedSchemaVersion(i64),
    /// 已有表、字段或索引与固定 v1 契约不兼容。
    IncompatibleSchema(String),
    /// 存储线程在初始化阶段提前退出，调用方无法取得就绪结果。
    InitializationChannelClosed,
    /// 存储线程运行期间命令通道已经关闭。
    ChannelClosed,
    /// 查询请求超过固定页大小上限，避免一次性加载无界历史记录。
    InvalidPageSize {
        /// 调用方请求的页大小。
        requested: u32,
        /// 当前存储层允许的最大页大小。
        max: u32,
    },
    /// 存储线程发生未预期的 panic，无法安全继续使用连接。
    WorkerPanicked,
}

impl fmt::Display for StorageError {
    /// 将内部错误转换为不包含剪贴板正文的诊断描述。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "存储文件系统错误：{error}"),
            Self::Sqlite(error) => write!(formatter, "SQLite 错误：{error}"),
            Self::MissingLocalAppData => write!(formatter, "缺少 LOCALAPPDATA 环境变量"),
            Self::UnsupportedSchemaVersion(version) => {
                write!(formatter, "不支持的 SQLite schema 版本：{version}")
            }
            Self::IncompatibleSchema(detail) => {
                write!(formatter, "SQLite v1 schema 不兼容：{detail}")
            }
            Self::InitializationChannelClosed => write!(formatter, "存储线程未返回初始化结果"),
            Self::ChannelClosed => write!(formatter, "存储线程命令通道已关闭"),
            Self::InvalidPageSize { requested, max } => {
                write!(formatter, "历史查询页大小 {requested} 超过上限 {max}")
            }
            Self::WorkerPanicked => write!(formatter, "存储线程异常退出"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<io::Error> for StorageError {
    /// 将目录创建、线程创建和文件访问错误统一纳入存储错误边界。
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for StorageError {
    /// 将 SQLite 驱动错误统一纳入存储错误边界。
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// 返回默认的用户级数据库目录，不访问 SQLite，也不会创建目录。
pub fn default_data_directory() -> Result<PathBuf, StorageError> {
    data_directory_from_local_app_data(std::env::var_os("LOCALAPPDATA"))
}

/// 将环境变量值转换为默认数据目录；拆出纯函数以便无竞态地测试缺失环境变量。
fn data_directory_from_local_app_data(
    local_app_data: Option<OsString>,
) -> Result<PathBuf, StorageError> {
    let local_app_data = local_app_data.ok_or(StorageError::MissingLocalAppData)?;

    if local_app_data.is_empty() {
        return Err(StorageError::MissingLocalAppData);
    }

    Ok(PathBuf::from(local_app_data)
        .join("ClipboardBoard")
        .join("data"))
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证默认用户数据目录的缺失环境变量错误语义。

    use super::{data_directory_from_local_app_data, StorageError};

    /// 缺失 LOCALAPPDATA 时必须返回明确错误，不能静默写入当前目录。
    #[test]
    fn missing_local_app_data_is_rejected() {
        assert!(matches!(
            data_directory_from_local_app_data(None),
            Err(StorageError::MissingLocalAppData)
        ));
    }
}

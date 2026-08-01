//! 此模块定义配置深模块的公共接口，并隐藏 JSON 恢复、Windows 原子发布与线程生命周期。

use std::{ffi::OsString, fmt, io, path::PathBuf};

mod model;
mod persistence;
#[cfg(windows)]
mod windows_replace;
mod worker;

pub use model::{
    AppSettings, HistorySettings, PrivacySettings, RecordingPause, SettingsLoadSource,
    SettingsSnapshot,
};
pub use worker::{SettingsClient, SettingsWorker};

/// 配置模块可能返回的稳定错误；任何变体都不包含配置正文。
pub enum SettingsError {
    /// 默认路径解析时缺少用户级本地应用数据目录。
    MissingLocalAppData,
    /// 配置目录或文件操作失败。
    Io(io::Error),
    /// JSON 序列化失败。
    Serialization(serde_json::Error),
    /// 主文件和备份均存在但都无法恢复。
    UnrecoverableConfiguration,
    /// 主文件来自当前程序不支持的未来 schema。
    UnsupportedSchema(u64),
    /// 调用方提交了语义非法的已知设置字段。
    InvalidSettings(&'static str),
    /// expected revision 与 worker 当前 revision 不一致。
    RevisionConflict {
        /// 调用方提交的旧 revision。
        expected: u64,
        /// worker 当前权威 revision。
        actual: u64,
    },
    /// revision 已达到 u64 上限，保存必须在 IO 前停止。
    RevisionExhausted,
    /// 保存命令已入队，但回执断开使调用方不能判断结果。
    OutcomeUnknown,
    /// 工作线程命令通道已经断开。
    ChannelClosed,
    /// 所有者已进入关闭流程。
    SettingsClosing,
    /// worker 已被回收。
    SettingsClosed,
    /// finish_shutdown 前没有建立关闭线性化点。
    ShutdownNotBegun,
    /// 配置工作线程发生 panic。
    WorkerPanicked,
}

impl fmt::Display for SettingsError {
    /// 生成不包含配置正文的诊断描述。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingLocalAppData => write!(formatter, "缺少 LOCALAPPDATA 环境变量"),
            Self::Io(error) => write!(
                formatter,
                "配置文件系统错误：kind={:?}, code={:?}",
                error.kind(),
                error.raw_os_error()
            ),
            Self::Serialization(_) => write!(formatter, "配置 JSON 序列化失败"),
            Self::UnrecoverableConfiguration => write!(formatter, "主配置和备份均无法恢复"),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "配置 schema 版本 {version} 高于当前支持版本")
            }
            Self::InvalidSettings(field) => write!(formatter, "配置字段 {field} 超出合法范围"),
            Self::RevisionConflict { expected, actual } => {
                write!(
                    formatter,
                    "配置 revision 冲突：期望 {expected}，实际 {actual}"
                )
            }
            Self::RevisionExhausted => write!(formatter, "配置 revision 已耗尽"),
            Self::OutcomeUnknown => write!(formatter, "配置保存结果未知，必须读取快照对账"),
            Self::ChannelClosed => write!(formatter, "配置工作线程通道已关闭"),
            Self::SettingsClosing => write!(formatter, "配置工作线程正在关闭"),
            Self::SettingsClosed => write!(formatter, "配置工作线程已经关闭"),
            Self::ShutdownNotBegun => write!(formatter, "配置工作线程尚未进入关闭状态"),
            Self::WorkerPanicked => write!(formatter, "配置工作线程异常退出"),
        }
    }
}

impl fmt::Debug for SettingsError {
    /// Debug 与 Display 共用脱敏描述，禁止输出内部 JSON 或文件正文。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl std::error::Error for SettingsError {}

impl From<io::Error> for SettingsError {
    /// 把文件系统错误纳入配置错误接口。
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// 返回默认配置目录，不创建目录或文件。
pub fn default_config_directory() -> Result<PathBuf, SettingsError> {
    config_directory_from_local_app_data(std::env::var_os("LOCALAPPDATA"))
}

/// 用显式环境值构造默认目录，便于无进程环境竞态地测试。
fn config_directory_from_local_app_data(
    local_app_data: Option<OsString>,
) -> Result<PathBuf, SettingsError> {
    let local_app_data = local_app_data.ok_or(SettingsError::MissingLocalAppData)?;
    if local_app_data.is_empty() {
        return Err(SettingsError::MissingLocalAppData);
    }
    Ok(PathBuf::from(local_app_data)
        .join("ClipboardBoard")
        .join("config"))
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证默认目录纯函数不访问真实 LOCALAPPDATA。

    use std::{ffi::OsString, path::PathBuf};

    use super::{config_directory_from_local_app_data, SettingsError};

    /// 缺失和空值返回明确错误，合法值只拼接应用配置目录。
    #[test]
    fn resolves_config_directory_from_explicit_local_app_data() {
        assert!(matches!(
            config_directory_from_local_app_data(None),
            Err(SettingsError::MissingLocalAppData)
        ));
        assert!(matches!(
            config_directory_from_local_app_data(Some(OsString::new())),
            Err(SettingsError::MissingLocalAppData)
        ));
        assert_eq!(
            config_directory_from_local_app_data(Some(OsString::from(r"X:\isolated"))).unwrap(),
            PathBuf::from(r"X:\isolated\ClipboardBoard\config")
        );
    }

    /// Display/Debug 不回显底层错误消息中可能携带的配置正文。
    #[test]
    fn error_diagnostics_do_not_leak_configuration_body() {
        let secret = "clipboard-secret-body";
        let error = SettingsError::Io(std::io::Error::other(secret));
        assert!(!error.to_string().contains(secret));
        assert!(!format!("{error:?}").contains(secret));
    }
}

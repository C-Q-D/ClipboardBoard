//! 非 Windows 平台的开机启动 stub；只用于保持跨平台编译，不执行任何系统写入。

pub mod windows {
    //! 保持与 Windows 路径一致的最小命名空间；真实注册表实现仅在 Windows 编译。

    use std::{
        ffi::{OsStr, OsString},
        fmt::{Display, Formatter},
    };

    /// 非 Windows 目标上的稳定不可用错误。
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum StartupErrorCategory {
        /// 当前平台没有 HKCU Run 实现。
        Unavailable,
        /// 输入包含不能安全转义的字符。
        InvalidInput,
    }

    /// 非 Windows stub 错误；不伪造已写入注册表。
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct StartupError {
        /// 稳定错误类别。
        pub category: StartupErrorCategory,
    }

    impl Display for StartupError {
        /// 生成不包含路径的稳定描述。
        fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
            match self.category {
                StartupErrorCategory::Unavailable => write!(formatter, "当前平台不支持开机启动"),
                StartupErrorCategory::InvalidInput => write!(formatter, "启动设置输入非法"),
            }
        }
    }

    impl std::error::Error for StartupError {}

    /// 非 Windows 目标使用的空后端标记。
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct WindowsRegistryBackend;

    impl WindowsRegistryBackend {
        /// 构造不可用 stub。
        pub const fn new() -> Self {
            Self
        }
    }

    /// 非 Windows 目标只验证输入边界，并明确返回不可用错误。
    pub fn quote_windows_single_argument(_path: &OsStr) -> Result<OsString, StartupError> {
        Err(StartupError {
            category: StartupErrorCategory::Unavailable,
        })
    }
}

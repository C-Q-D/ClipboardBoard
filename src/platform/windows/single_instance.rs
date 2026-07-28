//! 此模块负责保证当前 Windows 用户只有一个 ClipboardBoard 后台实例。
//!
//! 主实例持有 `Local` 命名互斥体直到应用退出；后续实例不会创建 UI、数据库或热键，
//! 而是通过主实例已有的 message-only HWND 投递“打开面板”消息后立即结束。

use super::system_window::{OPEN_PANEL_MESSAGE, WINDOW_CLASS_NAME};
use std::fmt::{Display, Formatter};
use std::ptr::{null, null_mut};
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowExW, PostMessageW, HWND_MESSAGE};

/// 使用会话级 `Local` 命名空间，避免要求管理员权限或跨会话共享状态。
#[cfg(test)]
const MUTEX_NAME_TEXT: &str = "Local\\ClipboardBoard.Instance.v1";
const MUTEX_NAME: windows_sys::core::PCWSTR =
    windows_sys::core::w!("Local\\ClipboardBoard.Instance.v1");

/// 主实例窗口刚创建时可能尚未进入可查找状态，因此只在有限窗口内重试。
const NOTIFY_ATTEMPTS: usize = 100;
const NOTIFY_INTERVAL: Duration = Duration::from_millis(10);

/// 单实例初始化失败时返回的可审计错误。
#[derive(Debug, Eq, PartialEq)]
pub enum SingleInstanceError {
    /// Windows 创建或查询命名对象失败。
    Windows { operation: &'static str, code: u32 },
    /// 互斥体已经存在，但主实例的消息窗口在有界等待后仍不可见。
    PrimaryWindowUnavailable { attempts: usize },
    /// 找到主实例窗口，但投递消息失败。
    PrimaryWindowMessage { code: u32 },
    /// 通知失败后重新夺取互斥体的恢复尝试也失败；两个错误都保留用于诊断。
    Recovery {
        notify: Box<SingleInstanceError>,
        acquire: Box<SingleInstanceError>,
    },
}

impl Display for SingleInstanceError {
    /// 将单实例错误转换成面向用户和日志的中文诊断信息。
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Windows { operation, code } => {
                write!(formatter, "Windows 调用 {operation} 失败，错误码 {code}")
            }
            Self::PrimaryWindowUnavailable { attempts } => {
                write!(formatter, "主实例消息窗口在重试 {attempts} 次后仍不可用")
            }
            Self::PrimaryWindowMessage { code } => {
                write!(formatter, "向主实例投递打开面板消息失败，错误码 {code}")
            }
            Self::Recovery { notify, acquire } => {
                write!(
                    formatter,
                    "主实例通知失败（{notify}），重新获取单实例资格也失败（{acquire}）"
                )
            }
        }
    }
}

impl std::error::Error for SingleInstanceError {
    /// 暴露恢复路径中保留的底层错误，便于调用方继续记录诊断链。
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Recovery { notify, .. } => Some(notify.as_ref()),
            _ => None,
        }
    }
}

/// 本次启动在单实例协议中的角色。
pub enum SingleInstanceRole {
    /// 主实例持有此 guard，直到 UI 事件循环和热键线程全部结束。
    Primary(SingleInstanceGuard),
    /// 已通知运行中的主实例，本进程不再创建任何后台资源。
    Secondary,
}

/// 持有命名互斥体内核句柄的 RAII guard。
pub struct SingleInstanceGuard {
    /// 句柄必须留在主线程，直到应用退出时由 Drop 关闭。
    handle: HANDLE,
}

impl Drop for SingleInstanceGuard {
    /// 释放互斥体，使下一次启动可以重新成为主实例。
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
            self.handle = null_mut();
        }
    }
}

/// 先获取用户级互斥体；若已有主实例，则通知它并让当前进程退出。
pub fn acquire_or_activate() -> Result<SingleInstanceRole, SingleInstanceError> {
    let (handle, already_exists) = create_mutex()?;
    if !already_exists {
        return Ok(SingleInstanceRole::Primary(SingleInstanceGuard { handle }));
    }

    // 已存在的句柄不是本进程所有权；不能调用 ReleaseMutex，只能立即关闭句柄。
    unsafe {
        let _ = CloseHandle(handle);
    }

    match notify_primary() {
        Ok(()) => Ok(SingleInstanceRole::Secondary),
        Err(notify_error) => match create_mutex() {
            Ok((retry_handle, false)) => {
                // 主实例可能在通知竞态中退出；新的句柄代表当前进程安全接管。
                Ok(SingleInstanceRole::Primary(SingleInstanceGuard {
                    handle: retry_handle,
                }))
            }
            Ok((retry_handle, true)) => {
                unsafe {
                    let _ = CloseHandle(retry_handle);
                }
                Err(notify_error)
            }
            Err(acquire_error) => Err(SingleInstanceError::Recovery {
                notify: Box::new(notify_error),
                acquire: Box::new(acquire_error),
            }),
        },
    }
}

/// 创建命名互斥体，并在调用后立即读取 `GetLastError` 判断是否已有实例。
fn create_mutex() -> Result<(HANDLE, bool), SingleInstanceError> {
    let handle = unsafe { CreateMutexW(null(), 1, MUTEX_NAME) };
    if handle.is_null() {
        return Err(SingleInstanceError::Windows {
            operation: "CreateMutexW",
            code: unsafe { GetLastError() },
        });
    }

    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    Ok((handle, already_exists))
}

/// 在主窗口启动竞态期间查找 message-only HWND 并投递无敏感负载的打开消息。
fn notify_primary() -> Result<(), SingleInstanceError> {
    let mut last_post_error = None;
    for _ in 0..NOTIFY_ATTEMPTS {
        let window = unsafe { FindWindowExW(HWND_MESSAGE, null_mut(), WINDOW_CLASS_NAME, null()) };
        if !window.is_null() {
            let posted = unsafe { PostMessageW(window, OPEN_PANEL_MESSAGE, 0, 0) };
            if posted != 0 {
                return Ok(());
            }
            last_post_error = Some(unsafe { GetLastError() });
        }
        thread::sleep(NOTIFY_INTERVAL);
    }

    match last_post_error {
        Some(code) => Err(SingleInstanceError::PrimaryWindowMessage { code }),
        None => Err(SingleInstanceError::PrimaryWindowUnavailable {
            attempts: NOTIFY_ATTEMPTS,
        }),
    }
}

/// 将 `GetLastError` 的结果转换为明确的“已有实例”布尔值，便于单元测试。
#[cfg(test)]
fn mutex_is_existing(last_error: u32) -> bool {
    last_error == ERROR_ALREADY_EXISTS
}

#[cfg(test)]
mod tests {
    //! 此测试模块锁定单实例协议的名称、错误分类和重试边界，不启动真实进程。

    use super::{mutex_is_existing, MUTEX_NAME_TEXT, NOTIFY_ATTEMPTS};
    use windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS;

    /// 互斥体必须使用 Local 命名空间，避免把单实例协议扩展到管理员会话。
    #[test]
    fn 互斥体名称是用户会话级() {
        assert!(MUTEX_NAME_TEXT.starts_with("Local\\"));
        assert!(MUTEX_NAME_TEXT.contains("ClipboardBoard.Instance.v1"));
    }

    /// 只有 Windows 明确返回 ERROR_ALREADY_EXISTS 时才进入二次启动分支。
    #[test]
    fn 互斥体已存在判定稳定() {
        assert!(mutex_is_existing(ERROR_ALREADY_EXISTS));
        assert!(!mutex_is_existing(0));
        assert!(!mutex_is_existing(5));
    }

    /// 通知重试必须有界，避免第二进程无限等待主窗口。
    #[test]
    fn 通知重试次数有界() {
        assert_eq!(NOTIFY_ATTEMPTS, 100);
    }
}

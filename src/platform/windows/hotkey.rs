//! 此模块负责启动、停止和诊断 Alt+V 全局热键线程。
//!
//! HWND 和 Win32 消息泵始终属于专用线程；业务层只得到可停止的管理器，
//! 不会持有或跨线程传递原生窗口句柄。

use super::system_window;
use crate::clipboard::{ClipboardCaptureInbox, ClipboardWriteExpectationStore};
use std::fmt::{Display, Formatter};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

/// Alt+V 热键的固定规格；用户可配置快捷键将在后续设置原子实现。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HotkeySpec {
    /// RegisterHotKey 使用的进程内标识。
    pub(crate) id: i32,
    /// Win32 修饰键位掩码。
    pub(crate) modifiers: u32,
    /// Win32 虚拟键码。
    pub(crate) virtual_key: u32,
    /// 用于错误提示的用户可读名称。
    pub(crate) label: &'static str,
}

/// 当前版本的默认全局快捷键。
pub(crate) const DEFAULT_HOTKEY: HotkeySpec = HotkeySpec {
    id: 0x4342,
    // 显隐切换必须把一次物理按住压缩成一个事件，避免系统键盘重复令面板来回闪烁。
    modifiers: windows_sys::Win32::UI::Input::KeyboardAndMouse::MOD_ALT
        | windows_sys::Win32::UI::Input::KeyboardAndMouse::MOD_NOREPEAT,
    virtual_key: windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_V as u32,
    label: "Alt + V",
};

/// 热键初始化或关闭过程中可以向上层报告的错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HotkeyError {
    /// 创建后台消息线程失败。
    ThreadStart(String),
    /// Win32 调用失败，保留原始错误码方便诊断。
    Windows { operation: &'static str, code: u32 },
    /// 快捷键已经被其他进程注册。
    RegistrationConflict { shortcut: &'static str },
    /// 热键线程没有按协议返回启动结果。
    StartupChannelClosed,
    /// 热键线程异常退出。
    ThreadPanicked,
    /// 托盘注册、菜单或托盘到 UI 的事件投递失败。
    Tray(String),
}

impl Display for HotkeyError {
    /// 输出面向用户和日志的明确错误，不隐藏热键冲突。
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ThreadStart(error) => write!(formatter, "无法启动全局热键线程：{error}"),
            Self::Windows { operation, code } => {
                write!(formatter, "Windows 调用 {operation} 失败，错误码 {code}")
            }
            Self::RegistrationConflict { shortcut } => {
                write!(formatter, "全局快捷键 {shortcut} 已被其他程序占用")
            }
            Self::StartupChannelClosed => write!(formatter, "全局热键线程未返回启动结果"),
            Self::ThreadPanicked => write!(formatter, "全局热键线程异常退出"),
            Self::Tray(error) => write!(formatter, "系统托盘操作失败：{error}"),
        }
    }
}

impl std::error::Error for HotkeyError {}

/// 持有热键消息线程的生命周期控制器。
pub struct HotkeyManager {
    /// 用于向拥有 HWND 的线程投递 WM_QUIT。
    thread_id: u32,
    /// 线程句柄只在停止时 join，确保退出前注销热键。
    join_handle: Option<JoinHandle<Result<(), HotkeyError>>>,
    /// 从 ClipboardIO worker 接收最新捕获结果以及 UI 仅复制命令的公共桥，供历史结果泵消费。
    clipboard_inbox: ClipboardCaptureInbox,
}

impl HotkeyManager {
    /// 创建消息线程并等待它完成 HWND、热键注册和消息队列初始化。
    pub fn start() -> Result<Self, HotkeyError> {
        Self::start_with_write_expectations(ClipboardWriteExpectationStore::new())
    }

    /// 使用调用方共享的写回预期启动消息线程，确保自身复制事件由捕获 worker 消费。
    pub fn start_with_write_expectations(
        write_expectations: ClipboardWriteExpectationStore,
    ) -> Result<Self, HotkeyError> {
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let clipboard_inbox = ClipboardCaptureInbox::new();
        let worker_inbox = clipboard_inbox.clone();
        let worker_expectations = write_expectations;
        let join_handle = thread::Builder::new()
            .name("clipboard-board-hotkey".to_owned())
            .spawn(move || {
                system_window::run(
                    DEFAULT_HOTKEY,
                    ready_sender,
                    worker_inbox,
                    worker_expectations,
                )
            })
            .map_err(|error| HotkeyError::ThreadStart(error.to_string()))?;

        match ready_receiver.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                join_handle: Some(join_handle),
                clipboard_inbox,
            }),
            Ok(Err(error)) => {
                let _ = join_handle.join();
                Err(error)
            }
            Err(_) => {
                let _ = join_handle.join();
                Err(HotkeyError::StartupChannelClosed)
            }
        }
    }

    /// 返回捕获结果桥副本；调用方只能消费拥有型结果，不会取得消息线程或 HWND 所有权。
    pub fn clipboard_inbox(&self) -> ClipboardCaptureInbox {
        self.clipboard_inbox.clone()
    }

    /// 请求消息线程退出并等待其完成注销，避免留下僵尸热键。
    pub fn stop(mut self) -> Result<(), HotkeyError> {
        let post_result = unsafe {
            if windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                self.thread_id,
                windows_sys::Win32::UI::WindowsAndMessaging::WM_QUIT,
                0,
                0,
            ) == 0
            {
                Err(HotkeyError::Windows {
                    operation: "PostThreadMessageW",
                    code: windows_sys::Win32::Foundation::GetLastError(),
                })
            } else {
                Ok(())
            }
        };

        let join_result = self
            .join_handle
            .take()
            .expect("热键管理器必须持有线程句柄")
            .join()
            .map_err(|_| HotkeyError::ThreadPanicked)
            .and_then(|result| result);

        post_result.and(join_result)
    }
}

impl Drop for HotkeyManager {
    /// 异常展开时尽力唤醒消息线程；正常路径由 `stop` 负责 join 和错误传播。
    fn drop(&mut self) {
        if self.join_handle.is_some() {
            unsafe {
                let _ = windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                    self.thread_id,
                    windows_sys::Win32::UI::WindowsAndMessaging::WM_QUIT,
                    0,
                    0,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块锁定默认快捷键的可审计常量，避免后续修改时静默改变用户入口。

    use super::DEFAULT_HOTKEY;

    /// Alt+V 必须由 Alt、V 和禁止重复标志组成，ID 不能使用系统保留的零值。
    #[test]
    fn 默认快捷键规格稳定() {
        assert_eq!(
            DEFAULT_HOTKEY.modifiers,
            windows_sys::Win32::UI::Input::KeyboardAndMouse::MOD_ALT
                | windows_sys::Win32::UI::Input::KeyboardAndMouse::MOD_NOREPEAT
        );
        assert_eq!(DEFAULT_HOTKEY.virtual_key, 86);
        assert_ne!(DEFAULT_HOTKEY.id, 0);
        assert_eq!(DEFAULT_HOTKEY.label, "Alt + V");
    }
}

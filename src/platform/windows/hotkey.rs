//! 此模块实现全局快捷键配置、消息线程事务协议和异步保存所有权。
//!
//! 原生 HWND 只在 `system_window` 所在线程使用；本文件只通过有界消息桥传递拥有型
//! DTO。设置保存由独立事务线程执行，避免 Slint/UI 或 Win32 消息线程等待磁盘。

use super::system_window;
use crate::clipboard::{ClipboardCaptureInbox, ClipboardWriteExpectationStore};
use crate::privacy::{GateMode, PauseCommandSender, RecordingGate};
use crate::settings::{
    validate_hotkey, AppSettings, HotkeySettings, SettingsClient, SettingsError, SettingsSnapshot,
};
use std::collections::{BTreeSet, HashMap};
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// 进程内注册 ID 的最大值；较小的正数范围便于审计和拒绝系统保留值。
pub(crate) const HOTKEY_ID_MAX: i32 = 0xBFFF;
/// 热键线程命令唤醒消息；不与托盘和二次启动消息复用。
pub(crate) const HOTKEY_COMMAND_MESSAGE: u32 =
    windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 3;
/// 默认热键在 Win32 消息中使用的稳定 ID，仅用于启动和兼容测试。
pub(crate) const DEFAULT_HOTKEY_ID: i32 = 0x4342;

/// Windows 热键注册规格；进程内 ID 和展示标签不写入 Settings JSON。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HotkeySpec {
    /// RegisterHotKey 使用的本进程 ID。
    pub(crate) id: i32,
    /// Windows 修饰键位掩码。
    pub(crate) modifiers: u32,
    /// Windows 虚拟键码。
    pub(crate) virtual_key: u32,
    /// 用户可读的规范化组合名称。
    pub(crate) label: String,
}

impl HotkeySpec {
    /// 从已通过设置校验的持久化 DTO 构造一次性进程规格。
    pub(crate) fn from_settings(id: i32, settings: &HotkeySettings) -> Result<Self, HotkeyError> {
        validate_hotkey(settings).map_err(|_| HotkeyError::InvalidSettings)?;
        if !(1..=HOTKEY_ID_MAX).contains(&id) {
            return Err(HotkeyError::InvalidId);
        }
        Ok(Self {
            id,
            modifiers: settings.modifiers,
            virtual_key: settings.virtual_key,
            label: settings.label(),
        })
    }
}

/// 生成默认 Alt+V 规格；不使用可变全局字符串，避免跨线程共享标签所有权。
pub(crate) fn default_hotkey_spec() -> HotkeySpec {
    HotkeySpec::from_settings(DEFAULT_HOTKEY_ID, &HotkeySettings::default())
        .expect("默认快捷键必须通过模型校验")
}

/// 热键消息线程和事务所有者共享的运行时安全信号。
#[derive(Clone)]
pub(crate) struct HotkeyRuntimeSignal {
    /// ActiveOld/Candidate/None/Unknown 状态编码。
    state: Arc<AtomicU8>,
    /// 当前唯一允许产生 WM_HOTKEY 的 active ID；0 表示没有可用 ID。
    active_id: Arc<AtomicI32>,
}

impl HotkeyRuntimeSignal {
    /// 创建初始状态；启动时由消息线程根据注册结果设置真实值。
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(HotkeyRuntimeState::None as u8)),
            active_id: Arc::new(AtomicI32::new(0)),
        }
    }

    /// 发布状态和 ID；先写 ID 再写状态，使读线程不会看到新状态旧 ID。
    pub(crate) fn set(&self, state: HotkeyRuntimeState, active_id: i32) {
        self.active_id.store(active_id, Ordering::Release);
        self.state.store(state as u8, Ordering::Release);
    }

    /// 读取状态；未知编码按最安全的 Unknown 处理。
    pub(crate) fn state(&self) -> HotkeyRuntimeState {
        HotkeyRuntimeState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// 读取当前 active ID。
    pub(crate) fn active_id(&self) -> i32 {
        self.active_id.load(Ordering::Acquire)
    }
}

/// WM_HOTKEY 可用性；Unknown/None 时消息全部丢弃。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum HotkeyRuntimeState {
    /// 旧配置仍可用，事务正在候选/保存阶段。
    ActiveOld = 0,
    /// 新候选已发布为当前 active。
    Candidate = 1,
    /// 启动或注册失败，没有可用热键但托盘仍运行。
    None = 2,
    /// 发布结果不明或需要人工/重启对账，所有热键消息停止。
    Unknown = 3,
}

impl HotkeyRuntimeState {
    /// 将原子整数转换为 fail-closed 状态。
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::ActiveOld,
            1 => Self::Candidate,
            2 => Self::None,
            _ => Self::Unknown,
        }
    }
}

/// 注册事务对外可观察状态；只返回状态，不暴露配置正文。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HotkeyTransactionStatus {
    /// 没有在途事务，当前 active 仍可正常使用。
    Idle,
    /// 候选注册/保存/发布正在进行。
    Busy,
    /// 启动或候选注册冲突，当前没有可用热键。
    HotkeyUnavailable,
    /// 事务后置状态不明，必须重启或显式对账。
    ReconcileRequired,
    /// active 状态未知，任何热键输入都被忽略。
    ActiveUnknown,
}

/// 热键事务入口的非阻塞错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HotkeyTransactionError {
    /// 已有一个事务在途，不覆盖前一个候选。
    Busy,
    /// 当前状态需要对账，拒绝新的快捷键修改。
    ReconcileRequired,
    /// 事务 worker 已关闭。
    Closed,
    /// 组合或 ID 不符合持久化/Win32 边界。
    InvalidSettings,
}

impl Display for HotkeyTransactionError {
    /// 生成不含配置正文的稳定错误描述。
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(formatter, "快捷键事务正在处理中"),
            Self::ReconcileRequired => write!(formatter, "快捷键状态需要重启后对账"),
            Self::Closed => write!(formatter, "快捷键事务入口已经关闭"),
            Self::InvalidSettings => write!(formatter, "快捷键组合不合法"),
        }
    }
}

impl std::error::Error for HotkeyTransactionError {}

/// 初始化/关闭过程中可向上层报告的错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HotkeyError {
    /// 创建后台消息线程失败。
    ThreadStart(String),
    /// Win32 调用失败，保留错误码方便诊断。
    Windows { operation: &'static str, code: u32 },
    /// 快捷键被其他程序动态占用。
    RegistrationConflict { shortcut: String },
    /// 设置语义不合法。
    InvalidSettings,
    /// 应用注册 ID 不在允许范围。
    InvalidId,
    /// 热键线程没有按协议返回启动结果。
    StartupChannelClosed,
    /// 热键线程异常退出。
    ThreadPanicked,
    /// 托盘注册、菜单或托盘到 UI 的事件投递失败。
    Tray(String),
    /// 事务命令队列已经关闭。
    CommandChannelClosed,
    /// 事务命令无法投递到 HWND 所属线程。
    ThreadMessageFailed(u32),
    /// 事务回执在消息线程关闭前未到达。
    AckTimeout,
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
            Self::InvalidSettings => write!(formatter, "全局快捷键配置不合法"),
            Self::InvalidId => write!(formatter, "全局快捷键注册 ID 不合法"),
            Self::StartupChannelClosed => write!(formatter, "全局热键线程未返回启动结果"),
            Self::ThreadPanicked => write!(formatter, "全局热键线程异常退出"),
            Self::Tray(error) => write!(formatter, "系统托盘操作失败：{error}"),
            Self::CommandChannelClosed => write!(formatter, "全局热键命令通道已关闭"),
            Self::ThreadMessageFailed(code) => {
                write!(formatter, "无法唤醒全局热键线程，错误码 {code}")
            }
            Self::AckTimeout => write!(formatter, "全局热键线程未返回事务回执"),
        }
    }
}

impl std::error::Error for HotkeyError {}

/// 消息线程查询回执中的 active 归属；Unknown 表示不可安全推断。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum QueryActiveState {
    /// 旧配置仍是 active，候选尚未发布。
    Old,
    /// 候选已登记或已发布为 active。
    Candidate,
    /// 当前没有 active。
    None,
    /// HWND/消息线程状态未知。
    Unknown,
}

/// 消息线程事务状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThreadTransactionState {
    /// 尚未登记。
    NotFound,
    /// 候选已登记但尚未发布。
    CandidateRegistered,
    /// 已发布为当前 active。
    Published,
    /// 已取消或 generation 过期。
    Cancelled,
}

/// QueryTransaction 的完整回执；active/candidate ID 和 generation 一起用于对账。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ThreadQueryResult {
    /// 查询到的事务状态。
    pub(crate) transaction: ThreadTransactionState,
    /// 当前 active 的归属。
    pub(crate) active_state: QueryActiveState,
    /// 当前 active ID，0 表示没有或未知。
    pub(crate) active_id: i32,
    /// 当前候选 ID，0 表示没有或未知。
    pub(crate) candidate_id: i32,
    /// 当前事务 generation。
    pub(crate) generation: u64,
}

/// 消息线程给事务 worker 的拥有型回执。
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) enum HotkeyThreadAck {
    /// 候选注册成功。
    CandidateRegistered {
        /// 事务 ID。
        transaction_id: u64,
        /// generation。
        generation: u64,
        /// 新候选注册 ID。
        candidate_id: i32,
    },
    /// 候选注册失败，旧 active 未受影响。
    RegistrationFailed {
        /// 事务 ID。
        transaction_id: u64,
        /// generation。
        generation: u64,
        /// 失败原因。
        error: HotkeyError,
    },
    /// 发布成功。
    Published {
        /// 事务 ID。
        transaction_id: u64,
        /// generation。
        generation: u64,
        /// 当前 active ID。
        active_id: i32,
    },
    /// 候选注销完成。
    CandidateDropped {
        /// 事务 ID。
        transaction_id: u64,
        /// generation。
        generation: u64,
        /// 注销是否成功；失败会登记 stale。
        success: bool,
    },
    /// 事务取消完成。
    Cancelled {
        /// 事务 ID。
        transaction_id: u64,
        /// generation。
        generation: u64,
    },
    /// 查询回执。
    Query {
        /// 事务 ID。
        transaction_id: u64,
        /// 查询结果。
        result: ThreadQueryResult,
    },
    /// Shutdown 完成。
    ShutdownComplete {
        /// 注销失败的 stale 数量，仅用于诊断和测试。
        stale_count: usize,
    },
}

/// 消息线程命令；每个命令携带自己的有界回执发送端，避免全局共享 HWND。
pub(crate) enum HotkeyThreadCommand {
    /// 登记候选，线程分配新的正 ID。
    RegisterCandidate {
        /// 事务 ID。
        transaction_id: u64,
        /// 事务 generation。
        generation: u64,
        /// 候选设置。
        settings: HotkeySettings,
        /// 回执端。
        reply: mpsc::SyncSender<HotkeyThreadAck>,
    },
    /// 发布已保存的候选为 active。
    PublishActive {
        /// 事务 ID。
        transaction_id: u64,
        /// 事务 generation。
        generation: u64,
        /// 候选 ID。
        candidate_id: i32,
        /// 保存后的设置 revision。
        settings_revision: u64,
        /// 回执端。
        reply: mpsc::SyncSender<HotkeyThreadAck>,
    },
    /// 查询事务和 active/candidate 归属。
    QueryTransaction {
        /// 事务 ID。
        transaction_id: u64,
        /// 回执端。
        reply: mpsc::SyncSender<HotkeyThreadAck>,
    },
    /// 写入 tombstone 并取消候选；迟到 publish 只能收到 Cancelled。
    CancelTransaction {
        /// 事务 ID。
        transaction_id: u64,
        /// generation。
        generation: u64,
        /// 回执端。
        reply: mpsc::SyncSender<HotkeyThreadAck>,
    },
    /// 删除尚未发布的候选。
    DropCandidate {
        /// 事务 ID。
        transaction_id: u64,
        /// generation。
        generation: u64,
        /// 候选 ID。
        candidate_id: i32,
        /// 回执端。
        reply: mpsc::SyncSender<HotkeyThreadAck>,
    },
    /// 先拒绝新事务并注销 current/candidate/stale。
    Shutdown {
        /// 回执端。
        reply: mpsc::SyncSender<HotkeyThreadAck>,
    },
}

/// 只在消息线程创建的命令发送桥；发送后通过 PostThreadMessageW 唤醒该线程。
#[derive(Clone)]
pub(crate) struct HotkeyThreadSender {
    /// 有界命令队列。
    sender: mpsc::SyncSender<HotkeyThreadCommand>,
    /// 消息线程 ID，只用于 PostThreadMessageW，不跨线程传 HWND。
    thread_id: u32,
}

impl HotkeyThreadSender {
    /// 创建消息线程命令桥。
    pub(crate) fn new(sender: mpsc::SyncSender<HotkeyThreadCommand>, thread_id: u32) -> Self {
        Self { sender, thread_id }
    }

    /// 入队并唤醒 HWND 所属线程；PostThreadMessage 失败时调用方进入未知状态。
    pub(crate) fn send(&self, command: HotkeyThreadCommand) -> Result<(), HotkeyError> {
        self.sender
            .send(command)
            .map_err(|_| HotkeyError::CommandChannelClosed)?;
        let posted = unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                self.thread_id,
                HOTKEY_COMMAND_MESSAGE,
                0,
                0,
            )
        };
        if posted == 0 {
            return Err(HotkeyError::ThreadMessageFailed(unsafe {
                windows_sys::Win32::Foundation::GetLastError()
            }));
        }
        Ok(())
    }

    /// 在首次 RegisterHotKey 的唤醒失败后追加 tombstone；若消息线程稍后恢复，
    /// 先入队的登记命令也会被后续 CancelTransaction 拒绝，避免迟到命令重新启用热键。
    pub(crate) fn cancel_best_effort(&self, transaction_id: u64, generation: u64) {
        let (reply, _receiver) = mpsc::sync_channel(1);
        if self
            .sender
            .try_send(HotkeyThreadCommand::CancelTransaction {
                transaction_id,
                generation,
                reply,
            })
            .is_err()
        {
            return;
        }
        unsafe {
            let _ = windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                self.thread_id,
                HOTKEY_COMMAND_MESSAGE,
                0,
                0,
            );
        }
    }
}

/// 事务 worker 的内部命令；UI 只调用 `request`，不接触磁盘或 HWND。
enum OwnerCommand {
    /// 提交一个候选快捷键。
    Submit(HotkeySettings),
    /// 请求 worker 停止并等待当前事务收口。
    Shutdown,
}

/// 托盘/设置壳使用的轻量快捷键提交句柄；只持有有界队列和只读状态，不拥有 HWND。
#[derive(Clone)]
pub struct HotkeyRequestHandle {
    /// 非阻塞事务命令队列。
    sender: mpsc::SyncSender<OwnerCommand>,
    /// 当前事务状态快照。
    status: Arc<Mutex<HotkeyTransactionStatus>>,
    /// 最近一次已提交快捷键的展示标签。
    active_label: Arc<Mutex<String>>,
}

impl HotkeyRequestHandle {
    /// 非阻塞提交预置/用户选择的快捷键；Busy 或对账状态不会覆盖在途事务。
    pub fn request(&self, settings: HotkeySettings) -> Result<(), HotkeyTransactionError> {
        validate_hotkey(&settings).map_err(|_| HotkeyTransactionError::InvalidSettings)?;
        let mut status = self
            .status
            .lock()
            .map_err(|_| HotkeyTransactionError::Closed)?;
        let previous_status = *status;
        match previous_status {
            HotkeyTransactionStatus::Busy | HotkeyTransactionStatus::ActiveUnknown => {
                return Err(HotkeyTransactionError::Busy)
            }
            HotkeyTransactionStatus::ReconcileRequired => {
                return Err(HotkeyTransactionError::ReconcileRequired)
            }
            HotkeyTransactionStatus::Idle | HotkeyTransactionStatus::HotkeyUnavailable => {}
        }
        // 先占用 Busy，再入队，避免两个并发调用在 worker 观察 Busy 之前同时排队。
        *status = HotkeyTransactionStatus::Busy;
        drop(status);
        match self.sender.try_send(OwnerCommand::Submit(settings)) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => {
                set_status(&self.status, previous_status);
                Err(HotkeyTransactionError::Busy)
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                set_status(&self.status, HotkeyTransactionStatus::ActiveUnknown);
                Err(HotkeyTransactionError::Closed)
            }
        }
    }

    /// 返回当前事务状态；锁损坏时 fail-closed 为 ActiveUnknown。
    pub fn status(&self) -> HotkeyTransactionStatus {
        self.status
            .lock()
            .map(|status| *status)
            .unwrap_or(HotkeyTransactionStatus::ActiveUnknown)
    }

    /// 返回最近一次已提交组合的展示标签，不暴露配置正文。
    pub fn active_label(&self) -> String {
        self.active_label
            .lock()
            .map(|label| label.clone())
            .unwrap_or_else(|_| "快捷键未知".to_owned())
    }
}

/// 进程内唯一的托盘设置入口；只保存无 HWND 的轻量句柄，退出时显式清空。
static GLOBAL_HOTKEY_REQUEST_HANDLE: OnceLock<Mutex<Option<HotkeyRequestHandle>>> = OnceLock::new();

/// 安装当前进程的托盘快捷键设置句柄；重复安装只替换同一主实例的旧句柄。
pub(crate) fn install_global_hotkey_request_handle(handle: HotkeyRequestHandle) {
    let slot = GLOBAL_HOTKEY_REQUEST_HANDLE.get_or_init(|| Mutex::new(None));
    if let Ok(mut current) = slot.lock() {
        *current = Some(handle);
    }
}

/// 读取托盘设置入口的轻量句柄；锁损坏时返回 None，托盘仍可打开/退出。
pub(crate) fn global_hotkey_request_handle() -> Option<HotkeyRequestHandle> {
    GLOBAL_HOTKEY_REQUEST_HANDLE
        .get()
        .and_then(|slot| slot.lock().ok()?.clone())
}

/// 清除托盘设置入口，避免停止后的菜单回调继续提交到已关闭 worker。
pub(crate) fn clear_global_hotkey_request_handle() {
    if let Some(slot) = GLOBAL_HOTKEY_REQUEST_HANDLE.get() {
        if let Ok(mut current) = slot.lock() {
            *current = None;
        }
    }
}

/// 管理快捷键修改事务的独立所有者；同一时刻只允许一个事务在途。
pub(crate) struct HotkeyTransactionOwner {
    /// UI/托盘使用的非阻塞有界入口。
    sender: mpsc::SyncSender<OwnerCommand>,
    /// 只读状态快照。
    status: Arc<Mutex<HotkeyTransactionStatus>>,
    /// 已提交快捷键的展示标签。
    active_label: Arc<Mutex<String>>,
    /// worker 句柄，仅由 owner 回收。
    join: Option<JoinHandle<()>>,
}

impl HotkeyTransactionOwner {
    /// 启动事务 worker；保存使用 SettingsClient clone，不阻塞消息线程。
    pub(crate) fn start(
        settings_client: SettingsClient,
        initial_snapshot: SettingsSnapshot,
        thread_sender: HotkeyThreadSender,
        signal: HotkeyRuntimeSignal,
        initial_status: HotkeyTransactionStatus,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel(2);
        let status = Arc::new(Mutex::new(initial_status));
        let active_label = Arc::new(Mutex::new(initial_snapshot.settings().hotkey.label()));
        let worker_status = Arc::clone(&status);
        let worker_active_label = Arc::clone(&active_label);
        let join = thread::Builder::new()
            .name("clipboard-board-hotkey-settings".to_owned())
            .spawn(move || {
                transaction_worker(
                    receiver,
                    settings_client,
                    initial_snapshot,
                    thread_sender,
                    signal,
                    worker_status,
                    worker_active_label,
                )
            })
            .ok();
        Self {
            sender,
            status,
            active_label,
            join,
        }
    }

    /// 返回可跨线程复制的托盘/设置壳提交句柄。
    pub(crate) fn handle(&self) -> HotkeyRequestHandle {
        HotkeyRequestHandle {
            sender: self.sender.clone(),
            status: Arc::clone(&self.status),
            active_label: Arc::clone(&self.active_label),
        }
    }

    /// 非阻塞提交候选；忙或对账状态不会覆盖已有事务。
    pub(crate) fn request(&self, settings: HotkeySettings) -> Result<(), HotkeyTransactionError> {
        self.handle().request(settings)
    }

    /// 返回当前事务状态；失败时 fail-closed 为 ActiveUnknown。
    pub(crate) fn status(&self) -> HotkeyTransactionStatus {
        self.handle().status()
    }

    /// 关闭事务 worker；不跨线程传递 HWND。
    pub(crate) fn stop(&mut self) {
        let _ = self.sender.try_send(OwnerCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for HotkeyTransactionOwner {
    /// 异常展开时尽力停止 worker；显式 stop 仍由 HotkeyManager 负责顺序收口。
    fn drop(&mut self) {
        self.stop();
    }
}

/// 持有消息线程和剪贴板捕获桥的生命周期控制器。
pub struct HotkeyManager {
    /// 消息线程 ID，只用于唤醒/停止，不跨线程传 HWND。
    thread_id: u32,
    /// 线程句柄只在停止时 join。
    join_handle: Option<JoinHandle<Result<(), HotkeyError>>>,
    /// ClipboardIO worker 的公共结果桥。
    clipboard_inbox: ClipboardCaptureInbox,
    /// 可选快捷键事务所有者；旧测试入口不传 SettingsClient 时为空。
    transaction_owner: Option<HotkeyTransactionOwner>,
    /// 消息线程命令桥，停止时先发送 Shutdown 再 WM_QUIT。
    thread_sender: HotkeyThreadSender,
}

impl HotkeyManager {
    /// 使用默认设置启动消息线程，保留旧调用方兼容性。
    pub fn start() -> Result<Self, HotkeyError> {
        Self::start_with_write_expectations(ClipboardWriteExpectationStore::new())
    }

    /// 使用写回预期启动消息线程。
    pub fn start_with_write_expectations(
        write_expectations: ClipboardWriteExpectationStore,
    ) -> Result<Self, HotkeyError> {
        Self::start_with_privacy(
            write_expectations,
            RecordingGate::new(GateMode::Active),
            PauseCommandSender::disabled(),
        )
    }

    /// 兼容旧启动入口；无 SettingsClient 时仍使用默认 Alt+V，但不支持运行时修改。
    pub fn start_with_privacy(
        write_expectations: ClipboardWriteExpectationStore,
        recording_gate: RecordingGate,
        pause_commands: PauseCommandSender,
    ) -> Result<Self, HotkeyError> {
        Self::start_internal(
            default_hotkey_spec(),
            None,
            write_expectations,
            recording_gate,
            pause_commands,
        )
    }

    /// 从已验证设置快照启动，并创建独立快捷键事务 worker。
    pub fn start_with_privacy_and_settings(
        write_expectations: ClipboardWriteExpectationStore,
        recording_gate: RecordingGate,
        pause_commands: PauseCommandSender,
        settings_client: SettingsClient,
        initial_snapshot: SettingsSnapshot,
    ) -> Result<Self, HotkeyError> {
        let hotkey =
            HotkeySpec::from_settings(DEFAULT_HOTKEY_ID, &initial_snapshot.settings().hotkey)?;
        Self::start_internal(
            hotkey,
            Some((settings_client, initial_snapshot)),
            write_expectations,
            recording_gate,
            pause_commands,
        )
    }

    /// 启动消息线程，并在注册冲突时保留 HWND/托盘进入 HotkeyUnavailable。
    fn start_internal(
        hotkey: HotkeySpec,
        settings: Option<(SettingsClient, SettingsSnapshot)>,
        write_expectations: ClipboardWriteExpectationStore,
        recording_gate: RecordingGate,
        pause_commands: PauseCommandSender,
    ) -> Result<Self, HotkeyError> {
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (command_sender, command_receiver) = mpsc::sync_channel(16);
        let signal = HotkeyRuntimeSignal::new();
        let worker_signal = signal.clone();
        let clipboard_inbox = ClipboardCaptureInbox::new();
        let worker_inbox = clipboard_inbox.clone();
        let worker_expectations = write_expectations;
        let join_handle = thread::Builder::new()
            .name("clipboard-board-hotkey".to_owned())
            .spawn(move || {
                system_window::run(
                    hotkey,
                    ready_sender,
                    command_receiver,
                    worker_signal,
                    worker_inbox,
                    worker_expectations,
                    recording_gate,
                    pause_commands,
                )
            })
            .map_err(|error| HotkeyError::ThreadStart(error.to_string()))?;

        let startup = match ready_receiver.recv() {
            Ok(Ok(startup)) => startup,
            Ok(Err(error)) => {
                let _ = join_handle.join();
                return Err(error);
            }
            Err(_) => {
                let _ = join_handle.join();
                return Err(HotkeyError::StartupChannelClosed);
            }
        };
        let thread_sender = HotkeyThreadSender::new(command_sender, startup.thread_id);
        let transaction_owner = settings.map(|(client, snapshot)| {
            let initial_status = if startup.hotkey_available {
                HotkeyTransactionStatus::Idle
            } else {
                HotkeyTransactionStatus::HotkeyUnavailable
            };
            HotkeyTransactionOwner::start(
                client,
                snapshot,
                thread_sender.clone(),
                signal.clone(),
                initial_status,
            )
        });
        clear_global_hotkey_request_handle();
        if let Some(owner) = transaction_owner.as_ref() {
            install_global_hotkey_request_handle(owner.handle());
        }
        Ok(Self {
            thread_id: startup.thread_id,
            join_handle: Some(join_handle),
            clipboard_inbox,
            transaction_owner,
            thread_sender,
        })
    }

    /// 返回捕获结果桥副本；调用方不取得消息线程或 HWND 所有权。
    pub fn clipboard_inbox(&self) -> ClipboardCaptureInbox {
        self.clipboard_inbox.clone()
    }

    /// 异步提交一条新的全局快捷键配置；UI 不等待磁盘或 RegisterHotKey。
    pub fn request_hotkey_change(
        &self,
        settings: HotkeySettings,
    ) -> Result<(), HotkeyTransactionError> {
        self.transaction_owner
            .as_ref()
            .ok_or(HotkeyTransactionError::Closed)?
            .request(settings)
    }

    /// 返回快捷键事务状态；用于设置壳/托盘展示冲突和对账提示。
    pub fn hotkey_status(&self) -> HotkeyTransactionStatus {
        self.transaction_owner
            .as_ref()
            .map(HotkeyTransactionOwner::status)
            .unwrap_or(HotkeyTransactionStatus::Idle)
    }

    /// 返回托盘/设置壳可复制的异步提交句柄；旧兼容启动入口没有设置 worker 时返回 None。
    pub fn hotkey_request_handle(&self) -> Option<HotkeyRequestHandle> {
        self.transaction_owner
            .as_ref()
            .map(HotkeyTransactionOwner::handle)
    }

    /// 请求消息线程先排空事务并注销热键，再发送 WM_QUIT 收口。
    pub fn stop(mut self) -> Result<(), HotkeyError> {
        clear_global_hotkey_request_handle();
        if let Some(mut owner) = self.transaction_owner.take() {
            owner.stop();
        }
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        let shutdown_result = self.thread_sender.send(HotkeyThreadCommand::Shutdown {
            reply: reply_sender,
        });
        let shutdown_ack = if shutdown_result.is_ok() {
            reply_receiver
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| HotkeyError::AckTimeout)
                .and_then(|ack| match ack {
                    HotkeyThreadAck::ShutdownComplete { .. } => Ok(()),
                    _ => Err(HotkeyError::AckTimeout),
                })
        } else {
            shutdown_result
        };
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
        shutdown_ack.and(post_result).and(join_result)
    }
}

impl Drop for HotkeyManager {
    /// 异常展开时尽力唤醒消息线程；正常路径由 stop 负责完整顺序。
    fn drop(&mut self) {
        clear_global_hotkey_request_handle();
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

/// 消息线程事务状态；所有字段只在线程内修改，避免 HWND 所有权泄漏。
pub(crate) struct ThreadHotkeyState {
    /// 当前旧/候选 active。
    pub(crate) active: Option<HotkeySpec>,
    /// 当前已注册但尚未发布的候选。
    pub(crate) candidate: Option<(u64, u64, i32, HotkeySettings)>,
    /// 已注销失败、当前生命周期内仍禁止复用的 stale ID。
    pub(crate) stale_ids: BTreeSet<i32>,
    /// 事务记录和 tombstone。
    pub(crate) transactions: HashMap<u64, (u64, ThreadTransactionState)>,
    /// 单调 ID 分配器。
    pub(crate) next_id: i32,
    /// 消息线程是否已被 Shutdown 封锁。
    pub(crate) shutting_down: bool,
    /// active 是否可安全推断。
    pub(crate) active_state: QueryActiveState,
    /// 当前 active 对应的 Settings revision；仅用于发布顺序审计，不写入磁盘。
    pub(crate) active_revision: u64,
}

impl ThreadHotkeyState {
    /// 使用启动规格构造线程状态；注册失败时 active=None 仍保留托盘。
    pub(crate) fn new(active: Option<HotkeySpec>) -> Self {
        let active_state = if active.is_some() {
            QueryActiveState::Old
        } else {
            QueryActiveState::None
        };
        Self {
            active,
            candidate: None,
            stale_ids: BTreeSet::new(),
            transactions: HashMap::new(),
            next_id: DEFAULT_HOTKEY_ID,
            shutting_down: false,
            active_state,
            active_revision: 0,
        }
    }

    /// 分配未被 active/candidate/stale 占用的正 ID；溢出或耗尽直接失败。
    pub(crate) fn allocate_id(&mut self) -> Option<i32> {
        // next_id 可能正好落在上限之后；先归一化到有限 ID 环，避免沿 i32 全范围空转。
        let start = if (1..=HOTKEY_ID_MAX).contains(&self.next_id) {
            self.next_id
        } else {
            1
        };
        let mut candidate = start;
        loop {
            if candidate > 0
                && candidate <= HOTKEY_ID_MAX
                && self
                    .active
                    .as_ref()
                    .is_none_or(|active| active.id != candidate)
                && self
                    .candidate
                    .as_ref()
                    .is_none_or(|entry| entry.2 != candidate)
                && !self.stale_ids.contains(&candidate)
            {
                self.next_id = if candidate == HOTKEY_ID_MAX {
                    1
                } else {
                    candidate + 1
                };
                return Some(candidate);
            }
            candidate = if candidate == HOTKEY_ID_MAX {
                1
            } else {
                candidate + 1
            };
            if candidate == start {
                return None;
            }
        }
    }

    /// 将取消事务登记为 tombstone；任何迟到命令只能得到 Cancelled。
    pub(crate) fn cancel(
        &mut self,
        transaction_id: u64,
        generation: u64,
    ) -> ThreadTransactionState {
        self.transactions.insert(
            transaction_id,
            (generation, ThreadTransactionState::Cancelled),
        );
        if self
            .candidate
            .as_ref()
            .is_some_and(|entry| entry.0 == transaction_id && entry.1 == generation)
        {
            self.candidate = None;
        }
        ThreadTransactionState::Cancelled
    }

    /// 判断命令是否是当前 generation；已取消或过期命令 fail-closed。
    pub(crate) fn accepts(&self, transaction_id: u64, generation: u64) -> bool {
        !self.shutting_down
            && !matches!(
                self.transactions.get(&transaction_id),
                Some((known_generation, ThreadTransactionState::Cancelled))
                    if *known_generation == generation
            )
            && self
                .transactions
                .get(&transaction_id)
                .is_none_or(|(known_generation, _)| *known_generation == generation)
    }
}

/// 运行一次保存/注册事务；所有阻塞都位于本 worker，不进入 UI 或消息线程。
fn transaction_worker(
    receiver: mpsc::Receiver<OwnerCommand>,
    settings_client: SettingsClient,
    mut snapshot: SettingsSnapshot,
    thread_sender: HotkeyThreadSender,
    signal: HotkeyRuntimeSignal,
    status: Arc<Mutex<HotkeyTransactionStatus>>,
    active_label: Arc<Mutex<String>>,
) {
    let mut next_transaction_id = 1_u64;
    let mut generation = 1_u64;
    while let Ok(command) = receiver.recv() {
        match command {
            OwnerCommand::Submit(candidate) => {
                set_status(&status, HotkeyTransactionStatus::Busy);
                let transaction_id = next_transaction_id;
                next_transaction_id = next_transaction_id.checked_add(1).unwrap_or(1);
                let current_generation = generation;
                generation = generation.checked_add(1).unwrap_or(1);
                let old_settings = snapshot.settings().clone();
                let old_hotkey_available =
                    signal.active_id() != 0 && !matches!(signal.state(), HotkeyRuntimeState::None);
                let result = run_transaction(
                    transaction_id,
                    current_generation,
                    candidate,
                    old_settings,
                    snapshot.revision(),
                    &settings_client,
                    &mut snapshot,
                    &thread_sender,
                    &signal,
                );
                match result {
                    TransactionOutcome::Committed => {
                        if let Ok(mut label) = active_label.lock() {
                            *label = snapshot.settings().hotkey.label();
                        }
                        set_status(&status, HotkeyTransactionStatus::Idle);
                    }
                    TransactionOutcome::Unavailable => {
                        set_status(
                            &status,
                            if old_hotkey_available {
                                HotkeyTransactionStatus::Idle
                            } else {
                                HotkeyTransactionStatus::HotkeyUnavailable
                            },
                        );
                    }
                    TransactionOutcome::Reconcile => {
                        signal.set(HotkeyRuntimeState::Unknown, 0);
                        set_status(&status, HotkeyTransactionStatus::ReconcileRequired);
                    }
                    TransactionOutcome::Unknown => {
                        signal.set(HotkeyRuntimeState::Unknown, 0);
                        set_status(&status, HotkeyTransactionStatus::ActiveUnknown);
                    }
                }
            }
            OwnerCommand::Shutdown => break,
        }
    }
}

/// 事务收口结果；Unknown 和 Reconcile 都必须让输入 fail-closed。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransactionOutcome {
    /// 新热键已发布并且持久化快照已确认。
    Committed,
    /// 候选注册/保存失败，旧 active 或 None 状态保持。
    Unavailable,
    /// 已知不能安全回滚，进入对账状态。
    Reconcile,
    /// 线程/发布结果未知，先封锁所有输入。
    Unknown,
}

/// 执行候选登记、CAS 保存和发布；不会自行假设未知回执为失败。
///
/// 参数分别代表事务身份、候选配置、CAS 快照、消息线程桥和 fail-closed 信号，
/// 保持这些边界显式有利于审计注册/持久化/发布的顺序；因此允许该函数保留较多参数。
#[allow(clippy::too_many_arguments)]
fn run_transaction(
    transaction_id: u64,
    generation: u64,
    candidate: HotkeySettings,
    old_settings: AppSettings,
    old_revision: u64,
    settings_client: &SettingsClient,
    snapshot: &mut SettingsSnapshot,
    thread_sender: &HotkeyThreadSender,
    signal: &HotkeyRuntimeSignal,
) -> TransactionOutcome {
    let register_reply = mpsc::sync_channel(1);
    if thread_sender
        .send(HotkeyThreadCommand::RegisterCandidate {
            transaction_id,
            generation,
            settings: candidate.clone(),
            reply: register_reply.0,
        })
        .is_err()
    {
        thread_sender.cancel_best_effort(transaction_id, generation);
        return TransactionOutcome::Unknown;
    }
    let candidate_id = match register_reply.1.recv_timeout(Duration::from_secs(2)) {
        Ok(HotkeyThreadAck::CandidateRegistered { candidate_id, .. }) => candidate_id,
        Ok(HotkeyThreadAck::RegistrationFailed { .. }) | Ok(HotkeyThreadAck::Cancelled { .. }) => {
            return TransactionOutcome::Unavailable
        }
        Ok(_) | Err(_) => {
            // 回执丢失时登记命令可能已经执行；追加 tombstone，迟到登记只能被取消。
            thread_sender.cancel_best_effort(transaction_id, generation);
            return TransactionOutcome::Unknown;
        }
    };

    let mut new_settings = old_settings.clone();
    new_settings.hotkey = candidate.clone();
    let durable_snapshot = match settings_client.save(old_revision, new_settings.clone()) {
        Ok(saved) => Some(saved),
        Err(SettingsError::OutcomeUnknown) => match settings_client.snapshot() {
            Ok(authoritative) if authoritative.settings() == &new_settings => Some(authoritative),
            Ok(authoritative) => {
                return rollback_after_save_failure(
                    transaction_id,
                    generation,
                    candidate_id,
                    &old_settings,
                    snapshot,
                    settings_client,
                    Some(authoritative),
                    thread_sender,
                    signal,
                );
            }
            Err(_) => return TransactionOutcome::Reconcile,
        },
        Err(_) => {
            return rollback_after_save_failure(
                transaction_id,
                generation,
                candidate_id,
                &old_settings,
                snapshot,
                settings_client,
                None,
                thread_sender,
                signal,
            );
        }
    };
    let Some(durable_snapshot) = durable_snapshot else {
        return TransactionOutcome::Reconcile;
    };
    *snapshot = durable_snapshot;

    let publish_reply = mpsc::sync_channel(1);
    if thread_sender
        .send(HotkeyThreadCommand::PublishActive {
            transaction_id,
            generation,
            candidate_id,
            settings_revision: snapshot.revision(),
            reply: publish_reply.0,
        })
        .is_err()
    {
        signal.set(HotkeyRuntimeState::Unknown, 0);
        thread_sender.cancel_best_effort(transaction_id, generation);
        return reconcile_after_publish_loss(
            transaction_id,
            generation,
            candidate_id,
            snapshot,
            old_settings,
            old_revision,
            settings_client,
            thread_sender,
            signal,
        );
    }
    match publish_reply.1.recv_timeout(Duration::from_secs(2)) {
        Ok(HotkeyThreadAck::Published { active_id, .. }) => {
            signal.set(HotkeyRuntimeState::Candidate, active_id);
            TransactionOutcome::Committed
        }
        Ok(_) | Err(_) => {
            // Publish ack 丢失时先追加取消 tombstone，再通过 Query 判断发布是否已线性化。
            thread_sender.cancel_best_effort(transaction_id, generation);
            reconcile_after_publish_loss(
                transaction_id,
                generation,
                candidate_id,
                snapshot,
                old_settings,
                old_revision,
                settings_client,
                thread_sender,
                signal,
            )
        }
    }
}

/// 保存失败时注销候选；注销失败登记 stale 并进入对账，而非丢失旧热键。
fn rollback_candidate(
    transaction_id: u64,
    generation: u64,
    candidate_id: i32,
    thread_sender: &HotkeyThreadSender,
    signal: &HotkeyRuntimeSignal,
) -> TransactionOutcome {
    let reply = mpsc::sync_channel(1);
    if thread_sender
        .send(HotkeyThreadCommand::DropCandidate {
            transaction_id,
            generation,
            candidate_id,
            reply: reply.0,
        })
        .is_err()
    {
        thread_sender.cancel_best_effort(transaction_id, generation);
        return TransactionOutcome::Unknown;
    }
    match reply.1.recv_timeout(Duration::from_secs(2)) {
        Ok(HotkeyThreadAck::CandidateDropped { success: true, .. }) => {
            signal.set(
                if signal.active_id() == 0 {
                    HotkeyRuntimeState::None
                } else {
                    HotkeyRuntimeState::ActiveOld
                },
                signal.active_id(),
            );
            TransactionOutcome::Unavailable
        }
        Ok(HotkeyThreadAck::CandidateDropped { success: false, .. }) => {
            TransactionOutcome::Reconcile
        }
        _ => {
            thread_sender.cancel_best_effort(transaction_id, generation);
            TransactionOutcome::Unknown
        }
    }
}

/// 根据保存失败后的权威快照决定是否可以恢复旧运行态；未知或已被并发修改的配置一律对账。
fn resolve_rollback_snapshot(
    old_settings: &AppSettings,
    snapshot: &mut SettingsSnapshot,
    authoritative: Result<SettingsSnapshot, SettingsError>,
) -> TransactionOutcome {
    match authoritative {
        Ok(authoritative) => {
            let same_hotkey = authoritative.settings().hotkey == old_settings.hotkey;
            *snapshot = authoritative;
            if same_hotkey {
                TransactionOutcome::Unavailable
            } else {
                // 权威配置已经换过热键，但当前 HWND 仍保留旧 active，必须停止输入等待对账。
                TransactionOutcome::Reconcile
            }
        }
        Err(_) => TransactionOutcome::Reconcile,
    }
}

/// 保存失败后先清理候选，再确认权威快照仍指向旧热键；并发修改了其他设置时，
/// 也要刷新本地 revision，避免下一次提交反复使用旧 CAS 身份覆盖新字段。
///
/// 该收口函数需要同时接收事务身份、旧快照、权威快照来源、线程桥和运行信号，
/// 这些参数对应不同失败边界，合并成隐式上下文会削弱回滚审计，因此保留显式参数。
#[allow(clippy::too_many_arguments)]
fn rollback_after_save_failure(
    transaction_id: u64,
    generation: u64,
    candidate_id: i32,
    old_settings: &AppSettings,
    snapshot: &mut SettingsSnapshot,
    settings_client: &SettingsClient,
    known_snapshot: Option<SettingsSnapshot>,
    thread_sender: &HotkeyThreadSender,
    signal: &HotkeyRuntimeSignal,
) -> TransactionOutcome {
    let rollback = rollback_candidate(
        transaction_id,
        generation,
        candidate_id,
        thread_sender,
        signal,
    );
    if !matches!(rollback, TransactionOutcome::Unavailable) {
        return rollback;
    }
    let authoritative = known_snapshot
        .map(Ok)
        .unwrap_or_else(|| settings_client.snapshot());
    resolve_rollback_snapshot(old_settings, snapshot, authoritative)
}

/// 发布回执丢失后只按 Query 结果收敛，不根据发送错误擅自 CAS 回滚。
///
/// 查询对账必须携带事务身份、候选 ID、CAS revision、设置客户端和线程桥，
/// 以便明确区分已发布、已回滚和未知状态；这里保留参数是为了避免隐藏副作用。
#[allow(clippy::too_many_arguments)]
fn reconcile_after_publish_loss(
    transaction_id: u64,
    generation: u64,
    candidate_id: i32,
    snapshot: &mut SettingsSnapshot,
    old_settings: AppSettings,
    old_revision: u64,
    settings_client: &SettingsClient,
    thread_sender: &HotkeyThreadSender,
    signal: &HotkeyRuntimeSignal,
) -> TransactionOutcome {
    let reply = mpsc::sync_channel(1);
    if thread_sender
        .send(HotkeyThreadCommand::QueryTransaction {
            transaction_id,
            reply: reply.0,
        })
        .is_err()
    {
        thread_sender.cancel_best_effort(transaction_id, generation);
        return TransactionOutcome::Unknown;
    }
    let Ok(HotkeyThreadAck::Query { result, .. }) = reply.1.recv_timeout(Duration::from_secs(2))
    else {
        thread_sender.cancel_best_effort(transaction_id, generation);
        return TransactionOutcome::Unknown;
    };
    match (result.transaction, result.active_state) {
        (ThreadTransactionState::Published, QueryActiveState::Candidate)
            if result.active_id == candidate_id =>
        {
            signal.set(HotkeyRuntimeState::Candidate, candidate_id);
            TransactionOutcome::Committed
        }
        (ThreadTransactionState::CandidateRegistered, QueryActiveState::Old) => {
            let drop_reply = mpsc::sync_channel(1);
            if thread_sender
                .send(HotkeyThreadCommand::DropCandidate {
                    transaction_id,
                    generation,
                    candidate_id,
                    reply: drop_reply.0,
                })
                .is_err()
            {
                thread_sender.cancel_best_effort(transaction_id, generation);
                return TransactionOutcome::Reconcile;
            }
            if !matches!(
                drop_reply.1.recv_timeout(Duration::from_secs(2)),
                Ok(HotkeyThreadAck::CandidateDropped { success: true, .. })
            ) {
                thread_sender.cancel_best_effort(transaction_id, generation);
                return TransactionOutcome::Reconcile;
            }
            // `old_settings` 是保存前的完整 DTO，CAS 成功后恢复它才能避免覆盖并发的未知字段。
            match settings_client.save(snapshot.revision(), old_settings) {
                Ok(restored) if restored.revision() > old_revision => {
                    *snapshot = restored;
                    signal.set(HotkeyRuntimeState::ActiveOld, result.active_id);
                    TransactionOutcome::Unavailable
                }
                Ok(_) => TransactionOutcome::Reconcile,
                Err(_) => TransactionOutcome::Reconcile,
            }
        }
        // NotFound/Cancelled 不能证明保存未发生；保存后禁止无声回滚到 Idle。
        _ => TransactionOutcome::Reconcile,
    }
}

/// 线程安全写入状态；锁损坏时状态保持最保守 Busy。
fn set_status(status: &Arc<Mutex<HotkeyTransactionStatus>>, value: HotkeyTransactionStatus) {
    if let Ok(mut current) = status.lock() {
        *current = value;
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证组合边界、ID 分配、事务 tombstone 和运行时消息过滤，不注册真实热键。

    use super::*;
    use crate::settings::{HotkeySettings, SettingsLoadSource};

    /// Alt+V 必须保持默认值和规范化展示。
    #[test]
    fn 默认快捷键规格稳定() {
        let default = default_hotkey_spec();
        assert_eq!(default.id, DEFAULT_HOTKEY_ID);
        assert_eq!(default.virtual_key, 0x56);
        assert_eq!(default.label, "Alt + V");
    }

    /// ID allocator 必须跳过 active、candidate、stale，并在范围耗尽时 fail-closed。
    #[test]
    fn id_allocator_skips_active_candidate_stale() {
        let mut state = ThreadHotkeyState::new(Some(default_hotkey_spec()));
        state.next_id = DEFAULT_HOTKEY_ID;
        state.stale_ids.insert(DEFAULT_HOTKEY_ID + 1);
        state.candidate = Some((1, 1, DEFAULT_HOTKEY_ID + 2, HotkeySettings::default()));
        assert_eq!(state.allocate_id(), Some(DEFAULT_HOTKEY_ID + 3));
    }

    /// ID 分配到上限后必须回到 1，不能因上限之后的 next_id 空转整个 i32 范围。
    #[test]
    fn id_allocator_wraps_at_bounded_upper_limit() {
        let mut state = ThreadHotkeyState::new(Some(default_hotkey_spec()));
        state.next_id = HOTKEY_ID_MAX;
        state.stale_ids.insert(HOTKEY_ID_MAX);
        assert_eq!(state.allocate_id(), Some(1));
        assert_eq!(state.next_id, 2);
    }

    /// Cancel 会登记 generation tombstone，迟到命令不得重新启用事务。
    #[test]
    fn cancel_tombstone_rejects_late_generation() {
        let mut state = ThreadHotkeyState::new(Some(default_hotkey_spec()));
        state
            .transactions
            .insert(9, (4, ThreadTransactionState::CandidateRegistered));
        assert_eq!(state.cancel(9, 4), ThreadTransactionState::Cancelled);
        assert!(!state.accepts(9, 4));
        assert!(state.accepts(10, 1));
    }

    /// Unknown runtime state 必须关闭输入；candidate/stale ID 也不能穿透过滤器。
    #[test]
    fn runtime_signal_unknown_blocks_all_hotkeys() {
        let signal = HotkeyRuntimeSignal::new();
        signal.set(HotkeyRuntimeState::ActiveOld, DEFAULT_HOTKEY_ID);
        assert_eq!(signal.state(), HotkeyRuntimeState::ActiveOld);
        assert_eq!(signal.active_id(), DEFAULT_HOTKEY_ID);
        signal.set(HotkeyRuntimeState::Unknown, 0);
        assert_eq!(signal.state(), HotkeyRuntimeState::Unknown);
        assert_eq!(signal.active_id(), 0);
    }

    /// 保存失败后只有权威快照仍保留旧快捷键时才可恢复 Idle；并发改动或快照未知必须对账。
    #[test]
    fn 保存失败回滚只接受旧快捷键权威快照() {
        let old_settings = AppSettings::default();
        let mut snapshot =
            SettingsSnapshot::new(old_settings.clone(), SettingsLoadSource::Defaults, 1);
        assert_eq!(
            resolve_rollback_snapshot(
                &old_settings,
                &mut snapshot,
                Ok(SettingsSnapshot::new(
                    old_settings.clone(),
                    SettingsLoadSource::Primary,
                    2,
                )),
            ),
            TransactionOutcome::Unavailable
        );
        assert_eq!(snapshot.revision(), 2);

        let mut changed_settings = old_settings.clone();
        changed_settings.hotkey.virtual_key = 0x43;
        assert_eq!(
            resolve_rollback_snapshot(
                &old_settings,
                &mut snapshot,
                Ok(SettingsSnapshot::new(
                    changed_settings,
                    SettingsLoadSource::Primary,
                    3,
                )),
            ),
            TransactionOutcome::Reconcile
        );
        assert_eq!(
            resolve_rollback_snapshot(
                &old_settings,
                &mut snapshot,
                Err(SettingsError::OutcomeUnknown),
            ),
            TransactionOutcome::Reconcile
        );
    }
}

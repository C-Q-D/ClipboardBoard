//! 此模块实现当前用户开机启动设置的注册表适配、双资源事务和异步所有者。
//!
//! 生产实现只访问 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，并把
//! 注册表值限制为当前可执行文件的单参数命令行。所有 Win32 字符串都在本模块内以
//! UTF-16 处理；测试通过 `RegistryBackend` 注入 fake，不触碰真实用户注册表。

use std::{
    ffi::{OsStr, OsString},
    fmt::{Display, Formatter},
    num::NonZeroU64,
    os::windows::ffi::{OsStrExt, OsStringExt},
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender, TrySendError},
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
};

use crate::settings::{SettingsClient, SettingsError, SettingsSnapshot};
use windows_sys::Win32::{
    Foundation::{
        ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_PATH_NOT_FOUND,
        ERROR_SUCCESS,
    },
    System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
    },
};

/// 当前用户 Run 子键；固定范围防止实现漂移到机器级启动位置。
pub const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// ClipboardBoard 固定使用的值名；值名固定不代表自动拥有已有值。
pub const RUN_VALUE_NAME: &str = "ClipboardBoard";
/// 注册表字符串的最大字节数，防止损坏值造成无界分配。
const MAX_REGISTRY_DATA_BYTES: usize = 1024 * 1024;
/// owner 命令通道的固定容量；满队列必须立即返回 Busy。
const OWNER_QUEUE_CAPACITY: usize = 1;
/// Settings revision 冲突的最大自动重试次数。
const SETTINGS_RETRY_LIMIT: usize = 3;

/// 启动状态在 UI/托盘边界使用的稳定分类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectiveStartupState {
    /// 配置期望关闭且 Run 值缺失。
    Disabled,
    /// 配置期望开启且 Run 值由当前程序拥有。
    Enabled,
    /// 配置期望开启但 Run 值缺失。
    Missing,
    /// 配置期望关闭但 Run 值仍由当前程序拥有，或发生其他可报告错配。
    Mismatch,
    /// 同名值属于其他命令行，不能覆盖或删除。
    Conflict,
    /// 注册表值类型或 UTF-16 数据无法作为合法字符串。
    InvalidValue,
    /// 访问注册表权限不足。
    PermissionDenied,
    /// 读写后置状态暂时无法证明。
    Unknown,
}

/// 底层操作的稳定错误类别；不携带路径、命令行或注册表正文。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupErrorCategory {
    /// 输入包含 NUL、控制字符、引号或非法 UTF-16。
    InvalidInput,
    /// 当前用户注册表访问被拒绝。
    PermissionDenied,
    /// 注册表或配置 IO 暂时不可用。
    Unavailable,
    /// 固定值名已被其他命令行/类型占用。
    ForeignConflict,
    /// SettingsWorker 的 revision 发生并发冲突。
    SettingsConflict,
    /// 写入、读回或补偿后的状态不可判定。
    OutcomeUnknown,
    /// 旧注册表状态无法安全补偿。
    CompensationFailed,
    /// owner 已停止或进入关闭流程。
    OwnerClosed,
}

/// 启动设置错误；Display 只输出稳定类别，避免泄露完整路径或原始 Win32 文本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartupError {
    /// 稳定错误类别。
    pub category: StartupErrorCategory,
    /// 可选 Win32 错误码，仅供调用方内部诊断，不进入 UI 文案。
    pub code: Option<u32>,
}

impl StartupError {
    /// 构造没有底层错误码的稳定错误。
    const fn category(category: StartupErrorCategory) -> Self {
        Self {
            category,
            code: None,
        }
    }
}

impl Display for StartupError {
    /// 生成不包含路径、命令行或底层错误文本的稳定描述。
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let label = match self.category {
            StartupErrorCategory::InvalidInput => "启动设置输入非法",
            StartupErrorCategory::PermissionDenied => "启动设置权限不足",
            StartupErrorCategory::Unavailable => "启动设置暂时不可用",
            StartupErrorCategory::ForeignConflict => "启动项同名冲突",
            StartupErrorCategory::SettingsConflict => "启动设置配置并发冲突",
            StartupErrorCategory::OutcomeUnknown => "启动设置结果未知",
            StartupErrorCategory::CompensationFailed => "启动设置补偿失败",
            StartupErrorCategory::OwnerClosed => "启动设置所有者已关闭",
        };
        write!(formatter, "{label}")
    }
}

impl std::error::Error for StartupError {}

/// 注册表后端看到的原始值；`data` 保留 REG_SZ 的所有 UTF-16 code unit（含终止 NUL）。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryValue {
    /// Win32 注册表值类型，例如 `REG_SZ`。
    pub value_type: u32,
    /// 原始 UTF-16 数据，不进行 lossy 转换。
    pub data: Vec<u16>,
}

/// Run 值的可选快照；`None` 表示值缺失。
pub type RegistrySnapshot = Option<RegistryValue>;

/// fake/生产后端的底层错误；owner 负责把它映射为稳定错误类别。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryError {
    /// 访问被拒绝。
    PermissionDenied,
    /// 键或值不存在。
    NotFound,
    /// 普通 IO/Win32 错误。
    Unavailable(Option<u32>),
    /// 读到奇数长度、过长或其他无法解码的数据。
    InvalidData,
    /// expected-state CAS 失败，说明有外部改值。
    Conflict,
    /// 后端无法证明调用后的状态。
    OutcomeUnknown,
}

/// RegistryBackend 是唯一允许 owner 访问 Run 值的抽象；实现必须执行 expected-state 守卫。
pub trait RegistryBackend: Send {
    /// 读取固定 Run 值；缺失值返回 `Ok(None)`，不把缺失误报为 IO 错误。
    fn read(&mut self) -> Result<RegistrySnapshot, RegistryError>;

    /// 仅当当前值仍等于 expected 时写入新值；不得把普通 read→write 当作 CAS。
    fn set_if_matches(
        &mut self,
        expected: &RegistrySnapshot,
        replacement: &RegistryValue,
    ) -> Result<(), RegistryError>;

    /// 仅当当前值仍等于 expected 时删除值；expected 缺失时操作是幂等成功。
    fn delete_if_matches(&mut self, expected: &RegistrySnapshot) -> Result<(), RegistryError>;
}

/// 需要执行的启动方向；Query/Retry/Shutdown 由命令协议单独表示。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupIntent {
    /// 把当前账户启动项设置为启用。
    Enable,
    /// 把当前账户启动项设置为禁用。
    Disable,
}

/// owner 对外发送的命令种类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupCommandKind {
    /// 启用登录启动。
    Enable,
    /// 禁用登录启动。
    Disable,
    /// 只查询当前 effective 状态。
    Query,
    /// 重新对账上一次未知事务。
    Retry,
    /// 拒绝新命令并排空后停止 owner。
    Shutdown,
}

/// owner 发出的稳定结果事件；具体错误不携带原始注册表文本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupResultKind {
    /// 双资源均确认目标已应用。
    Applied,
    /// 目标状态原本已满足，未产生写入。
    AlreadyApplied,
    /// 同名值属于其他程序或路径发生冲突。
    Conflict,
    /// 注册表已补偿回旧状态，配置保存确定失败。
    SaveFailed,
    /// 权限/IO 暂时失败，可稍后重试。
    PendingRetry,
    /// 事务后置状态无法证明，必须先对账。
    ReconcileRequired,
    /// 当前有另一个事务在途或命令队列已满。
    Busy,
    /// owner 已停止。
    Stopped,
    /// 命令携带的 generation 已被新的 tombstone 淘汰；命令未执行。
    Stale,
    /// 查询到的 effective 状态。
    Status(EffectiveStartupState),
}

impl StartupResultKind {
    /// 将启动设置结果转换为不泄露路径或底层错误的稳定 UI 文案。
    pub const fn ui_label(self) -> &'static str {
        match self {
            Self::Applied => "开机启动已更新",
            Self::AlreadyApplied => "开机启动状态未变化",
            Self::Conflict => "开机启动存在冲突",
            Self::SaveFailed => "配置保存失败，已恢复",
            Self::PendingRetry => "开机启动暂不可用，请重试",
            Self::ReconcileRequired => "开机启动状态待对账",
            Self::Busy => "开机启动处理中",
            Self::Stopped => "开机启动服务已停止",
            Self::Stale => "开机启动请求已过期",
            Self::Status(state) => match state {
                EffectiveStartupState::Disabled => "开机启动：已关闭",
                EffectiveStartupState::Enabled => "开机启动：已启用",
                EffectiveStartupState::Missing => "开机启动：缺少注册项",
                EffectiveStartupState::Mismatch => "开机启动：配置不一致",
                EffectiveStartupState::Conflict => "开机启动：存在冲突",
                EffectiveStartupState::PermissionDenied => "开机启动：权限不足",
                EffectiveStartupState::InvalidValue => "开机启动：注册项无效",
                EffectiveStartupState::Unknown => "开机启动：状态未知",
            },
        }
    }
}

/// 命令结果事件；transaction/generation 使迟到回执可被安全丢弃。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartupResult {
    /// 业务事务正整数身份。
    pub transaction_id: NonZeroU64,
    /// 命令 generation。
    pub generation: u64,
    /// 稳定结果种类。
    pub kind: StartupResultKind,
    /// 可选稳定错误类别。
    pub error: Option<StartupErrorCategory>,
}

/// 由 UI/托盘持有的非阻塞命令入口。
#[derive(Clone)]
pub struct StartupCommandSender {
    /// 容量一 owner 命令通道。
    sender: SyncSender<StartupCommand>,
    /// 生命周期门禁；关闭后先拒绝新命令。
    lifecycle: Arc<StartupLifecycle>,
    /// 自增事务身份。
    next_transaction: Arc<AtomicU64>,
    /// 丢弃迟到命令的 generation。
    generation: Arc<AtomicU64>,
    /// 在途门禁；worker 正在处理一个命令时，后续提交立即返回 Busy。
    in_flight: Arc<AtomicBool>,
}

/// owner 生命周期只允许 Open→Closing→Closed。
struct StartupLifecycle {
    /// 关闭意图必须在线性化锁外先发布，阻断竞态提交。
    closing_intent: AtomicBool,
    /// 状态锁把检查与入队绑定为一个线性化点。
    state: Mutex<StartupLifecycleState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupLifecycleState {
    /// 接收新命令。
    Open,
    /// 拒绝新命令，等待已准入命令排空。
    Closing,
    /// owner 已 join。
    Closed,
}

/// owner 所有者；独占后端和 settings client，负责关闭线程。
pub struct StartupSettingsOwner {
    /// 生命周期和命令入口。
    lifecycle: Arc<StartupLifecycle>,
    /// owner 命令发送端。
    sender: StartupCommandSender,
    /// worker 线程句柄。
    worker: Option<JoinHandle<()>>,
}

/// worker 内部保存的事务状态；旧值和写入值均为完整 raw 快照。
struct TransactionState {
    /// 目标方向。
    intent: StartupIntent,
    /// 操作前完整设置快照。
    old_settings: SettingsSnapshot,
    /// 操作前 Run 值。
    old_registry: RegistrySnapshot,
    /// 本事务写入的 Run 值；补偿时必须仍由本事务拥有。
    written_registry: RegistrySnapshot,
}

/// worker 接收的命令；回复通道容量一，断开不影响已提交事务。
struct StartupCommand {
    /// 命令种类。
    kind: StartupCommandKind,
    /// 正整数事务身份。
    transaction_id: NonZeroU64,
    /// generation。
    generation: u64,
    /// 调用方最后观察到的 settings revision。
    expected_revision: u64,
    /// 一次性回执；UI 可丢弃但 worker 仍完成事务。
    reply: SyncSender<StartupResult>,
}

impl StartupCommandSender {
    /// 构造不启用开机启动入口的兼容 sender，供旧测试/非主流程使用。
    pub(crate) fn disabled() -> Self {
        let (sender, receiver) = sync_channel(OWNER_QUEUE_CAPACITY);
        drop(receiver);
        Self {
            sender,
            lifecycle: Arc::new(StartupLifecycle {
                closing_intent: AtomicBool::new(true),
                state: Mutex::new(StartupLifecycleState::Closed),
            }),
            next_transaction: Arc::new(AtomicU64::new(1)),
            generation: Arc::new(AtomicU64::new(1)),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 以当前 generation 投递启用命令；队列满或关闭立即返回 Busy/OwnerClosed。
    pub fn try_enable(
        &self,
        expected_revision: u64,
    ) -> Result<Receiver<StartupResult>, StartupError> {
        self.submit(StartupCommandKind::Enable, expected_revision)
    }

    /// 以当前 generation 投递禁用命令。
    pub fn try_disable(
        &self,
        expected_revision: u64,
    ) -> Result<Receiver<StartupResult>, StartupError> {
        self.submit(StartupCommandKind::Disable, expected_revision)
    }

    /// 投递查询命令；查询不改变任何耐久资源。
    pub fn try_query(&self) -> Result<Receiver<StartupResult>, StartupError> {
        self.submit(StartupCommandKind::Query, 0)
    }

    /// 投递上一次未知事务的对账重试。
    pub fn try_retry(&self) -> Result<Receiver<StartupResult>, StartupError> {
        self.submit(StartupCommandKind::Retry, 0)
    }

    /// 投递关闭命令；调用方应先让 owner 进入 Closing。
    fn try_shutdown(&self) -> Result<Receiver<StartupResult>, StartupError> {
        let (command, receiver) = {
            let state = self
                .lifecycle
                .state
                .lock()
                .map_err(|_| StartupError::category(StartupErrorCategory::OwnerClosed))?;
            if *state != StartupLifecycleState::Closing {
                return Err(StartupError::category(StartupErrorCategory::OwnerClosed));
            }
            let raw_transaction = self.next_transaction.fetch_add(1, Ordering::Relaxed);
            let transaction_id = NonZeroU64::new(raw_transaction.max(1)).expect("正整数事务 ID");
            let generation = self.generation.load(Ordering::Acquire);
            let (reply, receiver) = sync_channel(1);
            (
                StartupCommand {
                    kind: StartupCommandKind::Shutdown,
                    transaction_id,
                    generation,
                    expected_revision: 0,
                    reply,
                },
                receiver,
            )
        };
        // Shutdown 只由 owner 关闭线程调用；允许唯一的在途命令先排空后再入队。
        // 这里的阻塞不在 UI/托盘线程，且保证 finish_shutdown 不会因队列暂满而
        // 放弃 join，避免 SettingsWorker 先关造成跨线程使用已关闭 client。
        if self.sender.send(command).is_err() {
            if let Ok(mut state) = self.lifecycle.state.lock() {
                *state = StartupLifecycleState::Closed;
            }
            return Err(StartupError::category(StartupErrorCategory::OwnerClosed));
        }
        Ok(receiver)
    }

    /// 读取 owner 是否已关闭，不触碰 registry/settings IO。
    pub fn is_closed(&self) -> bool {
        self.lifecycle
            .state
            .lock()
            .map_or(true, |state| *state == StartupLifecycleState::Closed)
    }

    /// 开始下一代命令；旧 generation 即使晚到也只会被 worker 丢弃。
    pub fn advance_generation(&self) -> u64 {
        self.generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1)
    }

    /// 在生命周期锁内完成命令入队，避免关闭和提交出现竞态窗口。
    fn submit(
        &self,
        kind: StartupCommandKind,
        expected_revision: u64,
    ) -> Result<Receiver<StartupResult>, StartupError> {
        let retry_during_closing = kind == StartupCommandKind::Retry;
        if self.lifecycle.closing_intent.load(Ordering::Acquire) && !retry_during_closing {
            return Err(StartupError::category(StartupErrorCategory::OwnerClosed));
        }
        let mut state = self
            .lifecycle
            .state
            .lock()
            .map_err(|_| StartupError::category(StartupErrorCategory::OwnerClosed))?;
        let accepted_lifecycle = *state == StartupLifecycleState::Open
            || (retry_during_closing && *state == StartupLifecycleState::Closing);
        if !accepted_lifecycle
            || (self.lifecycle.closing_intent.load(Ordering::Acquire) && !retry_during_closing)
        {
            return Err(StartupError::category(StartupErrorCategory::OwnerClosed));
        }
        if self
            .in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // 在途事务和容量一队列都不允许继续排队；通过一次性回执让 UI
            // 能区分 Busy，而不是把“未执行”误显示成普通不可用。
            let raw_transaction = self.next_transaction.fetch_add(1, Ordering::Relaxed);
            let transaction_id = NonZeroU64::new(raw_transaction.max(1)).expect("正整数事务 ID");
            let generation = self.generation.load(Ordering::Acquire);
            let (busy_sender, busy_receiver) = sync_channel(1);
            let _ = busy_sender.send(StartupResult {
                transaction_id,
                generation,
                kind: StartupResultKind::Busy,
                error: None,
            });
            return Ok(busy_receiver);
        }
        let raw_transaction = self.next_transaction.fetch_add(1, Ordering::Relaxed);
        let transaction_id = NonZeroU64::new(raw_transaction.max(1)).expect("正整数事务 ID");
        let generation = self.generation.load(Ordering::Acquire);
        let (reply, receiver) = sync_channel(1);
        let command = StartupCommand {
            kind,
            transaction_id,
            generation,
            expected_revision,
            reply,
        };
        match self.sender.try_send(command) {
            Ok(()) => Ok(receiver),
            Err(TrySendError::Full(command)) => {
                // 队列满不阻塞消息线程；用一次性 Busy 事件把原因交还给调用方。
                self.in_flight.store(false, Ordering::Release);
                let (busy_sender, busy_receiver) = sync_channel(1);
                let _ = busy_sender.send(StartupResult {
                    transaction_id: command.transaction_id,
                    generation: command.generation,
                    kind: StartupResultKind::Busy,
                    error: None,
                });
                Ok(busy_receiver)
            }
            Err(TrySendError::Disconnected(_)) => {
                self.in_flight.store(false, Ordering::Release);
                *state = StartupLifecycleState::Closed;
                Err(StartupError::category(StartupErrorCategory::OwnerClosed))
            }
        }
    }
}

impl StartupSettingsOwner {
    /// 生产构造：读取当前 exe 并使用 HKCU Run 后端。
    pub fn start(settings: SettingsClient) -> Result<Self, StartupError> {
        let executable = std::env::current_exe()
            .map_err(|_| StartupError::category(StartupErrorCategory::Unavailable))?
            .into_os_string();
        let backend = Box::new(WindowsRegistryBackend::new());
        Self::start_with_backend(settings, backend, executable)
    }

    /// 测试构造：注入 registry fake 和可控 exe 路径，绝不访问真实 HKCU。
    pub fn start_with_backend(
        settings: SettingsClient,
        backend: Box<dyn RegistryBackend>,
        executable: OsString,
    ) -> Result<Self, StartupError> {
        let canonical = quote_windows_single_argument(&executable)?;
        let lifecycle = Arc::new(StartupLifecycle {
            closing_intent: AtomicBool::new(false),
            state: Mutex::new(StartupLifecycleState::Open),
        });
        let (sender, receiver) = sync_channel(OWNER_QUEUE_CAPACITY);
        let next_transaction = Arc::new(AtomicU64::new(1));
        let generation = Arc::new(AtomicU64::new(1));
        let in_flight = Arc::new(AtomicBool::new(false));
        let command_sender = StartupCommandSender {
            sender: sender.clone(),
            lifecycle: Arc::clone(&lifecycle),
            next_transaction,
            generation: Arc::clone(&generation),
            in_flight: Arc::clone(&in_flight),
        };
        let worker_lifecycle = Arc::clone(&lifecycle);
        let worker_generation = Arc::clone(&generation);
        let worker_in_flight = Arc::clone(&in_flight);
        let worker = thread::Builder::new()
            .name("clipboard-board-startup-settings".to_owned())
            .spawn(move || {
                owner_main(
                    settings,
                    backend,
                    canonical,
                    receiver,
                    worker_lifecycle,
                    worker_generation,
                    worker_in_flight,
                )
            })
            .map_err(|_| StartupError::category(StartupErrorCategory::Unavailable))?;
        Ok(Self {
            lifecycle,
            sender: command_sender,
            worker: Some(worker),
        })
    }

    /// 返回 UI/托盘使用的非阻塞命令入口。
    pub fn sender(&self) -> StartupCommandSender {
        self.sender.clone()
    }

    /// 建立关闭线性化点；之后所有新命令立即拒绝。
    pub fn begin_closing(&mut self) -> Result<(), StartupError> {
        self.lifecycle.closing_intent.store(true, Ordering::Release);
        let mut state = self
            .lifecycle
            .state
            .lock()
            .map_err(|_| StartupError::category(StartupErrorCategory::OwnerClosed))?;
        match *state {
            StartupLifecycleState::Open => {
                *state = StartupLifecycleState::Closing;
                Ok(())
            }
            // 关闭流程可能被 RuntimeCleanup 重入；Closing 已是线性化后的稳定状态，
            // 重复调用必须保持幂等，方便后续再次尝试 finish_shutdown。
            StartupLifecycleState::Closing => Ok(()),
            StartupLifecycleState::Closed => {
                Err(StartupError::category(StartupErrorCategory::OwnerClosed))
            }
        }
    }

    /// 排队 Shutdown 并等待 worker join；已准入命令会先完成。
    pub fn finish_shutdown(&mut self) -> Result<(), StartupError> {
        {
            let state = self
                .lifecycle
                .state
                .lock()
                .map_err(|_| StartupError::category(StartupErrorCategory::OwnerClosed))?;
            if *state == StartupLifecycleState::Open {
                return Err(StartupError::category(StartupErrorCategory::OwnerClosed));
            }
            if *state == StartupLifecycleState::Closed {
                return Err(StartupError::category(StartupErrorCategory::OwnerClosed));
            }
        }
        let receiver = self.sender.try_shutdown()?;
        let shutdown_result = receiver.recv().ok();
        if matches!(
            shutdown_result.map(|result| result.kind),
            Some(StartupResultKind::ReconcileRequired)
        ) {
            // owner 仍在 Closing 且保留 pending 槽；调用方必须先 Retry，
            // 不能 join 后再关闭其仍在使用的 SettingsWorker。
            return Err(StartupError::category(StartupErrorCategory::OutcomeUnknown));
        }
        let worker = self
            .worker
            .take()
            .ok_or_else(|| StartupError::category(StartupErrorCategory::OwnerClosed))?;
        worker
            .join()
            .map_err(|_| StartupError::category(StartupErrorCategory::Unavailable))?;
        if let Ok(mut state) = self.lifecycle.state.lock() {
            *state = StartupLifecycleState::Closed;
        }
        match shutdown_result.map(|result| result.kind) {
            Some(StartupResultKind::Stopped) => Ok(()),
            Some(StartupResultKind::ReconcileRequired) => {
                Err(StartupError::category(StartupErrorCategory::OutcomeUnknown))
            }
            _ => Err(StartupError::category(StartupErrorCategory::Unavailable)),
        }
    }
}

impl Drop for StartupSettingsOwner {
    /// 异常展开时尽力停止线程；显式关闭仍由调用方取得稳定错误。
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.begin_closing();
            let _ = self.finish_shutdown();
        }
    }
}

/// Windows 后端；每次操作都重新打开 HKCU Run，避免把句柄跨 owner 生命周期泄漏。
pub struct WindowsRegistryBackend {
    /// 进程内互斥只串行化 ClipboardBoard 自身，外部变化仍由 expected-state 复核发现。
    mutation_lock: &'static Mutex<()>,
}

impl WindowsRegistryBackend {
    /// 构造生产注册表后端。
    pub fn new() -> Self {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        Self {
            mutation_lock: LOCK.get_or_init(|| Mutex::new(())),
        }
    }
}

impl Default for WindowsRegistryBackend {
    /// 使用默认 HKCU Run 后端。
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryBackend for WindowsRegistryBackend {
    fn read(&mut self) -> Result<RegistrySnapshot, RegistryError> {
        read_windows_run_value()
    }

    fn set_if_matches(
        &mut self,
        expected: &RegistrySnapshot,
        replacement: &RegistryValue,
    ) -> Result<(), RegistryError> {
        let _guard = self
            .mutation_lock
            .lock()
            .map_err(|_| RegistryError::OutcomeUnknown)?;
        if read_windows_run_value()? != *expected {
            return Err(RegistryError::Conflict);
        }
        let key = open_run_key(KEY_QUERY_VALUE | KEY_SET_VALUE, true)?;
        let result = (|| {
            if read_key_value(key)? != *expected {
                return Err(RegistryError::Conflict);
            }
            write_key_value(key, replacement)
        })();
        unsafe {
            RegCloseKey(key);
        }
        result
    }

    fn delete_if_matches(&mut self, expected: &RegistrySnapshot) -> Result<(), RegistryError> {
        let _guard = self
            .mutation_lock
            .lock()
            .map_err(|_| RegistryError::OutcomeUnknown)?;
        if expected.is_none() {
            return Ok(());
        }
        if read_windows_run_value()? != *expected {
            return Err(RegistryError::Conflict);
        }
        let key = open_run_key(KEY_QUERY_VALUE | KEY_SET_VALUE, false)?;
        let result = (|| {
            if read_key_value(key)? != *expected {
                return Err(RegistryError::Conflict);
            }
            let name = wide_z(RUN_VALUE_NAME);
            let code = unsafe { RegDeleteValueW(key, name.as_ptr()) };
            if code == ERROR_SUCCESS {
                Ok(())
            } else if code == ERROR_FILE_NOT_FOUND {
                Err(RegistryError::Conflict)
            } else {
                Err(map_registry_code(code))
            }
        })();
        unsafe {
            RegCloseKey(key);
        }
        result
    }
}

/// owner 主循环；设置快照与 registry 后端都只在此线程访问。
fn owner_main(
    settings: SettingsClient,
    mut backend: Box<dyn RegistryBackend>,
    canonical: OsString,
    receiver: Receiver<StartupCommand>,
    lifecycle: Arc<StartupLifecycle>,
    generation: Arc<AtomicU64>,
    in_flight: Arc<AtomicBool>,
) {
    let mut tombstone_generation = 1_u64;
    let mut pending_reconcile: Option<TransactionState> = None;
    while let Ok(command) = receiver.recv() {
        // generation 是发送端与 worker 共享的单调计数器。即使旧命令已经
        // 在容量一的队列中，advance_generation 也会在这里把它标成迟到。
        tombstone_generation = tombstone_generation.max(generation.load(Ordering::Acquire));
        let result = if command.kind != StartupCommandKind::Shutdown
            && command.generation < tombstone_generation
        {
            StartupResult {
                transaction_id: command.transaction_id,
                generation: command.generation,
                kind: StartupResultKind::Stale,
                error: None,
            }
        } else {
            match command.kind {
                StartupCommandKind::Enable => {
                    if pending_reconcile.is_some() {
                        return_reconcile_required(command.transaction_id, command.generation)
                    } else {
                        run_transaction(
                            &settings,
                            backend.as_mut(),
                            &canonical,
                            StartupIntent::Enable,
                            command.expected_revision,
                            None,
                            command.transaction_id,
                            command.generation,
                            &mut pending_reconcile,
                        )
                    }
                }
                StartupCommandKind::Disable => {
                    if pending_reconcile.is_some() {
                        return_reconcile_required(command.transaction_id, command.generation)
                    } else {
                        run_transaction(
                            &settings,
                            backend.as_mut(),
                            &canonical,
                            StartupIntent::Disable,
                            command.expected_revision,
                            None,
                            command.transaction_id,
                            command.generation,
                            &mut pending_reconcile,
                        )
                    }
                }
                StartupCommandKind::Query => query_status(
                    &settings,
                    backend.as_mut(),
                    &canonical,
                    command.transaction_id,
                    command.generation,
                ),
                StartupCommandKind::Retry => {
                    if let Some(state) = pending_reconcile.take() {
                        // Retry 允许无关 settings 字段推进 revision，但仍要确认
                        // pending 事务记录的 startup 期望没有被外部改写。
                        let expected_startup = state.old_settings.settings().startup.run_on_login;
                        let result = run_transaction(
                            &settings,
                            backend.as_mut(),
                            &canonical,
                            state.intent,
                            0,
                            Some(expected_startup),
                            command.transaction_id,
                            command.generation,
                            &mut pending_reconcile,
                        );
                        // Retry 只有在状态已经被明确证明完成时才能清空对账槽。
                        // PendingRetry/Conflict/ReconcileRequired 必须保留旧 state，
                        // 否则下一次 Enable/Disable 会绕过未知状态继续写入。
                        if !matches!(
                            result.kind,
                            StartupResultKind::Applied
                                | StartupResultKind::AlreadyApplied
                                | StartupResultKind::SaveFailed
                        ) && pending_reconcile.is_none()
                        {
                            pending_reconcile.replace(state);
                        }
                        result
                    } else {
                        query_status(
                            &settings,
                            backend.as_mut(),
                            &canonical,
                            command.transaction_id,
                            command.generation,
                        )
                    }
                }
                StartupCommandKind::Shutdown => {
                    let result = if let Some(state) = pending_reconcile.take() {
                        // 关闭前最多自动重试一次；成功或已补偿才允许静默停止。
                        let expected_startup = state.old_settings.settings().startup.run_on_login;
                        let retry_result = run_transaction(
                            &settings,
                            backend.as_mut(),
                            &canonical,
                            state.intent,
                            0,
                            Some(expected_startup),
                            command.transaction_id,
                            command.generation,
                            &mut pending_reconcile,
                        );
                        if matches!(
                            retry_result.kind,
                            StartupResultKind::Applied
                                | StartupResultKind::AlreadyApplied
                                | StartupResultKind::SaveFailed
                        ) {
                            StartupResult {
                                transaction_id: command.transaction_id,
                                generation: command.generation,
                                kind: StartupResultKind::Stopped,
                                error: None,
                            }
                        } else {
                            if pending_reconcile.is_none() {
                                pending_reconcile.replace(state);
                            }
                            let result = StartupResult {
                                transaction_id: command.transaction_id,
                                generation: command.generation,
                                kind: StartupResultKind::ReconcileRequired,
                                error: Some(StartupErrorCategory::OutcomeUnknown),
                            };
                            // 不把未解决槽位伪装成 Stopped；owner 保持 Closing，
                            // 允许专用 Retry 继续对账，finish_shutdown 也不会 join。
                            let _ = command.reply.send(result);
                            continue;
                        }
                    } else {
                        StartupResult {
                            transaction_id: command.transaction_id,
                            generation: command.generation,
                            kind: StartupResultKind::Stopped,
                            error: None,
                        }
                    };
                    let _ = command.reply.send(result);
                    break;
                }
            }
        };
        // 关闭命令不会由 submit 设置门禁；普通命令必须在结果已发出后
        // 释放门禁，使回执接收方一醒来就能得到真实 Busy/可入队判定。
        if command.kind != StartupCommandKind::Shutdown {
            in_flight.store(false, Ordering::Release);
        }
        let _ = command.reply.send(result);
    }
    if let Ok(mut state) = lifecycle.state.lock() {
        *state = StartupLifecycleState::Closed;
    }
}

/// pending_reconcile 存在时拒绝新的 Enable/Disable，避免在未知状态上叠加写入。
fn return_reconcile_required(transaction_id: NonZeroU64, generation: u64) -> StartupResult {
    StartupResult {
        transaction_id,
        generation,
        kind: StartupResultKind::ReconcileRequired,
        error: Some(StartupErrorCategory::OutcomeUnknown),
    }
}

/// 查询配置与 Run 观测值并映射到 effective 状态。
fn query_status(
    settings: &SettingsClient,
    backend: &mut dyn RegistryBackend,
    canonical: &OsString,
    transaction_id: NonZeroU64,
    generation: u64,
) -> StartupResult {
    let snapshot = settings.snapshot();
    let registry = backend.read();
    let state = match (snapshot, registry) {
        (Ok(snapshot), Ok(value)) => {
            effective_state(snapshot.settings().startup.run_on_login, &value, canonical)
        }
        (Err(error), _) => {
            return StartupResult {
                transaction_id,
                generation,
                kind: StartupResultKind::PendingRetry,
                error: Some(map_settings_error(&error)),
            }
        }
        (_, Err(error)) => {
            return registry_query_failure(transaction_id, generation, error);
        }
    };
    match state {
        Ok(state) => StartupResult {
            transaction_id,
            generation,
            kind: StartupResultKind::Status(state),
            error: None,
        },
        Err(error) if error.category == StartupErrorCategory::ForeignConflict => StartupResult {
            transaction_id,
            generation,
            kind: StartupResultKind::Status(EffectiveStartupState::Conflict),
            error: Some(error.category),
        },
        Err(error) if error.category == StartupErrorCategory::InvalidInput => StartupResult {
            transaction_id,
            generation,
            kind: StartupResultKind::Status(EffectiveStartupState::InvalidValue),
            error: Some(error.category),
        },
        Err(error) => StartupResult {
            transaction_id,
            generation,
            kind: StartupResultKind::Status(EffectiveStartupState::Unknown),
            error: Some(error.category),
        },
    }
}

/// 查询注册表失败时优先转成 effective 状态，避免把确定的冲突/权限伪装成普通重试。
fn registry_query_failure(
    transaction_id: NonZeroU64,
    generation: u64,
    error: RegistryError,
) -> StartupResult {
    let (state, category) = match error {
        RegistryError::PermissionDenied => (
            EffectiveStartupState::PermissionDenied,
            StartupErrorCategory::PermissionDenied,
        ),
        RegistryError::InvalidData => (
            EffectiveStartupState::InvalidValue,
            StartupErrorCategory::InvalidInput,
        ),
        RegistryError::Conflict => (
            EffectiveStartupState::Conflict,
            StartupErrorCategory::ForeignConflict,
        ),
        RegistryError::OutcomeUnknown => (
            EffectiveStartupState::Unknown,
            StartupErrorCategory::OutcomeUnknown,
        ),
        RegistryError::NotFound | RegistryError::Unavailable(_) => {
            return StartupResult {
                transaction_id,
                generation,
                kind: StartupResultKind::PendingRetry,
                error: Some(map_registry_error(error)),
            }
        }
    };
    StartupResult {
        transaction_id,
        generation,
        kind: StartupResultKind::Status(state),
        error: Some(category),
    }
}

/// 执行注册表先行、配置 CAS 后行的双资源事务，并在失败时做 ownership guard 补偿。
///
/// 事务需要同时携带两类资源、命令身份和对账槽位；这些参数刻意保持显式，
/// 以便审查者能看到每个资源与 generation 的边界，不通过隐式全局状态传递。
#[allow(clippy::too_many_arguments)]
fn run_transaction(
    settings: &SettingsClient,
    backend: &mut dyn RegistryBackend,
    canonical: &OsString,
    intent: StartupIntent,
    expected_revision: u64,
    expected_startup: Option<bool>,
    transaction_id: NonZeroU64,
    generation: u64,
    pending_reconcile: &mut Option<TransactionState>,
) -> StartupResult {
    let old_settings = match settings.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return StartupResult {
                transaction_id,
                generation,
                kind: StartupResultKind::PendingRetry,
                error: Some(map_settings_error(&error)),
            }
        }
    };
    if expected_revision != 0 && expected_revision != old_settings.revision() {
        return StartupResult {
            transaction_id,
            generation,
            kind: StartupResultKind::Conflict,
            error: Some(StartupErrorCategory::SettingsConflict),
        };
    }
    if let Some(expected_startup) = expected_startup {
        if old_settings.settings().startup.run_on_login != expected_startup {
            // Retry/Shutdown 不再使用过期 revision 做整份配置拒绝，
            // 但 pending 事务的 startup 字段若已被外部改变，仍必须阻断覆盖。
            return StartupResult {
                transaction_id,
                generation,
                kind: StartupResultKind::Conflict,
                error: Some(StartupErrorCategory::SettingsConflict),
            };
        }
    }
    let old_registry = match backend.read() {
        Ok(value) => value,
        Err(RegistryError::InvalidData) => {
            return StartupResult {
                transaction_id,
                generation,
                kind: StartupResultKind::Conflict,
                error: Some(StartupErrorCategory::InvalidInput),
            }
        }
        Err(error) => {
            return StartupResult {
                transaction_id,
                generation,
                kind: StartupResultKind::PendingRetry,
                error: Some(map_registry_error(error)),
            }
        }
    };
    let ownership = match classify_registry(&old_registry, canonical) {
        Ok(value) => value,
        Err(error) => {
            return StartupResult {
                transaction_id,
                generation,
                kind: StartupResultKind::Conflict,
                error: Some(error.category),
            }
        }
    };
    if ownership == RegistryOwnership::Foreign {
        return StartupResult {
            transaction_id,
            generation,
            kind: StartupResultKind::Conflict,
            error: Some(StartupErrorCategory::ForeignConflict),
        };
    }
    let desired_registry = match intent {
        StartupIntent::Enable => Some(RegistryValue {
            value_type: REG_SZ,
            data: utf16_z(canonical),
        }),
        StartupIntent::Disable => None,
    };
    if matches!(intent, StartupIntent::Enable) && ownership == RegistryOwnership::Owned {
        // 仍需保存配置字段，但注册表动作本身已经幂等完成。
    } else if matches!(intent, StartupIntent::Disable) && ownership == RegistryOwnership::Missing {
        // 缺失值删除是幂等成功，继续 CAS 配置字段。
    } else {
        let mutation = match desired_registry.as_ref() {
            Some(replacement) => backend.set_if_matches(&old_registry, replacement),
            None => backend.delete_if_matches(&old_registry),
        };
        if let Err(error) = mutation {
            return mutation_failure(
                settings,
                backend,
                transaction_id,
                generation,
                error,
                pending_reconcile,
                TransactionState {
                    intent,
                    old_settings: old_settings.clone(),
                    old_registry: old_registry.clone(),
                    written_registry: desired_registry.clone(),
                },
            );
        }
        match backend.read() {
            Ok(actual) if actual == desired_registry => {}
            Ok(_) | Err(_) => {
                // 写入后的 mismatch/read error 同样不能直接占槽：重新读取
                // settings+registry，若两份状态可证明已应用或仍为旧值则
                // 立即收口，否则才保留 ReconcileRequired。
                return reconcile_unknown_settings_outcome(
                    settings,
                    backend,
                    transaction_id,
                    generation,
                    pending_reconcile,
                    TransactionState {
                        intent,
                        old_settings: old_settings.clone(),
                        old_registry: old_registry.clone(),
                        written_registry: desired_registry.clone(),
                    },
                );
            }
        }
    }

    let mut latest = match settings.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return compensate_after_settings_failure(
                settings,
                backend,
                transaction_id,
                generation,
                pending_reconcile,
                TransactionState {
                    intent,
                    old_settings,
                    old_registry,
                    written_registry: desired_registry,
                },
                map_settings_error(&error),
            )
        }
    };
    if startup_revision_conflicts(&old_settings, &latest) {
        return compensate_after_settings_failure(
            settings,
            backend,
            transaction_id,
            generation,
            pending_reconcile,
            TransactionState {
                intent,
                old_settings,
                old_registry,
                written_registry: desired_registry,
            },
            StartupErrorCategory::SettingsConflict,
        );
    }

    let target = matches!(intent, StartupIntent::Enable);
    for _ in 0..=SETTINGS_RETRY_LIMIT {
        let mut candidate = latest.settings().clone();
        candidate.startup.run_on_login = target;
        match settings.save(latest.revision(), candidate) {
            Ok(_) => {
                return StartupResult {
                    transaction_id,
                    generation,
                    kind: if latest.settings().startup.run_on_login == target {
                        StartupResultKind::AlreadyApplied
                    } else {
                        StartupResultKind::Applied
                    },
                    error: None,
                }
            }
            Err(SettingsError::RevisionConflict { .. }) => match settings.snapshot() {
                Ok(snapshot) => {
                    // 冲突刷新后的快照同样必须经过 startup 字段守卫；否则
                    // 下一轮会把外部反向修改当作普通 revision 重试而覆盖。
                    if startup_revision_conflicts(&old_settings, &snapshot) {
                        return compensate_after_settings_failure(
                            settings,
                            backend,
                            transaction_id,
                            generation,
                            pending_reconcile,
                            TransactionState {
                                intent,
                                old_settings,
                                old_registry,
                                written_registry: desired_registry,
                            },
                            StartupErrorCategory::SettingsConflict,
                        );
                    }
                    latest = snapshot;
                }
                Err(error) => {
                    return compensate_after_settings_failure(
                        settings,
                        backend,
                        transaction_id,
                        generation,
                        pending_reconcile,
                        TransactionState {
                            intent,
                            old_settings,
                            old_registry,
                            written_registry: desired_registry,
                        },
                        map_settings_error(&error),
                    )
                }
            },
            Err(error) => {
                return compensate_after_settings_failure(
                    settings,
                    backend,
                    transaction_id,
                    generation,
                    pending_reconcile,
                    TransactionState {
                        intent,
                        old_settings,
                        old_registry,
                        written_registry: desired_registry,
                    },
                    map_settings_error(&error),
                )
            }
        }
    }

    compensate_after_settings_failure(
        settings,
        backend,
        transaction_id,
        generation,
        pending_reconcile,
        TransactionState {
            intent,
            old_settings,
            old_registry,
            written_registry: desired_registry,
        },
        StartupErrorCategory::SettingsConflict,
    )
}

/// 判断读取期间是否有另一调用方修改了 startup 字段；不依赖本次目标方向，
/// 因而 Enable/Disable 在 old==target 的幂等场景也不会覆盖外部反向修改。
fn startup_revision_conflicts(old: &SettingsSnapshot, latest: &SettingsSnapshot) -> bool {
    latest.revision() != old.revision()
        && latest.settings().startup.run_on_login != old.settings().startup.run_on_login
}

/// 注册表 mutation 失败的统一路由；未知状态必须保留对账上下文。
fn mutation_failure(
    settings: &SettingsClient,
    backend: &mut dyn RegistryBackend,
    transaction_id: NonZeroU64,
    generation: u64,
    error: RegistryError,
    pending_reconcile: &mut Option<TransactionState>,
    state: TransactionState,
) -> StartupResult {
    let category = map_registry_error(error);
    if error == RegistryError::OutcomeUnknown {
        // registry mutation 的未知回执也必须重新读取两份资源，不能直接
        // 把槽位标成待重试；写入可能已成功，或仍需 ownership guard 补偿。
        return reconcile_unknown_settings_outcome(
            settings,
            backend,
            transaction_id,
            generation,
            pending_reconcile,
            state,
        );
    }
    if error == RegistryError::Conflict {
        // CAS 冲突已经证明外部改值；这不是“未知”，不占用 pending 槽，
        // 让后续命令重新 Query 后再决定是否由当前 owner 接管。
        return StartupResult {
            transaction_id,
            generation,
            kind: StartupResultKind::Conflict,
            error: Some(StartupErrorCategory::ForeignConflict),
        };
    }
    if matches!(error, RegistryError::InvalidData) {
        return StartupResult {
            transaction_id,
            generation,
            kind: StartupResultKind::Conflict,
            error: Some(category),
        };
    }
    StartupResult {
        transaction_id,
        generation,
        kind: StartupResultKind::PendingRetry,
        error: Some(category),
    }
}

/// Settings 保存失败后，仅当 Run 值仍是本事务写入值才补偿旧值。
fn compensate_known_settings_failure(
    settings: &SettingsClient,
    backend: &mut dyn RegistryBackend,
    transaction_id: NonZeroU64,
    generation: u64,
    pending_reconcile: &mut Option<TransactionState>,
    state: TransactionState,
    error: StartupErrorCategory,
) -> StartupResult {
    let current = match backend.read() {
        Ok(value) => value,
        Err(_) => {
            pending_reconcile.replace(state);
            return StartupResult {
                transaction_id,
                generation,
                kind: StartupResultKind::ReconcileRequired,
                error: Some(StartupErrorCategory::OutcomeUnknown),
            };
        }
    };
    if current != state.written_registry {
        pending_reconcile.replace(state);
        return StartupResult {
            transaction_id,
            generation,
            kind: StartupResultKind::ReconcileRequired,
            error: Some(StartupErrorCategory::CompensationFailed),
        };
    }
    let compensation = match state.old_registry.as_ref() {
        Some(value) => backend.set_if_matches(&current, value),
        None => backend.delete_if_matches(&current),
    };
    match compensation {
        Ok(()) => StartupResult {
            transaction_id,
            generation,
            kind: StartupResultKind::SaveFailed,
            error: Some(error),
        },
        Err(_) => {
            pending_reconcile.replace(state);
            let _ = settings.snapshot();
            StartupResult {
                transaction_id,
                generation,
                kind: StartupResultKind::ReconcileRequired,
                error: Some(StartupErrorCategory::CompensationFailed),
            }
        }
    }
}

/// Settings 返回 OutcomeUnknown 时先读取两份权威状态，再决定已应用、可补偿或继续对账。
///
/// 未知回执不能直接走普通失败补偿：SettingsWorker 可能已经提交了新快照，
/// 这时把 Run 回滚会制造反向错配。只有确认 startup 字段仍是旧值后，才允许
/// 复用 ownership guard 补偿；任一读回失败或落入交叉状态都保留对账槽位。
fn compensate_after_settings_failure(
    settings: &SettingsClient,
    backend: &mut dyn RegistryBackend,
    transaction_id: NonZeroU64,
    generation: u64,
    pending_reconcile: &mut Option<TransactionState>,
    state: TransactionState,
    error: StartupErrorCategory,
) -> StartupResult {
    if error == StartupErrorCategory::OutcomeUnknown {
        return reconcile_unknown_settings_outcome(
            settings,
            backend,
            transaction_id,
            generation,
            pending_reconcile,
            state,
        );
    }
    compensate_known_settings_failure(
        settings,
        backend,
        transaction_id,
        generation,
        pending_reconcile,
        state,
        error,
    )
}

/// 未知回执的两资源对账路由。
fn reconcile_unknown_settings_outcome(
    settings: &SettingsClient,
    backend: &mut dyn RegistryBackend,
    transaction_id: NonZeroU64,
    generation: u64,
    pending_reconcile: &mut Option<TransactionState>,
    state: TransactionState,
) -> StartupResult {
    let current_settings = match settings.snapshot() {
        Ok(snapshot) => snapshot,
        Err(_) => {
            pending_reconcile.replace(state);
            return StartupResult {
                transaction_id,
                generation,
                kind: StartupResultKind::ReconcileRequired,
                error: Some(StartupErrorCategory::OutcomeUnknown),
            };
        }
    };
    let current_registry = match backend.read() {
        Ok(value) => value,
        Err(_) => {
            pending_reconcile.replace(state);
            return StartupResult {
                transaction_id,
                generation,
                kind: StartupResultKind::ReconcileRequired,
                error: Some(StartupErrorCategory::OutcomeUnknown),
            };
        }
    };

    match classify_unknown_reconciliation(&state, &current_settings, &current_registry) {
        UnknownReconciliation::Applied => StartupResult {
            transaction_id,
            generation,
            kind: StartupResultKind::Applied,
            error: None,
        },
        UnknownReconciliation::Compensate => compensate_known_settings_failure(
            settings,
            backend,
            transaction_id,
            generation,
            pending_reconcile,
            state,
            StartupErrorCategory::OutcomeUnknown,
        ),
        UnknownReconciliation::SaveFailed => StartupResult {
            transaction_id,
            generation,
            kind: StartupResultKind::SaveFailed,
            error: Some(StartupErrorCategory::OutcomeUnknown),
        },
        UnknownReconciliation::Reconcile => {
            pending_reconcile.replace(state);
            StartupResult {
                transaction_id,
                generation,
                kind: StartupResultKind::ReconcileRequired,
                error: Some(StartupErrorCategory::OutcomeUnknown),
            }
        }
    }
}

/// 未知回执的纯状态判定，便于用 fake 快照覆盖“已提交”和“仍为旧值”两分支。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnknownReconciliation {
    /// 两份资源都已经确认目标。
    Applied,
    /// 配置仍为旧值但 Run 已写入，需要 ownership guard 补偿。
    Compensate,
    /// 两份资源仍为旧值，确定没有发生持久化改变。
    SaveFailed,
    /// 交叉或未知状态，必须保留对账上下文。
    Reconcile,
}

fn classify_unknown_reconciliation(
    state: &TransactionState,
    current_settings: &SettingsSnapshot,
    current_registry: &RegistrySnapshot,
) -> UnknownReconciliation {
    let target = matches!(state.intent, StartupIntent::Enable);
    let settings_is_target = current_settings.settings().startup.run_on_login == target;
    let settings_is_old = current_settings.settings().startup.run_on_login
        == state.old_settings.settings().startup.run_on_login;
    let registry_is_written = current_registry == &state.written_registry;
    let registry_is_old = current_registry == &state.old_registry;
    if settings_is_target && registry_is_written {
        UnknownReconciliation::Applied
    } else if settings_is_old && registry_is_written {
        UnknownReconciliation::Compensate
    } else if settings_is_old && registry_is_old {
        UnknownReconciliation::SaveFailed
    } else {
        UnknownReconciliation::Reconcile
    }
}

/// 注册表值在当前 canonical 命令行下的 ownership 分类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegistryOwnership {
    /// 值缺失。
    Missing,
    /// 值正好由当前程序拥有。
    Owned,
    /// 值存在但属于其他程序。
    Foreign,
}

/// 严格校验并分类 Run 值；不会对 foreign value 做任何写操作。
fn classify_registry(
    value: &RegistrySnapshot,
    canonical: &OsString,
) -> Result<RegistryOwnership, StartupError> {
    let Some(value) = value else {
        return Ok(RegistryOwnership::Missing);
    };
    if value.value_type != REG_SZ {
        return Err(StartupError::category(
            StartupErrorCategory::ForeignConflict,
        ));
    }
    let decoded = decode_registry_string(&value.data)?;
    if decoded == *canonical {
        Ok(RegistryOwnership::Owned)
    } else {
        Ok(RegistryOwnership::Foreign)
    }
}

/// 组合配置期望与 Run 观测值；错配只报告，不主动修复。
fn effective_state(
    expected_enabled: bool,
    value: &RegistrySnapshot,
    canonical: &OsString,
) -> Result<EffectiveStartupState, StartupError> {
    match classify_registry(value, canonical)? {
        RegistryOwnership::Missing if expected_enabled => Ok(EffectiveStartupState::Missing),
        RegistryOwnership::Missing => Ok(EffectiveStartupState::Disabled),
        RegistryOwnership::Owned if expected_enabled => Ok(EffectiveStartupState::Enabled),
        RegistryOwnership::Owned => Ok(EffectiveStartupState::Mismatch),
        RegistryOwnership::Foreign => Ok(EffectiveStartupState::Conflict),
    }
}

/// 把当前程序路径编码为 UTF-16 的单参数命令行，拒绝可改变解析边界的字符。
pub fn quote_windows_single_argument(path: &OsStr) -> Result<OsString, StartupError> {
    let units: Vec<u16> = path.encode_wide().collect();
    validate_utf16_path(&units)?;
    let mut result = Vec::with_capacity(units.len().saturating_add(2));
    result.push(b'"' as u16);
    let mut slash_run = 0_usize;
    for unit in units {
        if unit == b'\\' as u16 {
            slash_run = slash_run.saturating_add(1);
            continue;
        }
        result.extend(std::iter::repeat_n(b'\\' as u16, slash_run));
        slash_run = 0;
        result.push(unit);
    }
    result.extend(std::iter::repeat_n(
        b'\\' as u16,
        slash_run.saturating_mul(2),
    ));
    result.push(b'"' as u16);
    Ok(OsString::from_wide(&result))
}

/// 校验 UTF-16 配对、NUL、控制字符和引号；路径中的引号永远不进入命令行。
fn validate_utf16_path(units: &[u16]) -> Result<(), StartupError> {
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        if unit == 0 || unit == b'"' as u16 || unit <= 0x1f || unit == 0x7f {
            return Err(StartupError::category(StartupErrorCategory::InvalidInput));
        }
        if (0xd800..=0xdbff).contains(&unit) {
            let Some(next) = units.get(index + 1) else {
                return Err(StartupError::category(StartupErrorCategory::InvalidInput));
            };
            if !(0xdc00..=0xdfff).contains(next) {
                return Err(StartupError::category(StartupErrorCategory::InvalidInput));
            }
            index += 2;
            continue;
        }
        if (0xdc00..=0xdfff).contains(&unit) {
            return Err(StartupError::category(StartupErrorCategory::InvalidInput));
        }
        index += 1;
    }
    Ok(())
}

/// 只去除一个终止 NUL，额外 NUL 或内部 NUL 均视为非法注册表字符串。
fn decode_registry_string(data: &[u16]) -> Result<OsString, StartupError> {
    if data.is_empty() || data.last() != Some(&0) {
        return Err(StartupError::category(StartupErrorCategory::InvalidInput));
    }
    let body = &data[..data.len() - 1];
    validate_utf16_commandline(body)?;
    Ok(OsString::from_wide(body))
}

/// 校验已经带外层引号的命令行 UTF-16；允许引号本身但拒绝 NUL/控制字符和坏 surrogate。
fn validate_utf16_commandline(units: &[u16]) -> Result<(), StartupError> {
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        if unit == 0 || unit <= 0x1f || unit == 0x7f {
            return Err(StartupError::category(StartupErrorCategory::InvalidInput));
        }
        if (0xd800..=0xdbff).contains(&unit) {
            let Some(next) = units.get(index + 1) else {
                return Err(StartupError::category(StartupErrorCategory::InvalidInput));
            };
            if !(0xdc00..=0xdfff).contains(next) {
                return Err(StartupError::category(StartupErrorCategory::InvalidInput));
            }
            index += 2;
            continue;
        }
        if (0xdc00..=0xdfff).contains(&unit) {
            return Err(StartupError::category(StartupErrorCategory::InvalidInput));
        }
        index += 1;
    }
    Ok(())
}

/// 生成带终止 NUL 的 UTF-16 数据；调用方已经通过 quote 校验输入。
fn utf16_z(value: &OsString) -> Vec<u16> {
    let mut encoded: Vec<u16> = value.encode_wide().collect();
    encoded.push(0);
    encoded
}

/// 将 settings 错误映射为启动稳定类别。
fn map_settings_error(error: &SettingsError) -> StartupErrorCategory {
    match error {
        SettingsError::RevisionConflict { .. } => StartupErrorCategory::SettingsConflict,
        SettingsError::OutcomeUnknown => StartupErrorCategory::OutcomeUnknown,
        SettingsError::SettingsClosing
        | SettingsError::SettingsClosed
        | SettingsError::ChannelClosed => StartupErrorCategory::OwnerClosed,
        _ => StartupErrorCategory::Unavailable,
    }
}

/// 将后端错误映射为启动稳定类别。
fn map_registry_error(error: RegistryError) -> StartupErrorCategory {
    match error {
        RegistryError::PermissionDenied => StartupErrorCategory::PermissionDenied,
        RegistryError::NotFound | RegistryError::Unavailable(_) => {
            StartupErrorCategory::Unavailable
        }
        RegistryError::InvalidData => StartupErrorCategory::InvalidInput,
        RegistryError::Conflict => StartupErrorCategory::ForeignConflict,
        RegistryError::OutcomeUnknown => StartupErrorCategory::OutcomeUnknown,
    }
}

/// 创建以 NUL 结尾的 Win32 UTF-16 字符串。
fn wide_z(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 打开 HKCU Run；enable 会创建缺失子键，query/disable 不创建。
fn open_run_key(access: u32, create: bool) -> Result<HKEY, RegistryError> {
    let subkey = wide_z(RUN_SUBKEY);
    let mut key = null_mut();
    let code = if create {
        let mut disposition = 0_u32;
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                null_mut(),
                0,
                access,
                null(),
                &mut key,
                &mut disposition,
            )
        }
    } else {
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, access, &mut key) }
    };
    if code == ERROR_SUCCESS {
        Ok(key)
    } else {
        Err(map_registry_code(code))
    }
}

/// 读取固定值名的原始 REG_* 数据。
fn read_windows_run_value() -> Result<RegistrySnapshot, RegistryError> {
    let key = match open_run_key(KEY_QUERY_VALUE, false) {
        Ok(key) => key,
        Err(RegistryError::NotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    let result = read_key_value(key);
    unsafe {
        RegCloseKey(key);
    }
    result
}

/// 使用 RegQueryValueExW 两阶段读取，并限制最大字节数。
fn read_key_value(key: HKEY) -> Result<RegistrySnapshot, RegistryError> {
    let name = wide_z(RUN_VALUE_NAME);
    let mut value_type = 0_u32;
    let mut size = 0_u32;
    let mut code = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            null_mut(),
            &mut value_type,
            null_mut(),
            &mut size,
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if code != ERROR_SUCCESS && code != ERROR_MORE_DATA {
        return Err(map_registry_code(code));
    }
    if usize::try_from(size)
        .ok()
        .filter(|size| *size <= MAX_REGISTRY_DATA_BYTES)
        .is_none()
    {
        return Err(RegistryError::InvalidData);
    }
    let mut bytes = vec![0_u8; size as usize];
    code = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            null_mut(),
            &mut value_type,
            bytes.as_mut_ptr(),
            &mut size,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(map_registry_code(code));
    }
    bytes.truncate(size as usize);
    if !bytes.len().is_multiple_of(2) {
        return Err(RegistryError::InvalidData);
    }
    let data = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();
    Ok(Some(RegistryValue { value_type, data }))
}

/// 写入 REG_SZ 原始 UTF-16 数据，明确传递包含终止 NUL 的字节长度。
fn write_key_value(key: HKEY, value: &RegistryValue) -> Result<(), RegistryError> {
    if value.value_type != REG_SZ || value.data.is_empty() || value.data.last() != Some(&0) {
        return Err(RegistryError::InvalidData);
    }
    let byte_len = value
        .data
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .filter(|len| *len <= MAX_REGISTRY_DATA_BYTES)
        .ok_or(RegistryError::InvalidData)?;
    let mut bytes = Vec::with_capacity(byte_len);
    for unit in &value.data {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let name = wide_z(RUN_VALUE_NAME);
    let code = unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            REG_SZ,
            bytes.as_ptr(),
            u32::try_from(bytes.len()).map_err(|_| RegistryError::InvalidData)?,
        )
    };
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(map_registry_code(code))
    }
}

/// 把 Win32 注册表错误码映射为稳定 fake/生产错误。
fn map_registry_code(code: u32) -> RegistryError {
    match code {
        ERROR_ACCESS_DENIED => RegistryError::PermissionDenied,
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => RegistryError::NotFound,
        _ => RegistryError::Unavailable(Some(code)),
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块只使用 fake backend 和显式临时 SettingsWorker，不触碰真实 HKCU。

    use super::*;
    use crate::settings::{
        AppSettings, SettingsLoadSource, SettingsSnapshot, SettingsWorker, StartupSettings,
    };

    /// 返回带 NUL 的 fake REG_SZ 值。
    fn registry_value(command: &OsStr) -> RegistryValue {
        RegistryValue {
            value_type: REG_SZ,
            data: utf16_z(&quote_windows_single_argument(command).unwrap()),
        }
    }

    /// 可注入故障与外部改值的 fake 注册表。
    struct FakeRegistry {
        /// 当前固定值。
        value: RegistrySnapshot,
        /// 下一次写入前替换为外部值，模拟进程间并发。
        external_before_mutation: Option<RegistrySnapshot>,
        /// 下一次 read 返回未知。
        read_unknown: bool,
        /// 下一次 mutation 成功后，读回返回未知的预置标志。
        unknown_after_mutation: bool,
        /// 下一次 mutation 成功后连续若干次读回返回未知，用于持久失败 fake。
        unknown_after_mutation_reads: u8,
        /// mutation 已成功，下一次读回返回未知。
        read_unknown_after_mutation: bool,
        /// 持久失败 fake 剩余的未知读回次数。
        read_unknown_remaining: u8,
        /// 下一次 read 返回权限拒绝。
        read_permission_denied: bool,
        /// 下一次 read 返回损坏数据。
        read_invalid_data: bool,
        /// 下一次 mutation 返回未知。
        mutation_unknown: bool,
        /// 下一次 mutation 返回权限拒绝。
        mutation_permission_denied: bool,
    }

    impl FakeRegistry {
        /// 构造指定初始值的 fake。
        fn new(value: RegistrySnapshot) -> Self {
            Self {
                value,
                external_before_mutation: None,
                read_unknown: false,
                unknown_after_mutation: false,
                unknown_after_mutation_reads: 0,
                read_unknown_after_mutation: false,
                read_unknown_remaining: 0,
                read_permission_denied: false,
                read_invalid_data: false,
                mutation_unknown: false,
                mutation_permission_denied: false,
            }
        }
    }

    impl RegistryBackend for FakeRegistry {
        fn read(&mut self) -> Result<RegistrySnapshot, RegistryError> {
            if self.read_unknown {
                self.read_unknown = false;
                return Err(RegistryError::OutcomeUnknown);
            }
            if self.read_unknown_after_mutation {
                self.read_unknown_after_mutation = false;
                return Err(RegistryError::OutcomeUnknown);
            }
            if self.read_unknown_remaining > 0 {
                self.read_unknown_remaining = self.read_unknown_remaining.saturating_sub(1);
                return Err(RegistryError::OutcomeUnknown);
            }
            if self.read_permission_denied {
                self.read_permission_denied = false;
                return Err(RegistryError::PermissionDenied);
            }
            if self.read_invalid_data {
                self.read_invalid_data = false;
                return Err(RegistryError::InvalidData);
            }
            Ok(self.value.clone())
        }

        fn set_if_matches(
            &mut self,
            expected: &RegistrySnapshot,
            replacement: &RegistryValue,
        ) -> Result<(), RegistryError> {
            if self.mutation_permission_denied {
                self.mutation_permission_denied = false;
                return Err(RegistryError::PermissionDenied);
            }
            if let Some(external) = self.external_before_mutation.take() {
                self.value = external;
            }
            if self.mutation_unknown {
                self.mutation_unknown = false;
                return Err(RegistryError::OutcomeUnknown);
            }
            if self.value != *expected {
                return Err(RegistryError::Conflict);
            }
            self.value = Some(replacement.clone());
            if self.unknown_after_mutation {
                self.unknown_after_mutation = false;
                self.read_unknown_after_mutation = true;
            }
            if self.unknown_after_mutation_reads > 0 {
                self.read_unknown_remaining = self.unknown_after_mutation_reads;
                self.unknown_after_mutation_reads = 0;
            }
            Ok(())
        }

        fn delete_if_matches(&mut self, expected: &RegistrySnapshot) -> Result<(), RegistryError> {
            if self.mutation_permission_denied {
                self.mutation_permission_denied = false;
                return Err(RegistryError::PermissionDenied);
            }
            if let Some(external) = self.external_before_mutation.take() {
                self.value = external;
            }
            if self.mutation_unknown {
                self.mutation_unknown = false;
                return Err(RegistryError::OutcomeUnknown);
            }
            if self.value != *expected {
                return Err(RegistryError::Conflict);
            }
            self.value = None;
            if self.unknown_after_mutation {
                self.unknown_after_mutation = false;
                self.read_unknown_after_mutation = true;
            }
            if self.unknown_after_mutation_reads > 0 {
                self.read_unknown_remaining = self.unknown_after_mutation_reads;
                self.unknown_after_mutation_reads = 0;
            }
            Ok(())
        }
    }

    /// 单参数 quoting 必须保留反斜杠并为末尾反斜杠加倍。
    #[test]
    fn quoting_handles_spaces_and_trailing_slashes() {
        let quoted =
            quote_windows_single_argument(OsStr::new(r"C:\Program Files\ClipboardBoard\\"))
                .unwrap();
        assert_eq!(
            quoted.to_string_lossy(),
            r#""C:\Program Files\ClipboardBoard\\\\""#
        );
    }

    /// 控制字符、NUL、引号和孤立 surrogate 都拒绝。
    #[test]
    fn quoting_rejects_unsafe_utf16() {
        let nul = OsString::from_wide(&[b'C' as u16, b':' as u16, b'\\' as u16, 0]);
        assert_eq!(
            quote_windows_single_argument(&nul).unwrap_err().category,
            StartupErrorCategory::InvalidInput
        );
        let quoted = OsString::from(r#"C:\"bad"#);
        assert_eq!(
            quote_windows_single_argument(&quoted).unwrap_err().category,
            StartupErrorCategory::InvalidInput
        );
        let invalid = OsString::from_wide(&[0xd800]);
        assert_eq!(
            quote_windows_single_argument(&invalid)
                .unwrap_err()
                .category,
            StartupErrorCategory::InvalidInput
        );
    }

    /// REG_SZ 读回只允许一个终止 NUL，额外 NUL 不得成为 owner 值。
    #[test]
    fn registry_decode_requires_single_terminator() {
        assert!(decode_registry_string(&[b'a' as u16, 0]).is_ok());
        assert!(decode_registry_string(&[b'a' as u16]).is_err());
        assert!(decode_registry_string(&[b'a' as u16, 0, 0]).is_err());
    }

    /// 配置期望和 Run 观测的八类状态映射固定。
    #[test]
    fn effective_state_maps_mismatch_without_repair() {
        let canonical =
            quote_windows_single_argument(OsStr::new(r"C:\ClipboardBoard.exe")).unwrap();
        let owned = Some(registry_value(OsStr::new(r"C:\ClipboardBoard.exe")));
        let missing = None;
        assert_eq!(
            effective_state(false, &missing, &canonical).unwrap(),
            EffectiveStartupState::Disabled
        );
        assert_eq!(
            effective_state(false, &owned, &canonical).unwrap(),
            EffectiveStartupState::Mismatch
        );
        assert_eq!(
            effective_state(true, &owned, &canonical).unwrap(),
            EffectiveStartupState::Enabled
        );
        assert_eq!(
            effective_state(true, &missing, &canonical).unwrap(),
            EffectiveStartupState::Missing
        );
        let wrong_type = Some(RegistryValue {
            value_type: 2,
            data: utf16_z(&canonical),
        });
        assert_eq!(
            classify_registry(&wrong_type, &canonical)
                .unwrap_err()
                .category,
            StartupErrorCategory::ForeignConflict
        );
    }

    /// fake expected-state 守卫在外部改值前必须拒绝写入。
    #[test]
    fn fake_backend_rejects_external_mutation() {
        let mut fake = FakeRegistry::new(None);
        fake.external_before_mutation = Some(Some(registry_value(OsStr::new(r"C:\other.exe"))));
        let replacement = registry_value(OsStr::new(r"C:\ClipboardBoard.exe"));
        assert_eq!(
            fake.set_if_matches(&None, &replacement),
            Err(RegistryError::Conflict)
        );
        assert_eq!(
            fake.value,
            Some(registry_value(OsStr::new(r"C:\other.exe")))
        );
    }

    /// 设置字段缺省 false 并能通过完整 DTO 保存。
    #[test]
    fn startup_setting_defaults_and_round_trips() {
        let directory = std::env::temp_dir().join(format!(
            "clipboard-board-startup-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let worker = SettingsWorker::start_at(&directory).unwrap();
        let client = worker.client();
        let snapshot = client.snapshot().unwrap();
        assert!(!snapshot.settings().startup.run_on_login);
        let mut settings = snapshot.settings().clone();
        settings.startup = StartupSettings { run_on_login: true };
        let saved = client.save(snapshot.revision(), settings).unwrap();
        assert!(saved.settings().startup.run_on_login);
        let mut worker = worker;
        worker.begin_closing().unwrap();
        worker.finish_shutdown().unwrap();
        let _ = std::fs::remove_dir_all(directory);
    }

    /// fake owner enable/disable 事务只写自身值，其他 Run 值在冲突时保持不变。
    #[test]
    fn owner_conflict_does_not_overwrite_foreign_value() {
        let directory = std::env::temp_dir().join(format!(
            "clipboard-board-startup-owner-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let worker = SettingsWorker::start_at(&directory).unwrap();
        let foreign = registry_value(OsStr::new(r"C:\other.exe"));
        let fake = FakeRegistry::new(Some(foreign.clone()));
        let mut owner = StartupSettingsOwner::start_with_backend(
            worker.client(),
            Box::new(fake),
            OsString::from(r"C:\ClipboardBoard.exe"),
        )
        .unwrap();
        let sender = owner.sender();
        let result = sender.try_enable(0).unwrap().recv().unwrap();
        assert_eq!(result.kind, StartupResultKind::Conflict);
        owner.begin_closing().unwrap();
        owner.finish_shutdown().unwrap();
        let mut worker = worker;
        worker.begin_closing().unwrap();
        worker.finish_shutdown().unwrap();
        let _ = std::fs::remove_dir_all(directory);
    }

    /// Disable 缺失值幂等，Enable→Disable 重复操作只产生 Applied/AlreadyApplied。
    #[test]
    fn owner_disable_missing_and_repeat_are_idempotent() {
        let directory = std::env::temp_dir().join(format!(
            "clipboard-board-startup-repeat-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let worker = SettingsWorker::start_at(&directory).unwrap();
        let mut owner = StartupSettingsOwner::start_with_backend(
            worker.client(),
            Box::new(FakeRegistry::new(None)),
            OsString::from(r"C:\ClipboardBoard.exe"),
        )
        .unwrap();
        let sender = owner.sender();
        assert_eq!(
            sender.try_disable(0).unwrap().recv().unwrap().kind,
            StartupResultKind::AlreadyApplied
        );
        assert_eq!(
            sender.try_enable(0).unwrap().recv().unwrap().kind,
            StartupResultKind::Applied
        );
        assert_eq!(
            sender.try_disable(0).unwrap().recv().unwrap().kind,
            StartupResultKind::Applied
        );
        owner.begin_closing().unwrap();
        owner.finish_shutdown().unwrap();
        let mut worker = worker;
        worker.begin_closing().unwrap();
        worker.finish_shutdown().unwrap();
        let _ = std::fs::remove_dir_all(directory);
    }

    /// mutation 发生 CAS 冲突时只报告 ForeignConflict，不把确定冲突误记为未知对账。
    #[test]
    fn owner_mutation_cas_conflict_is_not_pending_unknown() {
        let directory = std::env::temp_dir().join(format!(
            "clipboard-board-startup-cas-conflict-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let worker = SettingsWorker::start_at(&directory).unwrap();
        let mut fake = FakeRegistry::new(None);
        fake.external_before_mutation = Some(Some(registry_value(OsStr::new(r"C:\other.exe"))));
        let mut owner = StartupSettingsOwner::start_with_backend(
            worker.client(),
            Box::new(fake),
            OsString::from(r"C:\ClipboardBoard.exe"),
        )
        .unwrap();
        let sender = owner.sender();
        let result = sender.try_enable(0).unwrap().recv().unwrap();
        assert_eq!(result.kind, StartupResultKind::Conflict);
        assert_eq!(result.error, Some(StartupErrorCategory::ForeignConflict));
        let second = sender.try_disable(0).unwrap().recv().unwrap();
        assert_eq!(second.kind, StartupResultKind::Conflict);
        owner.begin_closing().unwrap();
        owner.finish_shutdown().unwrap();
        let mut worker = worker;
        worker.begin_closing().unwrap();
        worker.finish_shutdown().unwrap();
        let _ = std::fs::remove_dir_all(directory);
    }

    /// 权限拒绝只返回 PendingRetry，不占用未知对账槽；读回未知才进入对账门禁。
    #[test]
    fn owner_permission_and_readback_unknown_routes() {
        let permission_directory = std::env::temp_dir().join(format!(
            "clipboard-board-startup-permission-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&permission_directory);
        let permission_worker = SettingsWorker::start_at(&permission_directory).unwrap();
        let mut permission_fake = FakeRegistry::new(None);
        permission_fake.read_permission_denied = true;
        let mut permission_owner = StartupSettingsOwner::start_with_backend(
            permission_worker.client(),
            Box::new(permission_fake),
            OsString::from(r"C:\ClipboardBoard.exe"),
        )
        .unwrap();
        let permission_result = permission_owner
            .sender()
            .try_enable(0)
            .unwrap()
            .recv()
            .unwrap();
        assert_eq!(permission_result.kind, StartupResultKind::PendingRetry);
        assert_eq!(
            permission_result.error,
            Some(StartupErrorCategory::PermissionDenied)
        );
        permission_owner.begin_closing().unwrap();
        permission_owner.finish_shutdown().unwrap();
        let mut permission_worker = permission_worker;
        permission_worker.begin_closing().unwrap();
        permission_worker.finish_shutdown().unwrap();

        let invalid_directory = std::env::temp_dir().join(format!(
            "clipboard-board-startup-invalid-registry-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&invalid_directory);
        let invalid_worker = SettingsWorker::start_at(&invalid_directory).unwrap();
        let mut invalid_fake = FakeRegistry::new(None);
        invalid_fake.read_invalid_data = true;
        let mut invalid_owner = StartupSettingsOwner::start_with_backend(
            invalid_worker.client(),
            Box::new(invalid_fake),
            OsString::from(r"C:\ClipboardBoard.exe"),
        )
        .unwrap();
        let invalid_result = invalid_owner
            .sender()
            .try_enable(0)
            .unwrap()
            .recv()
            .unwrap();
        assert_eq!(invalid_result.kind, StartupResultKind::Conflict);
        assert_eq!(
            invalid_result.error,
            Some(StartupErrorCategory::InvalidInput)
        );
        invalid_owner.begin_closing().unwrap();
        invalid_owner.finish_shutdown().unwrap();
        let mut invalid_worker = invalid_worker;
        invalid_worker.begin_closing().unwrap();
        invalid_worker.finish_shutdown().unwrap();

        let readback_directory = std::env::temp_dir().join(format!(
            "clipboard-board-startup-readback-unknown-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&readback_directory);
        let readback_worker = SettingsWorker::start_at(&readback_directory).unwrap();
        let mut readback_fake = FakeRegistry::new(None);
        readback_fake.unknown_after_mutation = true;
        let mut readback_owner = StartupSettingsOwner::start_with_backend(
            readback_worker.client(),
            Box::new(readback_fake),
            OsString::from(r"C:\ClipboardBoard.exe"),
        )
        .unwrap();
        let readback_result = readback_owner
            .sender()
            .try_enable(0)
            .unwrap()
            .recv()
            .unwrap();
        assert_eq!(readback_result.kind, StartupResultKind::SaveFailed);
        assert_eq!(
            readback_result.error,
            Some(StartupErrorCategory::OutcomeUnknown)
        );
        readback_owner.begin_closing().unwrap();
        readback_owner.finish_shutdown().unwrap();
        let mut readback_worker = readback_worker;
        readback_worker.begin_closing().unwrap();
        readback_worker.finish_shutdown().unwrap();
        let _ = std::fs::remove_dir_all(permission_directory);
        let _ = std::fs::remove_dir_all(invalid_directory);
        let _ = std::fs::remove_dir_all(readback_directory);
    }

    /// Settings OutcomeUnknown 和补偿失败必须走稳定的实际路由，不伪造成功。
    #[test]
    fn settings_unknown_and_compensation_failure_routes() {
        let directory = std::env::temp_dir().join(format!(
            "clipboard-board-startup-compensation-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let worker = SettingsWorker::start_at(&directory).unwrap();
        let client = worker.client();
        let old_settings = client.snapshot().unwrap();
        let written = Some(registry_value(OsStr::new(r"C:\ClipboardBoard.exe")));
        let state = TransactionState {
            intent: StartupIntent::Enable,
            old_settings,
            old_registry: None,
            written_registry: written.clone(),
        };
        let mut pending = None;
        let mut backend = FakeRegistry::new(written.clone());
        let result = compensate_after_settings_failure(
            &client,
            &mut backend,
            NonZeroU64::new(1).unwrap(),
            1,
            &mut pending,
            state,
            StartupErrorCategory::OutcomeUnknown,
        );
        assert_eq!(result.kind, StartupResultKind::SaveFailed);
        assert_eq!(result.error, Some(StartupErrorCategory::OutcomeUnknown));

        let old_settings = client.snapshot().unwrap();
        let state = TransactionState {
            intent: StartupIntent::Enable,
            old_settings,
            old_registry: None,
            written_registry: written.clone(),
        };
        let mut failing_backend = FakeRegistry::new(written);
        failing_backend.mutation_permission_denied = true;
        let result = compensate_known_settings_failure(
            &client,
            &mut failing_backend,
            NonZeroU64::new(2).unwrap(),
            1,
            &mut pending,
            state,
            StartupErrorCategory::SettingsConflict,
        );
        assert_eq!(result.kind, StartupResultKind::ReconcileRequired);
        assert_eq!(result.error, Some(StartupErrorCategory::CompensationFailed));
        let mut worker = worker;
        worker.begin_closing().unwrap();
        worker.finish_shutdown().unwrap();
        let _ = std::fs::remove_dir_all(directory);
    }

    /// OutcomeUnknown 对账覆盖“配置已提交/仍为旧值”两条安全路径，避免误补偿。
    #[test]
    fn unknown_outcome_classifier_distinguishes_commit_and_rollback() {
        let old_settings =
            SettingsSnapshot::new(AppSettings::default(), SettingsLoadSource::Defaults, 0);
        let mut committed = old_settings.settings().clone();
        committed.startup.run_on_login = true;
        let committed_settings = SettingsSnapshot::new(committed, SettingsLoadSource::Primary, 1);
        let old_registry = None;
        let written_registry = Some(registry_value(OsStr::new(r"C:\ClipboardBoard.exe")));
        let state = TransactionState {
            intent: StartupIntent::Enable,
            old_settings: old_settings.clone(),
            old_registry: old_registry.clone(),
            written_registry: written_registry.clone(),
        };
        assert_eq!(
            classify_unknown_reconciliation(&state, &committed_settings, &written_registry),
            UnknownReconciliation::Applied
        );
        assert_eq!(
            classify_unknown_reconciliation(&state, &old_settings, &written_registry),
            UnknownReconciliation::Compensate
        );
        assert_eq!(
            classify_unknown_reconciliation(&state, &old_settings, &old_registry),
            UnknownReconciliation::SaveFailed
        );
    }

    /// startup revision 冲突必须对 Enable/Disable 对称，非 startup 字段变化则可合并。
    #[test]
    fn startup_revision_conflict_is_symmetric_and_field_scoped() {
        let old_disabled =
            SettingsSnapshot::new(AppSettings::default(), SettingsLoadSource::Defaults, 0);
        let mut enabled_settings = old_disabled.settings().clone();
        enabled_settings.startup.run_on_login = true;
        let latest_enabled =
            SettingsSnapshot::new(enabled_settings, SettingsLoadSource::Primary, 1);
        assert!(startup_revision_conflicts(&old_disabled, &latest_enabled));

        let old_enabled = latest_enabled.clone();
        assert!(startup_revision_conflicts(&old_enabled, &old_disabled));

        let mut history_only = old_disabled.settings().clone();
        history_only.history.max_items += 1;
        let latest_history_only =
            SettingsSnapshot::new(history_only, SettingsLoadSource::Primary, 1);
        assert!(!startup_revision_conflicts(
            &old_disabled,
            &latest_history_only
        ));
    }

    /// owner 在途时不再接收第二个变更命令，调用方得到明确 Busy 回执。
    #[test]
    fn sender_reports_busy_for_in_flight_command() {
        let (sender, _receiver) = sync_channel(OWNER_QUEUE_CAPACITY);
        let command_sender = StartupCommandSender {
            sender,
            lifecycle: Arc::new(StartupLifecycle {
                closing_intent: AtomicBool::new(false),
                state: Mutex::new(StartupLifecycleState::Open),
            }),
            next_transaction: Arc::new(AtomicU64::new(1)),
            generation: Arc::new(AtomicU64::new(1)),
            in_flight: Arc::new(AtomicBool::new(true)),
        };
        let result = command_sender.try_enable(0).unwrap().recv().unwrap();
        assert_eq!(result.kind, StartupResultKind::Busy);
    }

    /// 已入队的旧 generation 在消费前被 tombstone 淘汰，只回 Stale 且不触碰资源。
    #[test]
    fn owner_drops_stale_generation_with_receipt() {
        let directory = std::env::temp_dir().join(format!(
            "clipboard-board-startup-stale-generation-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let worker = SettingsWorker::start_at(&directory).unwrap();
        let settings_observer = worker.client();
        let settings_for_owner = worker.client();
        let (command_sender, command_receiver) = sync_channel(OWNER_QUEUE_CAPACITY);
        let lifecycle = Arc::new(StartupLifecycle {
            closing_intent: AtomicBool::new(false),
            state: Mutex::new(StartupLifecycleState::Open),
        });
        let generation = Arc::new(AtomicU64::new(2));
        let in_flight = Arc::new(AtomicBool::new(true));
        let (stale_reply, stale_result) = sync_channel(1);
        command_sender
            .send(StartupCommand {
                kind: StartupCommandKind::Enable,
                transaction_id: NonZeroU64::new(1).unwrap(),
                generation: 1,
                expected_revision: 0,
                reply: stale_reply,
            })
            .unwrap();
        let owner_thread = thread::spawn({
            let lifecycle = Arc::clone(&lifecycle);
            let generation = Arc::clone(&generation);
            let in_flight = Arc::clone(&in_flight);
            move || {
                owner_main(
                    settings_for_owner,
                    Box::new(FakeRegistry::new(None)),
                    OsString::from(r"C:\ClipboardBoard.exe"),
                    command_receiver,
                    lifecycle,
                    generation,
                    in_flight,
                )
            }
        });
        let stale = stale_result.recv().unwrap();
        assert_eq!(stale.kind, StartupResultKind::Stale);
        assert!(
            !settings_observer
                .snapshot()
                .unwrap()
                .settings()
                .startup
                .run_on_login
        );

        let (shutdown_reply, shutdown_result) = sync_channel(1);
        command_sender
            .send(StartupCommand {
                kind: StartupCommandKind::Shutdown,
                transaction_id: NonZeroU64::new(2).unwrap(),
                generation: 2,
                expected_revision: 0,
                reply: shutdown_reply,
            })
            .unwrap();
        assert_eq!(
            shutdown_result.recv().unwrap().kind,
            StartupResultKind::Stopped
        );
        owner_thread.join().unwrap();
        let mut worker = worker;
        worker.begin_closing().unwrap();
        worker.finish_shutdown().unwrap();
        let _ = std::fs::remove_dir_all(directory);
    }

    /// pending_reconcile 存在时拒绝新的 Enable/Disable，但允许 Retry 完成对账。
    #[test]
    fn owner_blocks_new_mutations_until_retry() {
        let directory = std::env::temp_dir().join(format!(
            "clipboard-board-startup-reconcile-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let worker = SettingsWorker::start_at(&directory).unwrap();
        let settings_observer = worker.client();
        let mut fake = FakeRegistry::new(None);
        fake.unknown_after_mutation_reads = 2;
        let mut owner = StartupSettingsOwner::start_with_backend(
            worker.client(),
            Box::new(fake),
            OsString::from(r"C:\ClipboardBoard.exe"),
        )
        .unwrap();
        let sender = owner.sender();
        let first = sender.try_enable(0).unwrap().recv().unwrap();
        assert_eq!(first.kind, StartupResultKind::ReconcileRequired);
        let blocked = sender.try_disable(0).unwrap().recv().unwrap();
        assert_eq!(blocked.kind, StartupResultKind::ReconcileRequired);

        // pending 期间只修改无关字段会推进 revision，但不得阻塞 Retry。
        let unrelated_snapshot = settings_observer.snapshot().unwrap();
        let mut unrelated_settings = unrelated_snapshot.settings().clone();
        unrelated_settings.history.max_items += 1;
        settings_observer
            .save(unrelated_snapshot.revision(), unrelated_settings)
            .unwrap();
        let retried = sender.try_retry().unwrap().recv().unwrap();
        assert_eq!(retried.kind, StartupResultKind::Applied);
        owner.begin_closing().unwrap();
        owner.finish_shutdown().unwrap();
        let mut worker = worker;
        worker.begin_closing().unwrap();
        worker.finish_shutdown().unwrap();
        let _ = std::fs::remove_dir_all(directory);
    }

    /// pending 期间若 startup 字段被外部改写，Retry 必须返回配置冲突且保留对账门禁。
    #[test]
    fn retry_rejects_pending_startup_change() {
        let directory = std::env::temp_dir().join(format!(
            "clipboard-board-startup-retry-startup-conflict-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let worker = SettingsWorker::start_at(&directory).unwrap();
        let settings_observer = worker.client();
        let mut fake = FakeRegistry::new(None);
        fake.unknown_after_mutation_reads = 2;
        let mut owner = StartupSettingsOwner::start_with_backend(
            worker.client(),
            Box::new(fake),
            OsString::from(r"C:\ClipboardBoard.exe"),
        )
        .unwrap();
        let sender = owner.sender();
        let first = sender.try_enable(0).unwrap().recv().unwrap();
        assert_eq!(first.kind, StartupResultKind::ReconcileRequired);

        // 外部事务改变 startup 期望后，Retry 不得覆盖其选择。
        let snapshot = settings_observer.snapshot().unwrap();
        let mut changed = snapshot.settings().clone();
        changed.startup.run_on_login = true;
        settings_observer
            .save(snapshot.revision(), changed)
            .unwrap();
        let retry = sender.try_retry().unwrap().recv().unwrap();
        assert_eq!(retry.kind, StartupResultKind::Conflict);
        assert_eq!(retry.error, Some(StartupErrorCategory::SettingsConflict));
        let blocked = sender.try_disable(0).unwrap().recv().unwrap();
        assert_eq!(blocked.kind, StartupResultKind::ReconcileRequired);

        owner.begin_closing().unwrap();
        assert_eq!(
            owner.finish_shutdown().unwrap_err().category,
            StartupErrorCategory::OutcomeUnknown
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    /// Retry 再次遇到冲突时必须恢复对账槽，后续变更仍被拒绝。
    #[test]
    fn retry_conflict_keeps_reconcile_gate() {
        let directory = std::env::temp_dir().join(format!(
            "clipboard-board-startup-retry-conflict-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let worker = SettingsWorker::start_at(&directory).unwrap();
        let foreign = registry_value(OsStr::new(r"C:\other.exe"));
        let mut fake = FakeRegistry::new(None);
        // 第一次 mutation 先被外部改值再返回未知；Retry 读取到 foreign 并返回 Conflict。
        fake.external_before_mutation = Some(Some(foreign));
        fake.mutation_unknown = true;
        let mut owner = StartupSettingsOwner::start_with_backend(
            worker.client(),
            Box::new(fake),
            OsString::from(r"C:\ClipboardBoard.exe"),
        )
        .unwrap();
        let sender = owner.sender();
        let first = sender.try_enable(0).unwrap().recv().unwrap();
        assert_eq!(first.kind, StartupResultKind::ReconcileRequired);
        let retry = sender.try_retry().unwrap().recv().unwrap();
        assert_eq!(retry.kind, StartupResultKind::Conflict);
        let blocked = sender.try_disable(0).unwrap().recv().unwrap();
        assert_eq!(blocked.kind, StartupResultKind::ReconcileRequired);
        owner.begin_closing().unwrap();
        assert_eq!(
            owner.finish_shutdown().unwrap_err().category,
            StartupErrorCategory::OutcomeUnknown
        );
        let _ = std::fs::remove_dir_all(directory);
    }
}

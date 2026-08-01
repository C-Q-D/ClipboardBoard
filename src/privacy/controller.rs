//! 此文件实现暂停命令槽、配置 RPC 端口、异步 helper 与运行时所有者。

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::pause::{restore_pause, GateMode, PauseClock, PauseTimeError, RecordingGate};
use crate::settings::{
    AppSettings, RecordingPause, SettingsClient, SettingsError, SettingsSnapshot, SettingsWorker,
};

/// 用户可请求的四类记录状态变化。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PauseCommand {
    /// 暂停五分钟。
    PauseFiveMinutes,
    /// 暂停三十分钟。
    PauseThirtyMinutes,
    /// 无限暂停。
    PauseIndefinitely,
    /// 恢复记录。
    Resume,
}

/// 托盘可只读观察的轻量状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PauseStatus {
    /// 正常记录。
    Active = 0,
    /// 定时暂停。
    PausedTimed = 1,
    /// 无限暂停。
    PausedIndefinite = 2,
    /// 正在排空 reader 或等待配置结果。
    Updating = 3,
    /// 保存结果未知且无法取得权威快照。
    Reconciling = 4,
}

/// 配置 RPC 的最小可注入端口。
pub trait SettingsRpcPort: Send + 'static {
    /// 取得当前权威配置快照。
    fn snapshot(&mut self) -> Result<SettingsSnapshot, SettingsError>;

    /// 以 revision compare-and-save 保存完整拥有型设置。
    fn save(
        &mut self,
        expected_revision: u64,
        settings: AppSettings,
    ) -> Result<SettingsSnapshot, SettingsError>;
}

/// 生产适配器只负责一对一委托 SettingsClient。
pub struct SettingsClientRpcAdapter {
    /// 同步配置客户端只存在于 RPC helper 线程。
    client: SettingsClient,
}

impl SettingsClientRpcAdapter {
    /// 包装生产配置客户端。
    pub fn new(client: SettingsClient) -> Self {
        Self { client }
    }
}

impl SettingsRpcPort for SettingsClientRpcAdapter {
    fn snapshot(&mut self) -> Result<SettingsSnapshot, SettingsError> {
        self.client.snapshot()
    }

    fn save(
        &mut self,
        expected_revision: u64,
        settings: AppSettings,
    ) -> Result<SettingsSnapshot, SettingsError> {
        self.client.save(expected_revision, settings)
    }
}

/// 全局容量一 latest-wins 命令槽。
struct CommandSlot {
    /// 尚未开始的唯一最新命令与关闭状态必须在同一锁内线性化。
    state: Mutex<CommandSlotState>,
    /// 唤醒 controller 的容量一令牌。
    wake: SyncSender<ControllerEvent>,
}

/// 暂停命令槽的可变状态；统一锁避免提交与关闭之间出现竞态窗口。
struct CommandSlotState {
    /// 尚未开始的唯一最新命令。
    pending: Option<PauseCommand>,
    /// 关闭后永久拒绝新命令。
    closed: bool,
}

impl CommandSlot {
    /// 原子替换最新命令；返回值只说明命令已进入槽位。
    fn try_submit(&self, command: PauseCommand) -> Result<(), PauseControllerError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| PauseControllerError::Closed)?;
        if state.closed {
            return Err(PauseControllerError::Closed);
        }
        state.pending = Some(command);
        drop(state);
        let _ = self.wake.try_send(ControllerEvent::Wake);
        Ok(())
    }

    /// 关闭槽并丢弃尚未开始的命令；关闭与提交共享同一线性化锁。
    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.pending = None;
        }
        let _ = self.wake.try_send(ControllerEvent::Wake);
    }

    /// 返回关闭快照；锁失效时保守视为已关闭。
    fn is_closed(&self) -> bool {
        self.state.lock().map(|state| state.closed).unwrap_or(true)
    }

    /// 取出尚未开始的最新命令；关闭后不再取出。
    fn take_pending(&self) -> Option<PauseCommand> {
        self.state
            .lock()
            .ok()
            .and_then(|mut state| state.pending.take())
    }
}

/// 可克隆非阻塞暂停命令入口。
#[derive(Clone)]
pub struct PauseCommandSender {
    /// 全局共享命令槽。
    slot: Arc<CommandSlot>,
    /// 只读状态。
    status: Arc<AtomicU8>,
}

impl PauseCommandSender {
    /// 创建永久关闭的 Active 占位入口，供不启用配置运行时的底层测试使用。
    pub fn disabled() -> Self {
        let (wake, _receiver) = mpsc::sync_channel(1);
        Self {
            slot: Arc::new(CommandSlot {
                state: Mutex::new(CommandSlotState {
                    pending: None,
                    closed: true,
                }),
                wake,
            }),
            status: Arc::new(AtomicU8::new(PauseStatus::Active as u8)),
        }
    }

    /// 原子替换尚未开始的任意旧命令；成功仅表示接收。
    pub fn try_submit(&self, command: PauseCommand) -> Result<(), PauseControllerError> {
        self.slot.try_submit(command)
    }

    /// 返回菜单可读取的无正文状态。
    pub fn status(&self) -> PauseStatus {
        decode_status(self.status.load(Ordering::Acquire))
    }
}

/// 暂停运行时有限错误。
#[derive(Debug)]
pub enum PauseControllerError {
    /// controller 已关闭。
    Closed,
    /// 时钟无法表示目标 deadline。
    Time(PauseTimeError),
    /// 配置 RPC helper 无法启动或已经断开。
    RpcUnavailable,
    /// controller 线程 panic。
    ThreadPanicked,
}

impl std::fmt::Display for PauseControllerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(formatter, "暂停控制器已关闭"),
            Self::Time(_) => write!(formatter, "暂停时间无法安全表示"),
            Self::RpcUnavailable => write!(formatter, "配置 RPC 不可用"),
            Self::ThreadPanicked => write!(formatter, "暂停控制器线程异常退出"),
        }
    }
}

impl std::error::Error for PauseControllerError {}

/// helper 请求。
enum RpcRequest {
    /// 保存目标配置。
    Save {
        /// 事务代次。
        generation: u64,
        /// 期望 revision。
        expected_revision: u64,
        /// 完整设置。
        settings: AppSettings,
    },
    /// 对账当前快照。
    Snapshot {
        /// 事务代次。
        generation: u64,
    },
    /// 停止 helper。
    Stop,
}

/// controller 单一事件通道。
enum ControllerEvent {
    /// 命令槽或关闭动作唤醒。
    Wake,
    /// RPC 保存结果。
    SaveResult {
        generation: u64,
        result: Result<SettingsSnapshot, SettingsError>,
    },
    /// RPC 快照结果。
    SnapshotResult {
        generation: u64,
        result: Result<SettingsSnapshot, SettingsError>,
    },
}

/// 串行配置 RPC helper。
struct SettingsRpcHelper {
    /// 有界请求入口。
    sender: SyncSender<RpcRequest>,
    /// 唯一 join 句柄。
    join: Option<JoinHandle<()>>,
}

impl SettingsRpcHelper {
    /// 启动独占端口的 helper。
    fn start(
        mut port: Box<dyn SettingsRpcPort>,
        events: SyncSender<ControllerEvent>,
    ) -> Result<Self, PauseControllerError> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("clipboard-board-settings-rpc".to_owned())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    match request {
                        RpcRequest::Save {
                            generation,
                            expected_revision,
                            settings,
                        } => {
                            let result = port.save(expected_revision, settings);
                            let _ = events.send(ControllerEvent::SaveResult { generation, result });
                        }
                        RpcRequest::Snapshot { generation } => {
                            let result = port.snapshot();
                            let _ =
                                events.send(ControllerEvent::SnapshotResult { generation, result });
                        }
                        RpcRequest::Stop => break,
                    }
                }
            })
            .map_err(|_| PauseControllerError::RpcUnavailable)?;
        Ok(Self {
            sender,
            join: Some(join),
        })
    }

    /// 停止并回收 helper；已执行的阻塞 RPC 先完成。
    fn stop(&mut self) -> Result<(), PauseControllerError> {
        let _ = self.sender.send(RpcRequest::Stop);
        self.join
            .take()
            .ok_or(PauseControllerError::Closed)?
            .join()
            .map_err(|_| PauseControllerError::ThreadPanicked)
    }
}

/// controller 所有者。
struct PauseController {
    /// 命令共享槽。
    slot: Arc<CommandSlot>,
    /// controller 线程。
    join: Option<JoinHandle<()>>,
}

/// 集中拥有 privacy 链全部资源。
pub struct PrivacyRuntimeOwner {
    /// 配置 worker 必须最后关闭。
    settings: Option<SettingsWorker>,
    /// controller 必须先于 helper停止。
    controller: Option<PauseController>,
    /// helper 必须先于 SettingsWorker停止。
    helper: Option<SettingsRpcHelper>,
    /// ClipboardIO 共享门禁。
    gate: RecordingGate,
    /// 托盘命令与状态入口。
    sender: PauseCommandSender,
}

impl PrivacyRuntimeOwner {
    /// 从显式 SettingsWorker、端口和时钟建立运行时；测试可完全注入。
    pub fn start_with(
        settings: SettingsWorker,
        initial: SettingsSnapshot,
        port: Box<dyn SettingsRpcPort>,
        clock: Arc<dyn PauseClock>,
    ) -> Result<Self, PauseControllerError> {
        let (mode, deadline) =
            restore_pause(&initial.settings().privacy.recording_pause, clock.as_ref())
                .map_err(PauseControllerError::Time)?;
        let gate = RecordingGate::new(mode);
        let status = Arc::new(AtomicU8::new(status_for_pause(
            &initial.settings().privacy.recording_pause,
            mode,
        ) as u8));
        let (event_sender, event_receiver) = mpsc::sync_channel(16);
        let slot = Arc::new(CommandSlot {
            state: Mutex::new(CommandSlotState {
                pending: None,
                closed: false,
            }),
            wake: event_sender.clone(),
        });
        let sender = PauseCommandSender {
            slot: Arc::clone(&slot),
            status: Arc::clone(&status),
        };
        let helper = SettingsRpcHelper::start(port, event_sender)?;
        let rpc_sender = helper.sender.clone();
        let controller_gate = gate.clone();
        let controller_slot = Arc::clone(&slot);
        let join = match thread::Builder::new()
            .name("clipboard-board-pause-controller".to_owned())
            .spawn(move || {
                controller_loop(
                    controller_slot,
                    status,
                    controller_gate,
                    initial,
                    deadline,
                    clock,
                    rpc_sender,
                    event_receiver,
                )
            }) {
            Ok(join) => join,
            Err(_) => {
                // helper 已经拥有独立线程；controller 创建失败时必须立即走唯一的
                // helper stop/join 路径，不能让 JoinHandle 被丢弃后成为脱离线程。
                let mut helper = helper;
                let _ = helper.stop();
                return Err(PauseControllerError::ThreadPanicked);
            }
        };
        Ok(Self {
            settings: Some(settings),
            controller: Some(PauseController {
                slot,
                join: Some(join),
            }),
            helper: Some(helper),
            gate,
            sender,
        })
    }

    /// 返回 ClipboardIO 使用的门禁。
    pub fn gate(&self) -> RecordingGate {
        self.gate.clone()
    }

    /// 返回托盘使用的非阻塞入口。
    pub fn sender(&self) -> PauseCommandSender {
        self.sender.clone()
    }

    /// 严格按 controller、helper、SettingsWorker 顺序关闭且每项一次。
    pub fn stop(mut self) -> Result<(), PauseControllerError> {
        self.close_inner()
    }

    /// 共享显式关闭与 Drop 的唯一逆序收敛实现。
    fn close_inner(&mut self) -> Result<(), PauseControllerError> {
        if let Some(mut controller) = self.controller.take() {
            controller.slot.close();
            controller
                .join
                .take()
                .ok_or(PauseControllerError::Closed)?
                .join()
                .map_err(|_| PauseControllerError::ThreadPanicked)?;
        }
        if let Some(mut helper) = self.helper.take() {
            helper.stop()?;
        }
        if let Some(mut settings) = self.settings.take() {
            settings
                .begin_closing()
                .map_err(|_| PauseControllerError::RpcUnavailable)?;
            settings
                .finish_shutdown()
                .map_err(|_| PauseControllerError::RpcUnavailable)?;
        }
        Ok(())
    }
}

impl Drop for PrivacyRuntimeOwner {
    /// 启动失败或异常展开时也按同一顺序尽力回收每个唯一资源。
    fn drop(&mut self) {
        let _ = self.close_inner();
    }
}

/// 当前配置事务。
struct Transaction {
    /// 单调代次，拒绝迟到 RPC。
    generation: u64,
    /// 目标持久化状态。
    target: RecordingPause,
    /// 最新权威回滚基线。
    baseline: SettingsSnapshot,
    /// 命令创建时计算出的当前进程单调 deadline；保存阻塞期间也不能丢失。
    deadline: Option<Duration>,
    /// 当前事务已经发起的对账尝试次数，避免错误回执导致无界自旋。
    reconciliation_attempts: u8,
    /// deadline 已跨过时晚到 Pause 只能归一化 Active。
    expired: bool,
    /// 当前事务是否仍有一个 Save 或 Snapshot RPC 在 helper 线程中执行。
    ///
    /// deadline 到达而没有在途请求时可以安全丢弃事务，让下一个托盘命令继续处理；
    /// 只有存在在途请求时才需要保留事务并把迟到回执归一化为 Active。
    in_flight: bool,
}

/// 单个暂停事务允许的有限对账次数；超过后保持 fail-closed 并等待新命令。
const MAX_RECONCILIATION_ATTEMPTS: u8 = 3;

#[allow(clippy::too_many_arguments)]
fn controller_loop(
    slot: Arc<CommandSlot>,
    status: Arc<AtomicU8>,
    gate: RecordingGate,
    mut snapshot: SettingsSnapshot,
    mut deadline: Option<Duration>,
    clock: Arc<dyn PauseClock>,
    rpc: SyncSender<RpcRequest>,
    events: Receiver<ControllerEvent>,
) {
    let mut generation = 0_u64;
    let mut transaction: Option<Transaction> = None;
    loop {
        if slot.is_closed() {
            break;
        }
        if deadline.is_some_and(|value| clock.monotonic_now() >= value) {
            deadline = None;
            gate.begin_update().finish(GateMode::Active);
            status.store(PauseStatus::Active as u8, Ordering::Release);
            let has_transaction = transaction.is_some();
            let can_drop_completed_transaction =
                transaction.as_ref().is_some_and(|active| !active.in_flight);
            if let Some(active) = transaction.as_mut() {
                active.expired = true;
            }
            if has_transaction {
                if can_drop_completed_transaction {
                    // 保存结果已经返回，deadline 只属于这条旧命令；释放事务槽后本轮
                    // 继续读取最新命令，避免一个无在途请求的事务永久阻塞 controller。
                    transaction = None;
                }
            } else {
                generation = generation.wrapping_add(1);
                let target = RecordingPause::Active;
                let mut settings = snapshot.settings().clone();
                settings.privacy.recording_pause = target.clone();
                let request_sent = rpc
                    .send(RpcRequest::Save {
                        generation,
                        expected_revision: snapshot.revision(),
                        settings,
                    })
                    .is_ok();
                if request_sent {
                    transaction = Some(Transaction {
                        generation,
                        target,
                        baseline: snapshot.clone(),
                        deadline: None,
                        reconciliation_attempts: 0,
                        expired: true,
                        in_flight: true,
                    });
                }
            }
        }
        if transaction.is_none() {
            let command = slot.take_pending();
            if let Some(command) = command {
                generation = generation.wrapping_add(1);
                let (target, command_deadline) = match command_to_pause(command, clock.as_ref()) {
                    Ok(target) => target,
                    Err(_) => continue,
                };
                // 记录命令开始前的可观察状态；Reconciling 表示权威配置不可得，失败时
                // 绝不能用旧 snapshot 的 Active 值重新打开门禁。
                let previous_status = decode_status(status.load(Ordering::Acquire));
                let previous_gate = gate.mode();
                status.store(PauseStatus::Updating as u8, Ordering::Release);
                let update = gate.begin_update();
                let mut settings = snapshot.settings().clone();
                settings.privacy.recording_pause = target.clone();
                // 在取得 gate 更新令牌之前已经固定本次命令的单调截止点。这样即使
                // 排空 reader 或磁盘保存跨过 5/30 分钟，timer 仍按命令创建时刻计时。
                deadline = command_deadline;
                if rpc
                    .send(RpcRequest::Save {
                        generation,
                        expected_revision: snapshot.revision(),
                        settings,
                    })
                    .is_err()
                {
                    if previous_status == PauseStatus::Reconciling {
                        // 对账状态下 helper 已断开或不可用，保持 fail-closed；即使旧
                        // snapshot 仍是 Active，也不能在没有权威确认时放行正文读取。
                        debug_assert_eq!(previous_gate, GateMode::Paused);
                        update.finish(GateMode::Paused);
                        status.store(PauseStatus::Reconciling as u8, Ordering::Release);
                        deadline = None;
                        continue;
                    }
                    update.finish(mode_for_pause(&snapshot.settings().privacy.recording_pause));
                    status.store(
                        status_for_pause(&snapshot.settings().privacy.recording_pause, gate.mode())
                            as u8,
                        Ordering::Release,
                    );
                    deadline = deadline_for_pause(
                        &snapshot.settings().privacy.recording_pause,
                        clock.as_ref(),
                    )
                    .ok()
                    .flatten();
                    continue;
                }
                // 更新期间始终 fail-closed；事务结果再明确提交或回滚。
                update.finish(GateMode::Paused);
                transaction = Some(Transaction {
                    generation,
                    target,
                    baseline: snapshot.clone(),
                    deadline: command_deadline,
                    reconciliation_attempts: 0,
                    expired: false,
                    in_flight: true,
                });
            }
        }

        let timeout = deadline
            .map(|value| value.saturating_sub(clock.monotonic_now()))
            .unwrap_or(Duration::from_secs(60));
        let event = events.recv_timeout(timeout.min(Duration::from_secs(60)));
        match event {
            Ok(ControllerEvent::Wake) => {}
            Ok(ControllerEvent::SaveResult {
                generation: result_generation,
                result,
            }) => {
                expire_transaction_if_due(
                    &mut transaction,
                    &mut deadline,
                    clock.as_ref(),
                    &gate,
                    &status,
                );
                let Some(active) = transaction.as_mut() else {
                    continue;
                };
                if result_generation != active.generation {
                    continue;
                }
                // 只有同一代次的回执才能结束当前请求；迟到的旧代次不能改写在途状态。
                active.in_flight = false;
                match result {
                    Ok(committed) => {
                        snapshot = committed;
                        // 有限暂停已经按单调时钟到期时，运行时 Active 是不可回退的边界。
                        // 旧的 Pause 回执即使返回 Active，也只能结束这次事务；不能按旧
                        // target 再次关闭门禁。配置若仍为 Pause，下面继续排队 Active 归一化。
                        if active.expired
                            && snapshot.settings().privacy.recording_pause == RecordingPause::Active
                        {
                            gate.begin_update().finish(GateMode::Active);
                            status.store(PauseStatus::Active as u8, Ordering::Release);
                            deadline = None;
                            transaction = None;
                            continue;
                        }
                        if active.expired
                            && snapshot.settings().privacy.recording_pause != RecordingPause::Active
                        {
                            active.target = RecordingPause::Active;
                            active.baseline = snapshot.clone();
                            active.deadline = None;
                            let mut settings = snapshot.settings().clone();
                            settings.privacy.recording_pause = RecordingPause::Active;
                            let request_sent = rpc
                                .send(RpcRequest::Save {
                                    generation: active.generation,
                                    expected_revision: snapshot.revision(),
                                    settings,
                                })
                                .is_ok();
                            active.in_flight = request_sent;
                            if !request_sent {
                                finalize_rpc_send_failure(active, &gate, &status, &mut deadline);
                                transaction = None;
                            }
                            continue;
                        }
                        let mode = mode_for_pause(&active.target);
                        gate.begin_update().finish(mode);
                        deadline = active.deadline;
                        status.store(
                            status_for_pause(&active.target, mode) as u8,
                            Ordering::Release,
                        );
                        transaction = None;
                    }
                    Err(SettingsError::RevisionConflict { .. })
                    | Err(SettingsError::OutcomeUnknown) => {
                        if active.reconciliation_attempts < MAX_RECONCILIATION_ATTEMPTS {
                            active.reconciliation_attempts += 1;
                            let request_sent = rpc
                                .send(RpcRequest::Snapshot {
                                    generation: active.generation,
                                })
                                .is_ok();
                            active.in_flight = request_sent;
                            if !request_sent {
                                finalize_rpc_send_failure(active, &gate, &status, &mut deadline);
                                transaction = None;
                            }
                        } else {
                            status.store(PauseStatus::Reconciling as u8, Ordering::Release);
                            deadline = None;
                            transaction = None;
                        }
                    }
                    Err(_) => {
                        if active.expired {
                            // 归一化保存失败不能把已经到期的有限暂停重新变成暂停。
                            gate.begin_update().finish(GateMode::Active);
                            status.store(PauseStatus::Active as u8, Ordering::Release);
                            deadline = None;
                            transaction = None;
                            continue;
                        }
                        snapshot = active.baseline.clone();
                        let baseline = snapshot.settings().privacy.recording_pause.clone();
                        let mode = mode_for_pause(&baseline);
                        gate.begin_update().finish(mode);
                        deadline = deadline_for_pause(&baseline, clock.as_ref()).ok().flatten();
                        status.store(status_for_pause(&baseline, mode) as u8, Ordering::Release);
                        transaction = None;
                    }
                }
            }
            Ok(ControllerEvent::SnapshotResult {
                generation: result_generation,
                result,
            }) => {
                expire_transaction_if_due(
                    &mut transaction,
                    &mut deadline,
                    clock.as_ref(),
                    &gate,
                    &status,
                );
                let Some(active) = transaction.as_mut() else {
                    continue;
                };
                if result_generation != active.generation {
                    continue;
                }
                // 只有同一代次的回执才能结束当前请求；迟到的旧代次不能改写在途状态。
                active.in_flight = false;
                match result {
                    Ok(authoritative) => {
                        snapshot = authoritative.clone();
                        active.baseline = authoritative;
                        if active.expired {
                            if snapshot.settings().privacy.recording_pause == RecordingPause::Active
                            {
                                gate.begin_update().finish(GateMode::Active);
                                status.store(PauseStatus::Active as u8, Ordering::Release);
                                deadline = None;
                                transaction = None;
                                continue;
                            }
                            // 权威配置仍是旧 Pause：仅发起 Active 归一化，门禁保持打开。
                            active.target = RecordingPause::Active;
                            let mut settings = snapshot.settings().clone();
                            settings.privacy.recording_pause = RecordingPause::Active;
                            let request_sent = rpc
                                .send(RpcRequest::Save {
                                    generation: active.generation,
                                    expected_revision: snapshot.revision(),
                                    settings,
                                })
                                .is_ok();
                            active.in_flight = request_sent;
                            if !request_sent {
                                finalize_rpc_send_failure(active, &gate, &status, &mut deadline);
                                transaction = None;
                            }
                            continue;
                        }
                        if snapshot.settings().privacy.recording_pause == active.target {
                            let mode = mode_for_pause(&active.target);
                            gate.begin_update().finish(mode);
                            deadline = active.deadline;
                            status.store(
                                status_for_pause(&active.target, mode) as u8,
                                Ordering::Release,
                            );
                            transaction = None;
                        } else if active.reconciliation_attempts >= MAX_RECONCILIATION_ATTEMPTS {
                            // 每次成功快照仍可能接着遇到冲突；预算耗尽时不能再发起
                            // 下一次 save，否则会形成无限 Save→Conflict→Snapshot 循环。
                            status.store(PauseStatus::Reconciling as u8, Ordering::Release);
                            deadline = None;
                            transaction = None;
                        } else {
                            let mut settings = snapshot.settings().clone();
                            settings.privacy.recording_pause = active.target.clone();
                            let request_sent = rpc
                                .send(RpcRequest::Save {
                                    generation: active.generation,
                                    expected_revision: snapshot.revision(),
                                    settings,
                                })
                                .is_ok();
                            active.in_flight = request_sent;
                            if !request_sent {
                                finalize_rpc_send_failure(active, &gate, &status, &mut deadline);
                                transaction = None;
                            }
                        }
                    }
                    Err(_) => {
                        // 到期后的 Active 是运行时硬边界；即使归一化对账失败，也不能
                        // 因为 fail-closed 的普通分支再次关闭有限暂停门禁。
                        gate.begin_update().finish(if active.expired {
                            GateMode::Active
                        } else {
                            GateMode::Paused
                        });
                        status.store(PauseStatus::Reconciling as u8, Ordering::Release);
                        if active.reconciliation_attempts < MAX_RECONCILIATION_ATTEMPTS {
                            active.reconciliation_attempts += 1;
                            let request_sent = rpc
                                .send(RpcRequest::Snapshot {
                                    generation: active.generation,
                                })
                                .is_ok();
                            active.in_flight = request_sent;
                            if !request_sent {
                                finalize_rpc_send_failure(active, &gate, &status, &mut deadline);
                                transaction = None;
                            }
                        } else {
                            // 已达到有界重试上限，释放事务槽让新的托盘命令可以重新发起
                            // 对账；门禁仍按到期语义或暂停语义保持关闭，不猜测 Active。
                            deadline = None;
                            transaction = None;
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// 后续 RPC 无法入队时收敛事务，避免无限暂停在 Updating 中永久占槽。
///
/// helper 通道断开意味着当前配置结果不可知；此时保留 fail-closed 门禁，并把事务
/// 置为 Reconciling。调用方随后清除事务，使新的托盘命令仍能进入同一 controller。
fn finalize_rpc_send_failure(
    active: &Transaction,
    gate: &RecordingGate,
    status: &Arc<AtomicU8>,
    deadline: &mut Option<Duration>,
) {
    gate.begin_update().finish(if active.expired {
        GateMode::Active
    } else {
        GateMode::Paused
    });
    status.store(PauseStatus::Reconciling as u8, Ordering::Release);
    *deadline = None;
}

/// 在处理迟到 RPC 事件前再次以当前单调时钟确认事务是否到期。
///
/// 事件可能恰好在 `recv_timeout` 返回后到达；此时 loop 顶部尚未再次检查 timer，不能
/// 只依赖 `active.expired` 标记，否则旧 Pause 回执会短暂甚至永久重新关闭读取门禁。
fn expire_transaction_if_due(
    transaction: &mut Option<Transaction>,
    deadline: &mut Option<Duration>,
    clock: &dyn PauseClock,
    gate: &RecordingGate,
    status: &Arc<AtomicU8>,
) {
    let should_drop = {
        let Some(active) = transaction.as_mut() else {
            return;
        };
        let Some(transaction_deadline) = active.deadline else {
            return;
        };
        if active.expired || clock.monotonic_now() < transaction_deadline {
            return;
        }
        active.expired = true;
        // 没有在途请求时不存在需要等待的迟到回执，可以立即释放事务槽；有在途
        // 请求时则必须保留它，等待同代次结果走 Active 归一化分支。
        !active.in_flight
    };
    *deadline = None;
    gate.begin_update().finish(GateMode::Active);
    status.store(PauseStatus::Active as u8, Ordering::Release);
    if should_drop {
        *transaction = None;
    }
}

/// 把托盘命令转换成持久化状态。
fn command_to_pause(
    command: PauseCommand,
    clock: &dyn PauseClock,
) -> Result<(RecordingPause, Option<Duration>), PauseTimeError> {
    let wall_now = clock.wall_now_millis()?;
    let monotonic_now = clock.monotonic_now();
    let minutes = match command {
        PauseCommand::PauseFiveMinutes => Some(5_u64),
        PauseCommand::PauseThirtyMinutes => Some(30_u64),
        PauseCommand::PauseIndefinitely => return Ok((RecordingPause::Indefinite, None)),
        PauseCommand::Resume => return Ok((RecordingPause::Active, None)),
    };
    let delta_millis = minutes.unwrap().saturating_mul(60_000);
    let deadline = wall_now
        .checked_add(delta_millis)
        .ok_or(PauseTimeError::DeadlineOverflow)?;
    let monotonic_deadline = monotonic_now
        .checked_add(Duration::from_millis(delta_millis))
        .ok_or(PauseTimeError::DeadlineOverflow)?;
    Ok((
        RecordingPause::UntilUnixMillis(deadline),
        Some(monotonic_deadline),
    ))
}

/// 从持久化状态取得运行时门禁模式。
fn mode_for_pause(pause: &RecordingPause) -> GateMode {
    if matches!(pause, RecordingPause::Active) {
        GateMode::Active
    } else {
        GateMode::Paused
    }
}

/// 为已提交 timed pause 重建当前进程 deadline。
fn deadline_for_pause(
    pause: &RecordingPause,
    clock: &dyn PauseClock,
) -> Result<Option<Duration>, PauseTimeError> {
    restore_pause(pause, clock).map(|(_, deadline)| deadline)
}

/// 把配置状态映射为菜单状态。
fn status_for_pause(pause: &RecordingPause, mode: GateMode) -> PauseStatus {
    if mode == GateMode::Active {
        PauseStatus::Active
    } else {
        match pause {
            RecordingPause::UntilUnixMillis(_) => PauseStatus::PausedTimed,
            RecordingPause::Indefinite => PauseStatus::PausedIndefinite,
            RecordingPause::Active => PauseStatus::Updating,
        }
    }
}

/// 解码原子状态；未知值保守视为 Reconciling。
fn decode_status(value: u8) -> PauseStatus {
    match value {
        0 => PauseStatus::Active,
        1 => PauseStatus::PausedTimed,
        2 => PauseStatus::PausedIndefinite,
        3 => PauseStatus::Updating,
        _ => PauseStatus::Reconciling,
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块覆盖暂停 RPC 跨 deadline 和命令关闭竞态，不访问真实剪贴板或托盘。

    use super::*;
    use crate::settings::{AppSettings, SettingsLoadSource};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// 共享假时钟；墙上时间和单调时间可以独立推进。
    struct ManualClock {
        /// UTC Unix epoch 毫秒。
        wall: AtomicU64,
        /// 当前进程单调毫秒。
        monotonic: AtomicU64,
    }

    impl ManualClock {
        /// 创建指定墙上时间、单调时间从零开始的测试时钟。
        fn new(wall: u64) -> Self {
            Self {
                wall: AtomicU64::new(wall),
                monotonic: AtomicU64::new(0),
            }
        }

        /// 推进单调时钟，不改变持久化用墙上时钟。
        fn advance_monotonic(&self, millis: u64) {
            self.monotonic.store(millis, Ordering::Release);
        }
    }

    impl PauseClock for ManualClock {
        fn wall_now_millis(&self) -> Result<u64, PauseTimeError> {
            Ok(self.wall.load(Ordering::Acquire))
        }

        fn monotonic_now(&self) -> Duration {
            Duration::from_millis(self.monotonic.load(Ordering::Acquire))
        }
    }

    /// 阻塞第一次暂停保存，释放后按真实目标返回快照。
    struct BlockingRpc {
        /// RPC 调用和测试控制共用的状态。
        state: Arc<(Mutex<BlockingRpcState>, Condvar)>,
    }

    /// 阻塞 RPC 的可变状态；正文和配置 JSON 都不进入诊断输出。
    struct BlockingRpcState {
        /// 当前权威配置快照。
        snapshot: SettingsSnapshot,
        /// 第一次 save 是否仍应阻塞。
        block_first_save: bool,
        /// 第一次 save 已进入阻塞点。
        entered: bool,
        /// 测试允许第一次 save 返回。
        release: bool,
        /// 到期后 Active 归一化请求已进入 fake RPC。
        normalization_entered: bool,
    }

    impl BlockingRpc {
        /// 用完整默认配置创建阻塞 RPC。
        fn new(snapshot: SettingsSnapshot) -> (Self, Arc<(Mutex<BlockingRpcState>, Condvar)>) {
            let state = Arc::new((
                Mutex::new(BlockingRpcState {
                    snapshot,
                    block_first_save: true,
                    entered: false,
                    release: false,
                    normalization_entered: false,
                }),
                Condvar::new(),
            ));
            (
                Self {
                    state: Arc::clone(&state),
                },
                state,
            )
        }
    }

    impl SettingsRpcPort for BlockingRpc {
        fn snapshot(&mut self) -> Result<SettingsSnapshot, SettingsError> {
            Ok(self.state.0.lock().unwrap().snapshot.clone())
        }

        fn save(
            &mut self,
            _expected_revision: u64,
            settings: AppSettings,
        ) -> Result<SettingsSnapshot, SettingsError> {
            let mut state = self.state.0.lock().unwrap();
            if state.block_first_save {
                state.block_first_save = false;
                state.entered = true;
                self.state.1.notify_all();
                while !state.release {
                    state = self.state.1.wait(state).unwrap();
                }
            }
            if settings.privacy.recording_pause == RecordingPause::Active {
                state.normalization_entered = true;
                self.state.1.notify_all();
            }
            let revision = state.snapshot.revision().saturating_add(1);
            state.snapshot =
                SettingsSnapshot::new(settings, SettingsLoadSource::Defaults, revision);
            Ok(state.snapshot.clone())
        }
    }

    /// 立即成功保存的 fake，用于验证完成事务跨 deadline 后仍能接收新命令。
    struct ImmediateRpc {
        /// fake 持有的最新权威快照。
        snapshot: SettingsSnapshot,
        /// 已处理保存请求数，用于确认到期后新命令确实进入 RPC。
        save_calls: Arc<AtomicUsize>,
    }

    impl SettingsRpcPort for ImmediateRpc {
        fn snapshot(&mut self) -> Result<SettingsSnapshot, SettingsError> {
            Ok(self.snapshot.clone())
        }

        fn save(
            &mut self,
            _expected_revision: u64,
            settings: AppSettings,
        ) -> Result<SettingsSnapshot, SettingsError> {
            self.save_calls.fetch_add(1, Ordering::AcqRel);
            let revision = self.snapshot.revision().saturating_add(1);
            self.snapshot = SettingsSnapshot::new(settings, SettingsLoadSource::Defaults, revision);
            Ok(self.snapshot.clone())
        }
    }

    /// 始终返回未知回执的 fake，用于验证对账请求有界重试且不打开门禁。
    struct AlwaysUnknownRpc {
        /// 已收到的 snapshot 请求数。
        snapshot_calls: Arc<AtomicUsize>,
    }

    impl SettingsRpcPort for AlwaysUnknownRpc {
        fn snapshot(&mut self) -> Result<SettingsSnapshot, SettingsError> {
            self.snapshot_calls.fetch_add(1, Ordering::AcqRel);
            Err(SettingsError::OutcomeUnknown)
        }

        fn save(
            &mut self,
            _expected_revision: u64,
            _settings: AppSettings,
        ) -> Result<SettingsSnapshot, SettingsError> {
            Err(SettingsError::OutcomeUnknown)
        }
    }

    /// 每次保存都制造 revision 冲突、但快照读取成功的 fake。
    struct AlwaysConflictRpc {
        /// 已收到的保存请求数。
        save_calls: Arc<AtomicUsize>,
        /// 已收到的快照请求数。
        snapshot_calls: Arc<AtomicUsize>,
        /// 冲突对账返回的权威 Active 快照。
        snapshot: SettingsSnapshot,
    }

    impl SettingsRpcPort for AlwaysConflictRpc {
        fn snapshot(&mut self) -> Result<SettingsSnapshot, SettingsError> {
            self.snapshot_calls.fetch_add(1, Ordering::AcqRel);
            Ok(self.snapshot.clone())
        }

        fn save(
            &mut self,
            expected_revision: u64,
            _settings: AppSettings,
        ) -> Result<SettingsSnapshot, SettingsError> {
            self.save_calls.fetch_add(1, Ordering::AcqRel);
            Err(SettingsError::RevisionConflict {
                expected: expected_revision,
                actual: expected_revision.saturating_add(1),
            })
        }
    }

    /// 等待阻塞 RPC 进入 save，避免用固定 sleep 猜测线程进度。
    fn wait_rpc_entered(state: &Arc<(Mutex<BlockingRpcState>, Condvar)>) {
        let mut guard = state.0.lock().unwrap();
        for _ in 0..200 {
            if guard.entered {
                return;
            }
            let (next, _) = state
                .1
                .wait_timeout(guard, Duration::from_millis(5))
                .unwrap();
            guard = next;
        }
        panic!("暂停保存未进入阻塞点");
    }

    /// 允许阻塞保存返回，并唤醒 settings RPC helper。
    fn release_rpc(state: &Arc<(Mutex<BlockingRpcState>, Condvar)>) {
        let mut guard = state.0.lock().unwrap();
        guard.release = true;
        state.1.notify_all();
    }

    /// 等待 controller 发出迟到暂停的 Active 归一化请求。
    fn wait_normalization_entered(state: &Arc<(Mutex<BlockingRpcState>, Condvar)>) {
        let mut guard = state.0.lock().unwrap();
        for _ in 0..200 {
            if guard.normalization_entered {
                return;
            }
            let (next, _) = state
                .1
                .wait_timeout(guard, Duration::from_millis(5))
                .unwrap();
            guard = next;
        }
        panic!("到期后的 Active 归一化未进入 fake RPC");
    }

    /// 构造每次运行独立的 SettingsWorker 配置目录。
    fn temporary_directory() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "clipboard-board-atom45-controller-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    /// 暂停保存跨过五分钟单调 deadline 后，迟到 Pause 回执只能保持 Active。
    #[test]
    fn 阻塞暂停保存跨过单调期限仍归一化_active() {
        let directory = temporary_directory();
        let worker = SettingsWorker::start_at(&directory).unwrap();
        let initial =
            SettingsSnapshot::new(AppSettings::default(), SettingsLoadSource::Defaults, 0);
        let (rpc, rpc_state) = BlockingRpc::new(initial.clone());
        let clock = Arc::new(ManualClock::new(0));
        let runtime = PrivacyRuntimeOwner::start_with(
            worker,
            initial,
            Box::new(rpc),
            Arc::clone(&clock) as Arc<dyn PauseClock>,
        )
        .unwrap();
        let sender = runtime.sender();
        sender.try_submit(PauseCommand::PauseFiveMinutes).unwrap();
        wait_rpc_entered(&rpc_state);
        clock.advance_monotonic(300_001);
        release_rpc(&rpc_state);
        // 该等待证明 SaveResult 到达时 controller 直接复核了 deadline，并发起 Active
        // 归一化；若只依赖 loop 顶部标记，第二次 save 不会出现。
        wait_normalization_entered(&rpc_state);

        assert_eq!(runtime.gate().mode(), GateMode::Active);
        assert_eq!(sender.status(), PauseStatus::Active);
        runtime.stop().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    /// 已成功提交的 timed pause 跨过 deadline 后，controller 必须释放无在途事务并处理新命令。
    #[test]
    fn 已完成事务跨过期限后仍可处理新的恢复命令() {
        let directory = temporary_directory();
        let worker = SettingsWorker::start_at(&directory).unwrap();
        let initial =
            SettingsSnapshot::new(AppSettings::default(), SettingsLoadSource::Defaults, 0);
        let save_calls = Arc::new(AtomicUsize::new(0));
        let clock = Arc::new(ManualClock::new(0));
        let runtime = PrivacyRuntimeOwner::start_with(
            worker,
            initial.clone(),
            Box::new(ImmediateRpc {
                snapshot: initial,
                save_calls: Arc::clone(&save_calls),
            }),
            Arc::clone(&clock) as Arc<dyn PauseClock>,
        )
        .unwrap();
        let sender = runtime.sender();
        sender.try_submit(PauseCommand::PauseFiveMinutes).unwrap();

        for _ in 0..200 {
            if sender.status() == PauseStatus::PausedTimed {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(sender.status(), PauseStatus::PausedTimed);
        assert_eq!(save_calls.load(Ordering::Acquire), 1);
        assert_eq!(runtime.gate().mode(), GateMode::Paused);

        // 此时 SaveResult 已完成，事务没有在途 RPC；跨过期限后新 Resume 不能被旧事务卡住。
        clock.advance_monotonic(300_001);
        sender.try_submit(PauseCommand::Resume).unwrap();
        for _ in 0..200 {
            if save_calls.load(Ordering::Acquire) >= 2 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        // 到期时 controller 可能先把持久化 timed pause 归一化为 Active，再处理 Resume，
        // 因而保存次数至少为两次；关键是不应停留在第一次 Pause 保存之后。
        assert!(save_calls.load(Ordering::Acquire) >= 2);
        assert_eq!(sender.status(), PauseStatus::Active);
        assert_eq!(runtime.gate().mode(), GateMode::Active);
        runtime.stop().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    /// 对账快照持续失败时只发起有限次数重试，门禁保持暂停而不猜测 Active。
    #[test]
    fn 对账失败有界重试且门禁保持关闭() {
        let directory = temporary_directory();
        let worker = SettingsWorker::start_at(&directory).unwrap();
        let initial =
            SettingsSnapshot::new(AppSettings::default(), SettingsLoadSource::Defaults, 0);
        let snapshot_calls = Arc::new(AtomicUsize::new(0));
        let runtime = PrivacyRuntimeOwner::start_with(
            worker,
            initial,
            Box::new(AlwaysUnknownRpc {
                snapshot_calls: Arc::clone(&snapshot_calls),
            }),
            Arc::new(ManualClock::new(0)),
        )
        .unwrap();
        runtime
            .sender()
            .try_submit(PauseCommand::PauseIndefinitely)
            .unwrap();

        for _ in 0..200 {
            if snapshot_calls.load(Ordering::Acquire) >= MAX_RECONCILIATION_ATTEMPTS as usize {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            snapshot_calls.load(Ordering::Acquire),
            MAX_RECONCILIATION_ATTEMPTS as usize
        );
        assert_eq!(runtime.gate().mode(), GateMode::Paused);
        assert_eq!(runtime.sender().status(), PauseStatus::Reconciling);
        runtime.stop().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    /// 连续 revision 冲突即使每次快照成功，也只能消耗固定预算，不得无限自旋。
    #[test]
    fn 连续revision冲突受统一预算限制() {
        let directory = temporary_directory();
        let worker = SettingsWorker::start_at(&directory).unwrap();
        let initial =
            SettingsSnapshot::new(AppSettings::default(), SettingsLoadSource::Defaults, 0);
        let save_calls = Arc::new(AtomicUsize::new(0));
        let snapshot_calls = Arc::new(AtomicUsize::new(0));
        let runtime = PrivacyRuntimeOwner::start_with(
            worker,
            initial.clone(),
            Box::new(AlwaysConflictRpc {
                save_calls: Arc::clone(&save_calls),
                snapshot_calls: Arc::clone(&snapshot_calls),
                snapshot: initial,
            }),
            Arc::new(ManualClock::new(0)),
        )
        .unwrap();
        runtime
            .sender()
            .try_submit(PauseCommand::PauseIndefinitely)
            .unwrap();

        for _ in 0..200 {
            if save_calls.load(Ordering::Acquire) >= MAX_RECONCILIATION_ATTEMPTS as usize {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            save_calls.load(Ordering::Acquire),
            MAX_RECONCILIATION_ATTEMPTS as usize
        );
        assert_eq!(
            snapshot_calls.load(Ordering::Acquire),
            MAX_RECONCILIATION_ATTEMPTS as usize
        );
        assert_eq!(runtime.gate().mode(), GateMode::Paused);
        assert_eq!(runtime.sender().status(), PauseStatus::Reconciling);
        runtime.stop().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    /// fake port 被 helper join 后才释放，第二次 stop 不能重复消费 JoinHandle。
    #[test]
    fn helper_stop只拥有一次join句柄() {
        struct DropProbe {
            dropped: Arc<AtomicU8>,
        }

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.dropped.store(1, Ordering::Release);
            }
        }

        impl SettingsRpcPort for DropProbe {
            fn snapshot(&mut self) -> Result<SettingsSnapshot, SettingsError> {
                Err(SettingsError::OutcomeUnknown)
            }

            fn save(
                &mut self,
                _expected_revision: u64,
                _settings: AppSettings,
            ) -> Result<SettingsSnapshot, SettingsError> {
                Err(SettingsError::OutcomeUnknown)
            }
        }

        let dropped = Arc::new(AtomicU8::new(0));
        let (events, _receiver) = mpsc::sync_channel(1);
        let mut helper = SettingsRpcHelper::start(
            Box::new(DropProbe {
                dropped: Arc::clone(&dropped),
            }),
            events,
        )
        .unwrap();
        helper.stop().unwrap();
        assert_eq!(dropped.load(Ordering::Acquire), 1);
        assert!(matches!(helper.stop(), Err(PauseControllerError::Closed)));
    }

    /// 提交线程与关闭线程争用同一锁时，关闭后的命令槽不残留迟到命令。
    #[test]
    fn 关闭与并发提交共享线性化锁() {
        let (wake, _receiver) = mpsc::sync_channel(1);
        let slot = Arc::new(CommandSlot {
            state: Mutex::new(CommandSlotState {
                pending: None,
                closed: false,
            }),
            wake,
        });
        let sender = PauseCommandSender {
            slot: Arc::clone(&slot),
            status: Arc::new(AtomicU8::new(PauseStatus::Active as u8)),
        };

        let state_guard = slot.state.lock().unwrap();
        let submitter = {
            let sender = sender.clone();
            thread::spawn(move || sender.try_submit(PauseCommand::PauseIndefinitely))
        };
        let closer = {
            let slot = Arc::clone(&slot);
            thread::spawn(move || slot.close())
        };
        drop(state_guard);

        let _ = submitter.join().unwrap();
        closer.join().unwrap();
        assert!(slot.is_closed());
        assert!(slot.state.lock().unwrap().pending.is_none());
        assert!(matches!(
            sender.try_submit(PauseCommand::Resume),
            Err(PauseControllerError::Closed)
        ));
    }

    /// SaveResult 后续 Snapshot 入队失败时，必须释放事务并保持暂停门禁关闭。
    #[test]
    fn 保存结果后的快照入队失败不会卡死事务() {
        let initial =
            SettingsSnapshot::new(AppSettings::default(), SettingsLoadSource::Defaults, 0);
        let gate = RecordingGate::new(GateMode::Active);
        let status = Arc::new(AtomicU8::new(PauseStatus::Active as u8));
        let (event_sender, event_receiver) = mpsc::sync_channel(16);
        let (rpc_sender, rpc_receiver) = mpsc::sync_channel(1);
        let slot = Arc::new(CommandSlot {
            state: Mutex::new(CommandSlotState {
                pending: None,
                closed: false,
            }),
            wake: event_sender.clone(),
        });
        let sender = PauseCommandSender {
            slot: Arc::clone(&slot),
            status: Arc::clone(&status),
        };
        let controller_slot = Arc::clone(&slot);
        let controller_gate = gate.clone();
        let controller_status = Arc::clone(&status);
        let join = thread::spawn(move || {
            controller_loop(
                controller_slot,
                controller_status,
                controller_gate,
                initial,
                None,
                Arc::new(ManualClock::new(0)),
                rpc_sender,
                event_receiver,
            );
        });

        sender.try_submit(PauseCommand::PauseIndefinitely).unwrap();
        let generation = match rpc_receiver.recv_timeout(Duration::from_secs(1)).unwrap() {
            RpcRequest::Save { generation, .. } => generation,
            RpcRequest::Snapshot { .. } | RpcRequest::Stop => panic!("首次请求不是 Save"),
        };
        // 让 controller 已经在途的 Save 回执可达，但让其后续 Snapshot 入队失败。
        drop(rpc_receiver);
        event_sender
            .send(ControllerEvent::SaveResult {
                generation,
                result: Err(SettingsError::RevisionConflict {
                    expected: 0,
                    actual: 1,
                }),
            })
            .unwrap();
        for _ in 0..200 {
            if sender.status() == PauseStatus::Reconciling {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(sender.status(), PauseStatus::Reconciling);
        assert_eq!(gate.mode(), GateMode::Paused);

        // 事务槽已释放；即使 helper 通道已断开，新 Resume 仍会被 controller 消费，但
        // Reconciling 的 fail-closed 语义不能因旧 Active snapshot 而误开门禁。
        sender.try_submit(PauseCommand::Resume).unwrap();
        for _ in 0..200 {
            let pending = slot.state.lock().unwrap().pending.is_some();
            if !pending && sender.status() == PauseStatus::Reconciling {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(slot.state.lock().unwrap().pending.is_none());
        assert_eq!(sender.status(), PauseStatus::Reconciling);
        assert_eq!(gate.mode(), GateMode::Paused);
        slot.close();
        join.join().unwrap();
    }

    /// SnapshotResult 后续 Save 入队失败时，必须释放事务并继续允许新的托盘命令。
    #[test]
    fn 快照结果后的保存入队失败不会卡死事务() {
        let initial =
            SettingsSnapshot::new(AppSettings::default(), SettingsLoadSource::Defaults, 0);
        let authoritative = initial.clone();
        let gate = RecordingGate::new(GateMode::Active);
        let status = Arc::new(AtomicU8::new(PauseStatus::Active as u8));
        let (event_sender, event_receiver) = mpsc::sync_channel(16);
        let (rpc_sender, rpc_receiver) = mpsc::sync_channel(1);
        let slot = Arc::new(CommandSlot {
            state: Mutex::new(CommandSlotState {
                pending: None,
                closed: false,
            }),
            wake: event_sender.clone(),
        });
        let sender = PauseCommandSender {
            slot: Arc::clone(&slot),
            status: Arc::clone(&status),
        };
        let controller_slot = Arc::clone(&slot);
        let controller_gate = gate.clone();
        let controller_status = Arc::clone(&status);
        let join = thread::spawn(move || {
            controller_loop(
                controller_slot,
                controller_status,
                controller_gate,
                initial,
                None,
                Arc::new(ManualClock::new(0)),
                rpc_sender,
                event_receiver,
            );
        });

        sender.try_submit(PauseCommand::PauseIndefinitely).unwrap();
        let generation = match rpc_receiver.recv_timeout(Duration::from_secs(1)).unwrap() {
            RpcRequest::Save { generation, .. } => generation,
            RpcRequest::Snapshot { .. } | RpcRequest::Stop => panic!("首次请求不是 Save"),
        };
        event_sender
            .send(ControllerEvent::SaveResult {
                generation,
                result: Err(SettingsError::RevisionConflict {
                    expected: 0,
                    actual: 1,
                }),
            })
            .unwrap();
        let snapshot_generation = match rpc_receiver.recv_timeout(Duration::from_secs(1)).unwrap() {
            RpcRequest::Snapshot { generation } => generation,
            RpcRequest::Save { .. } | RpcRequest::Stop => panic!("冲突后请求不是 Snapshot"),
        };
        // Snapshot 已被 helper 取走，后续 Save 发送时 receiver 已断开。
        drop(rpc_receiver);
        event_sender
            .send(ControllerEvent::SnapshotResult {
                generation: snapshot_generation,
                result: Ok(authoritative),
            })
            .unwrap();
        for _ in 0..200 {
            if sender.status() == PauseStatus::Reconciling {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(sender.status(), PauseStatus::Reconciling);
        assert_eq!(gate.mode(), GateMode::Paused);

        sender.try_submit(PauseCommand::Resume).unwrap();
        for _ in 0..200 {
            let pending = slot.state.lock().unwrap().pending.is_some();
            if !pending && sender.status() == PauseStatus::Reconciling {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(slot.state.lock().unwrap().pending.is_none());
        assert_eq!(sender.status(), PauseStatus::Reconciling);
        assert_eq!(gate.mode(), GateMode::Paused);
        slot.close();
        join.join().unwrap();
    }
}

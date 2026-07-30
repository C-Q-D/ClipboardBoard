//! 此模块提供收藏、单条删除和显式双范围清空历史的有界异步命令桥。
//!
//! UI 线程只执行非阻塞提交；三个独立后台 worker 均通过唯一存储 worker 串行访问 SQLite。
//! 关闭会拒绝新请求，但保留并排空已经接受的请求，确保退出不会丢失已承诺的事务。

use std::{
    io,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};

use crate::{
    image_pipeline::ImageWorkerSender,
    storage::{DeleteHistoryInput, SetPinnedInput, StorageClient, StorageError},
};

/// 收藏请求的固定队列容量；UI 同时只允许一个活动 mutation。
const PIN_MUTATION_QUEUE_CAPACITY: usize = 1;

/// 一次收藏状态变更的稳定身份；不携带正文、预览或来源信息。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinMutationRequest {
    /// UI 分配的单调 mutation 令牌，用于隔离同记录的迟到结果。
    pub mutation_token: u64,
    /// 点击发生时的面板代次，用于区分隐藏后重新打开的会话。
    pub panel_generation: u64,
    /// 历史记录数据库 ID。
    pub id: u64,
    /// 与 ID 同时校验的固定内容哈希。
    pub content_hash: [u8; 32],
    /// 用户要求事务提交后的明确收藏状态。
    pub is_pinned: bool,
}

/// 收藏 worker 对外暴露的有限失败类别；底层错误详情不会进入 UI 或诊断格式。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PinMutationFailure {
    /// ID 与内容哈希不再指向同一条记录，调用方应刷新当前数据集。
    IdentityChanged,
    /// 存储正在关闭、不可用或返回其他有限外部失败。
    StorageUnavailable,
}

/// 收藏事务完成结果；完整回显请求身份以便 UI 严格匹配活动 mutation。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinMutationResult {
    /// UI 分配的单调 mutation 令牌。
    pub mutation_token: u64,
    /// 点击发生时的面板代次。
    pub panel_generation: u64,
    /// 历史记录数据库 ID。
    pub id: u64,
    /// 与 ID 同时校验的固定内容哈希。
    pub content_hash: [u8; 32],
    /// 请求的明确收藏状态。
    pub is_pinned: bool,
    /// 事务成功或有限失败。
    pub outcome: Result<(), PinMutationFailure>,
}

/// UI 非阻塞提交收藏请求时的有限拒绝原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PinMutationSubmitError {
    /// 单槽已经包含一个等待处理的请求。
    Full,
    /// 请求入口已经关闭，不再接受新的变更。
    Closed,
}

/// 有界队列的互斥状态；关闭不清空 `pending`，由 worker 排空后退出。
struct QueueState {
    /// 唯一等待 worker 处理的请求。
    pending: Option<PinMutationRequest>,
    /// 关闭线性化标志；置位后所有新请求稳定失败。
    closed: bool,
}

/// 发送端和接收端共享的队列核心。
struct QueueShared {
    /// 同时保护 pending 和关闭标志，确保关闭与提交顺序明确。
    state: Mutex<QueueState>,
    /// 请求提交或关闭时唤醒 worker。
    ready: Condvar,
}

/// 可克隆的收藏请求入口；克隆不拥有 worker 或 SQLite 生命周期。
#[derive(Clone)]
pub struct PinMutationSender {
    /// 共享单槽状态。
    shared: Arc<QueueShared>,
}

/// 收藏 worker 独占的接收端；只有它可以取出已经接受的请求。
pub struct PinMutationReceiver {
    /// 与发送端共享的单槽状态。
    shared: Arc<QueueShared>,
}

/// 创建容量固定为一的收藏请求通道。
pub fn pin_mutation_channel() -> (PinMutationSender, PinMutationReceiver) {
    debug_assert_eq!(PIN_MUTATION_QUEUE_CAPACITY, 1);
    let shared = Arc::new(QueueShared {
        state: Mutex::new(QueueState {
            pending: None,
            closed: false,
        }),
        ready: Condvar::new(),
    });
    (
        PinMutationSender {
            shared: Arc::clone(&shared),
        },
        PinMutationReceiver { shared },
    )
}

impl PinMutationSender {
    /// 非阻塞提交一个收藏请求；满队列或关闭时立即返回有限错误。
    pub fn try_submit(&self, request: PinMutationRequest) -> Result<(), PinMutationSubmitError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| PinMutationSubmitError::Closed)?;
        if state.closed {
            return Err(PinMutationSubmitError::Closed);
        }
        if state.pending.is_some() {
            return Err(PinMutationSubmitError::Full);
        }
        state.pending = Some(request);
        self.shared.ready.notify_one();
        Ok(())
    }

    /// 关闭请求入口并唤醒 worker；已经接受的单槽请求仍会被处理。
    pub fn close(&self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.closed = true;
            self.shared.ready.notify_all();
        }
    }
}

impl PinMutationReceiver {
    /// 阻塞等待下一请求；关闭且队列排空后返回 `None`。
    fn receive(&self) -> Option<PinMutationRequest> {
        let mut state = self.shared.state.lock().ok()?;
        loop {
            if let Some(request) = state.pending.take() {
                return Some(request);
            }
            if state.closed {
                return None;
            }
            state = self.shared.ready.wait(state).ok()?;
        }
    }
}

/// 启动单一收藏 worker；已提交事务无论 UI 是否仍可接收结果都必须完成。
pub fn start_pin_mutation_worker<E>(
    storage: StorageClient,
    receiver: PinMutationReceiver,
    mut emit: E,
) -> io::Result<JoinHandle<()>>
where
    E: FnMut(PinMutationResult) -> bool + Send + 'static,
{
    thread::Builder::new()
        .name("clipboard-board-pin-mutation".to_owned())
        .spawn(move || {
            while let Some(request) = receiver.receive() {
                let result = execute_pin_mutation(&storage, request);
                // 事件循环退出后结果可以丢弃，但数据库事务已经在返回结果前提交。
                let _ = emit(result);
            }
        })
}

/// 执行一次收藏事务并把所有底层错误压缩为有限类别。
fn execute_pin_mutation(storage: &StorageClient, request: PinMutationRequest) -> PinMutationResult {
    let outcome = i64::try_from(request.id)
        .map_err(|_| PinMutationFailure::IdentityChanged)
        .and_then(|id| {
            storage
                .set_history_pinned(SetPinnedInput {
                    id,
                    content_hash: request.content_hash,
                    is_pinned: request.is_pinned,
                })
                .map(|_| ())
                .map_err(map_storage_failure)
        });

    PinMutationResult {
        mutation_token: request.mutation_token,
        panel_generation: request.panel_generation,
        id: request.id,
        content_hash: request.content_hash,
        is_pinned: request.is_pinned,
        outcome,
    }
}

/// 将存储错误压缩为不泄露 SQL 或原生错误详情的有限类别。
fn map_storage_failure(error: StorageError) -> PinMutationFailure {
    match error {
        StorageError::HistoryIdentityMismatch { .. } => PinMutationFailure::IdentityChanged,
        _ => PinMutationFailure::StorageUnavailable,
    }
}

/// 删除请求的固定队列容量；UI 会在 DEL-03 维持跨收藏和删除的全局 mutation 互斥。
const DELETE_MUTATION_QUEUE_CAPACITY: usize = 1;

/// 一次单条删除的稳定身份；不携带正文、预览、来源或内容类型。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeleteMutationRequest {
    /// UI 分配的单调 mutation 令牌，用于隔离迟到结果。
    pub mutation_token: u64,
    /// 点击发生时的面板代次；结果与 pending 匹配，但不要求面板仍处于该代次。
    pub panel_generation: u64,
    /// 历史记录数据库 ID。
    pub id: u64,
    /// 与 ID 同时校验的固定内容哈希。
    pub content_hash: [u8; 32],
}

/// 删除 worker 对外暴露的有限失败类别；底层错误详情不会进入 UI。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeleteMutationFailure {
    /// ID 与内容哈希不再指向同一条记录，调用方应刷新当前数据集。
    IdentityChanged,
    /// 目标存在但不是当前允许删除的文本记录。
    NotDeletable,
    /// 存储正在关闭、不可用或返回其他有限外部失败。
    StorageUnavailable,
}

/// 删除事务完成结果；完整回显请求身份以便 UI 严格匹配活动 mutation。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeleteMutationResult {
    /// UI 分配的单调 mutation 令牌。
    pub mutation_token: u64,
    /// 点击发生时的面板代次。
    pub panel_generation: u64,
    /// 历史记录数据库 ID。
    pub id: u64,
    /// 与 ID 同时校验的固定内容哈希。
    pub content_hash: [u8; 32],
    /// 事务成功或有限失败；目标已不存在同样属于成功。
    pub outcome: Result<(), DeleteMutationFailure>,
}

/// UI 非阻塞提交删除请求时的有限拒绝原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeleteMutationSubmitError {
    /// 单槽已经包含一个等待处理的请求。
    Full,
    /// 请求入口已经关闭，不再接受新的变更。
    Closed,
}

/// 删除队列的互斥状态；关闭不清空已接受请求。
struct DeleteQueueState {
    /// 唯一等待 worker 处理的请求。
    pending: Option<DeleteMutationRequest>,
    /// 关闭线性化标志；置位后所有新请求稳定失败。
    closed: bool,
}

/// 删除发送端和接收端共享的单槽核心。
struct DeleteQueueShared {
    /// 同时保护请求与关闭标志，确保关闭和提交有明确先后。
    state: Mutex<DeleteQueueState>,
    /// 请求提交或关闭时唤醒 worker。
    ready: Condvar,
}

/// 可克隆的删除请求入口；克隆不拥有 worker 或 SQLite 生命周期。
#[derive(Clone)]
pub struct DeleteMutationSender {
    /// 共享单槽状态。
    shared: Arc<DeleteQueueShared>,
}

/// 删除 worker 独占的接收端；只有它可以取出已接受请求。
pub struct DeleteMutationReceiver {
    /// 与发送端共享的单槽状态。
    shared: Arc<DeleteQueueShared>,
}

/// 创建容量固定为一的单条删除请求通道。
pub fn delete_mutation_channel() -> (DeleteMutationSender, DeleteMutationReceiver) {
    debug_assert_eq!(DELETE_MUTATION_QUEUE_CAPACITY, 1);
    let shared = Arc::new(DeleteQueueShared {
        state: Mutex::new(DeleteQueueState {
            pending: None,
            closed: false,
        }),
        ready: Condvar::new(),
    });
    (
        DeleteMutationSender {
            shared: Arc::clone(&shared),
        },
        DeleteMutationReceiver { shared },
    )
}

impl DeleteMutationSender {
    /// 非阻塞提交一个删除请求；满队列或关闭时立即返回有限错误。
    pub fn try_submit(
        &self,
        request: DeleteMutationRequest,
    ) -> Result<(), DeleteMutationSubmitError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| DeleteMutationSubmitError::Closed)?;
        if state.closed {
            return Err(DeleteMutationSubmitError::Closed);
        }
        if state.pending.is_some() {
            return Err(DeleteMutationSubmitError::Full);
        }
        state.pending = Some(request);
        self.shared.ready.notify_one();
        Ok(())
    }

    /// 关闭请求入口并唤醒 worker；已经接受的单槽请求仍会被处理。
    pub fn close(&self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.closed = true;
            self.shared.ready.notify_all();
        }
    }
}

impl DeleteMutationReceiver {
    /// 阻塞等待下一请求；关闭且队列排空后返回 `None`。
    fn receive(&self) -> Option<DeleteMutationRequest> {
        let mut state = self.shared.state.lock().ok()?;
        loop {
            if let Some(request) = state.pending.take() {
                return Some(request);
            }
            if state.closed {
                return None;
            }
            state = self.shared.ready.wait(state).ok()?;
        }
    }
}

/// 启动单一删除 worker；已提交事务无论 UI 是否仍可接收结果都必须完成。
pub fn start_delete_mutation_worker<E>(
    storage: StorageClient,
    image_worker: Option<ImageWorkerSender>,
    receiver: DeleteMutationReceiver,
    mut emit: E,
) -> io::Result<JoinHandle<()>>
where
    E: FnMut(DeleteMutationResult) -> bool + Send + 'static,
{
    thread::Builder::new()
        .name("clipboard-board-delete-mutation".to_owned())
        .spawn(move || {
            while let Some(request) = receiver.receive() {
                let result = execute_delete_mutation(&storage, image_worker.as_ref(), request);
                // 结果投递失败只表示 UI 已退出；SQLite 事务已经完成，不能反向回滚。
                let _ = emit(result);
            }
        })
}

/// 执行一次删除事务并把所有底层错误压缩为有限类别。
fn execute_delete_mutation(
    storage: &StorageClient,
    image_worker: Option<&ImageWorkerSender>,
    request: DeleteMutationRequest,
) -> DeleteMutationResult {
    let outcome = i64::try_from(request.id)
        .map_err(|_| DeleteMutationFailure::IdentityChanged)
        .and_then(|id| {
            let result = storage
                .delete_history(DeleteHistoryInput {
                    id,
                    content_hash: request.content_hash,
                })
                .map_err(map_delete_storage_failure)?;
            recycle_images(storage, image_worker, result.recycled_image);
            Ok(())
        });

    DeleteMutationResult {
        mutation_token: request.mutation_token,
        panel_generation: request.panel_generation,
        id: request.id,
        content_hash: request.content_hash,
        outcome,
    }
}

/// 将删除存储错误压缩为不泄露 SQL、类型值或正文的有限类别。
fn map_delete_storage_failure(error: StorageError) -> DeleteMutationFailure {
    match error {
        StorageError::HistoryIdentityMismatch { .. } => DeleteMutationFailure::IdentityChanged,
        StorageError::HistoryItemNotDeletable { .. } => DeleteMutationFailure::NotDeletable,
        _ => DeleteMutationFailure::StorageUnavailable,
    }
}

/// 清空请求的固定队列容量；两种范围共享同一 worker 和 UI mutation 互斥。
const CLEAR_HISTORY_QUEUE_CAPACITY: usize = 1;

/// 清空历史的显式危险范围；故意不实现 `Default`，避免调用方隐式选择全量删除。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClearHistoryScope {
    /// 只删除未收藏文本，收藏和非文本记录保持不变。
    UnpinnedText,
    /// 删除数据库中的全部类型和收藏状态记录。
    All,
}

/// 一次清空请求的稳定 UI 身份；请求不携带任何记录正文或哈希。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClearHistoryMutationRequest {
    /// UI 分配的单调 mutation 令牌，用于隔离迟到结果。
    pub mutation_token: u64,
    /// 确认发生时的面板代次；结果与 pending 匹配但不要求面板仍可见。
    pub panel_generation: u64,
    /// 调用方必须显式选择的删除范围；不得由 worker 猜测或提供默认值。
    pub scope: ClearHistoryScope,
}

/// 清空事务成功后的有限信息；修订号用于上层区分清空前后捕获。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClearHistoryMutationSuccess {
    /// 本次事务在明确范围内实际删除的记录数量。
    pub deleted_count: u64,
    /// 唯一存储线程分配的清空线性化修订号。
    pub clear_revision: u64,
}

/// 清空 worker 对外暴露的有限失败；底层 SQL 和系统错误不得进入 UI。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClearHistoryMutationFailure {
    /// 存储正在关闭、不可用、修订号耗尽或返回其他有限外部失败。
    StorageUnavailable,
}

/// 清空事务完成结果；完整回显请求身份并无损携带成功修订号。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClearHistoryMutationResult {
    /// UI 分配的单调 mutation 令牌。
    pub mutation_token: u64,
    /// 确认发生时的面板代次。
    pub panel_generation: u64,
    /// 从请求原样回显的清空范围，UI 必须与 pending 身份共同校验。
    pub scope: ClearHistoryScope,
    /// 事务成功的有限信息或固定失败类别。
    pub outcome: Result<ClearHistoryMutationSuccess, ClearHistoryMutationFailure>,
}

/// UI 非阻塞提交清空请求时的有限拒绝原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClearHistoryMutationSubmitError {
    /// 单槽已经包含一个等待处理的请求。
    Full,
    /// 请求入口已经关闭，不再接受新的清空。
    Closed,
}

/// 清空队列的互斥状态；关闭不清除已经接受的唯一请求。
struct ClearHistoryQueueState {
    /// 唯一等待 worker 处理的请求。
    pending: Option<ClearHistoryMutationRequest>,
    /// 关闭线性化标志；置位后所有新请求稳定失败。
    closed: bool,
}

/// 清空发送端和接收端共享的单槽核心。
struct ClearHistoryQueueShared {
    /// 同时保护请求与关闭标志，确保关闭和提交有明确先后。
    state: Mutex<ClearHistoryQueueState>,
    /// 请求提交或关闭时唤醒 worker。
    ready: Condvar,
}

/// 可克隆的清空请求入口；克隆不拥有 worker 或 SQLite 生命周期。
#[derive(Clone)]
pub struct ClearHistoryMutationSender {
    /// 共享单槽状态。
    shared: Arc<ClearHistoryQueueShared>,
}

/// 清空 worker 独占的接收端；只有它可以取出已接受请求。
pub struct ClearHistoryMutationReceiver {
    /// 与发送端共享的单槽状态。
    shared: Arc<ClearHistoryQueueShared>,
}

/// 创建容量固定为一的双范围清空请求通道。
pub fn clear_history_mutation_channel() -> (ClearHistoryMutationSender, ClearHistoryMutationReceiver)
{
    debug_assert_eq!(CLEAR_HISTORY_QUEUE_CAPACITY, 1);
    let shared = Arc::new(ClearHistoryQueueShared {
        state: Mutex::new(ClearHistoryQueueState {
            pending: None,
            closed: false,
        }),
        ready: Condvar::new(),
    });
    (
        ClearHistoryMutationSender {
            shared: Arc::clone(&shared),
        },
        ClearHistoryMutationReceiver { shared },
    )
}

impl ClearHistoryMutationSender {
    /// 非阻塞提交一个清空请求；满队列或关闭时立即返回有限错误。
    pub fn try_submit(
        &self,
        request: ClearHistoryMutationRequest,
    ) -> Result<(), ClearHistoryMutationSubmitError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| ClearHistoryMutationSubmitError::Closed)?;
        if state.closed {
            return Err(ClearHistoryMutationSubmitError::Closed);
        }
        if state.pending.is_some() {
            return Err(ClearHistoryMutationSubmitError::Full);
        }
        state.pending = Some(request);
        self.shared.ready.notify_one();
        Ok(())
    }

    /// 关闭请求入口并唤醒 worker；已经接受的单槽请求仍会被处理。
    pub fn close(&self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.closed = true;
            self.shared.ready.notify_all();
        }
    }
}

impl ClearHistoryMutationReceiver {
    /// 阻塞等待下一请求；关闭且队列排空后返回 `None`。
    fn receive(&self) -> Option<ClearHistoryMutationRequest> {
        let mut state = self.shared.state.lock().ok()?;
        loop {
            if let Some(request) = state.pending.take() {
                return Some(request);
            }
            if state.closed {
                return None;
            }
            state = self.shared.ready.wait(state).ok()?;
        }
    }
}

/// 启动单一清空 worker；已接受事务不因 UI 结果接收端退出而撤销。
pub fn start_clear_history_mutation_worker<E>(
    storage: StorageClient,
    image_worker: Option<ImageWorkerSender>,
    receiver: ClearHistoryMutationReceiver,
    mut emit: E,
) -> io::Result<JoinHandle<()>>
where
    E: FnMut(ClearHistoryMutationResult) -> bool + Send + 'static,
{
    thread::Builder::new()
        .name("clipboard-board-clear-history".to_owned())
        .spawn(move || {
            while let Some(request) = receiver.receive() {
                let result =
                    execute_clear_history_mutation(&storage, image_worker.as_ref(), request);
                // UI 已退出时只丢弃有限结果；数据库事务已经完成，不能反向回滚。
                let _ = emit(result);
            }
        })
}

/// 按请求的显式范围执行一次清空事务，并将任意存储错误压缩为固定失败类别。
fn execute_clear_history_mutation(
    storage: &StorageClient,
    image_worker: Option<&ImageWorkerSender>,
    request: ClearHistoryMutationRequest,
) -> ClearHistoryMutationResult {
    let storage_result = match request.scope {
        ClearHistoryScope::UnpinnedText => storage
            .clear_unpinned_text()
            .map(|result| (result.deleted_count, result.mutation_revision)),
        ClearHistoryScope::All => storage.clear_all_history().map(|result| {
            recycle_images(storage, image_worker, result.recycled_images);
            (result.deleted_count, result.mutation_revision)
        }),
    };
    let outcome = storage_result
        .map(|result| ClearHistoryMutationSuccess {
            deleted_count: result.0,
            clear_revision: result.1,
        })
        .map_err(|_| ClearHistoryMutationFailure::StorageUnavailable);

    ClearHistoryMutationResult {
        mutation_token: request.mutation_token,
        panel_generation: request.panel_generation,
        scope: request.scope,
        outcome,
    }
}

/// 将数据库已提交删除返回的图片资产逐项交给独占 ImageWorker 回收。
///
/// 回收失败不能恢复已经提交的数据库行；当前有限重试失败由后续启动期对账原子处理。
fn recycle_images(
    storage: &StorageClient,
    image_worker: Option<&ImageWorkerSender>,
    images: impl IntoIterator<Item = crate::domain::ImageMetadata>,
) {
    let Some(image_worker) = image_worker else {
        return;
    };
    for image in images {
        let Ok(receiver) = image_worker.recycle(storage.clone(), image) else {
            continue;
        };
        let _ = receiver.recv();
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证收藏、删除与双范围清空桥的单槽边界、路由、关闭排空和有限结果映射。

    use std::{
        fs,
        path::PathBuf,
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc::sync_channel,
        },
    };

    use rusqlite::{params, Connection};

    use super::{
        clear_history_mutation_channel, delete_mutation_channel, pin_mutation_channel,
        start_clear_history_mutation_worker, start_delete_mutation_worker,
        start_pin_mutation_worker, ClearHistoryMutationFailure, ClearHistoryMutationRequest,
        ClearHistoryMutationSubmitError, ClearHistoryScope, DeleteMutationFailure,
        DeleteMutationRequest, DeleteMutationSubmitError, PinMutationFailure, PinMutationRequest,
        PinMutationSubmitError,
    };
    use crate::storage::{StorageExecutor, TextUpsertInput};

    static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    /// 创建当前测试独占的 SQLite 目录。
    fn temporary_directory() -> PathBuf {
        let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "clipboard-board-history-mutation-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("创建收藏桥测试目录失败");
        directory
    }

    /// 构造不含正文的稳定收藏请求。
    fn request(token: u64, id: u64, hash: [u8; 32]) -> PinMutationRequest {
        PinMutationRequest {
            mutation_token: token,
            panel_generation: 3,
            id,
            content_hash: hash,
            is_pinned: true,
        }
    }

    /// 构造不含正文的稳定删除请求。
    fn delete_request(token: u64, id: u64, hash: [u8; 32]) -> DeleteMutationRequest {
        DeleteMutationRequest {
            mutation_token: token,
            panel_generation: 5,
            id,
            content_hash: hash,
        }
    }

    /// 构造不含任何记录内容的清空请求。
    fn clear_request(token: u64) -> ClearHistoryMutationRequest {
        clear_request_for_scope(token, ClearHistoryScope::UnpinnedText)
    }

    /// 构造显式携带危险范围的清空请求，禁止测试依赖隐式默认值。
    fn clear_request_for_scope(
        token: u64,
        scope: ClearHistoryScope,
    ) -> ClearHistoryMutationRequest {
        ClearHistoryMutationRequest {
            mutation_token: token,
            panel_generation: 7,
            scope,
        }
    }

    /// 单槽满时必须立即拒绝第二个请求，关闭后也必须稳定拒绝。
    #[test]
    fn bounded_channel_rejects_full_and_closed_without_blocking() {
        let (sender, _receiver) = pin_mutation_channel();
        sender
            .try_submit(request(1, 1, [1; 32]))
            .expect("首个请求应进入单槽");
        assert_eq!(
            sender.try_submit(request(2, 2, [2; 32])),
            Err(PinMutationSubmitError::Full)
        );
        sender.close();
        assert_eq!(
            sender.try_submit(request(3, 3, [3; 32])),
            Err(PinMutationSubmitError::Closed)
        );
    }

    /// 关闭桥后已经接受的请求仍须提交，worker 排空后才能正常退出。
    #[test]
    fn close_drains_accepted_request_before_worker_exit() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let inserted = executor
            .upsert_text(TextUpsertInput {
                content_hash: [7; 32],
                text_content: "仅用于存储的正文".to_owned(),
                preview_text: "收藏桥预览".to_owned(),
                source_exe: None,
                source_app: None,
                copied_at: 1,
            })
            .expect("写入收藏桥测试记录失败");
        let (sender, receiver) = pin_mutation_channel();
        let (result_sender, result_receiver) = sync_channel(1);
        let worker = start_pin_mutation_worker(executor.client(), receiver, move |result| {
            result_sender.send(result).is_ok()
        })
        .expect("启动收藏 worker 失败");

        sender
            .try_submit(request(
                9,
                u64::try_from(inserted.id).expect("测试 ID 应为正数"),
                inserted.content_hash,
            ))
            .expect("提交收藏请求失败");
        sender.close();
        worker.join().expect("收藏 worker 异常退出");

        let result = result_receiver.recv().expect("未收到收藏结果");
        assert_eq!(result.mutation_token, 9);
        assert_eq!(result.outcome, Ok(()));
        let inserted_id = inserted.id;
        drop(executor);
        let reopened = StorageExecutor::open_at(&directory).expect("退出后重启存储线程失败");
        assert!(
            reopened
                .get_history_payload(inserted_id)
                .expect("重启后读取收藏结果失败")
                .expect("重启后收藏记录不存在")
                .is_pinned
        );
        drop(reopened);
        fs::remove_dir_all(directory).expect("清理收藏桥测试目录失败");
    }

    /// 错误身份只返回有限类别，结果仍完整回显请求身份。
    #[test]
    fn stale_identity_maps_to_finite_failure() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动存储线程失败");
        let (sender, receiver) = pin_mutation_channel();
        let (result_sender, result_receiver) = sync_channel(1);
        let worker = start_pin_mutation_worker(executor.client(), receiver, move |result| {
            result_sender.send(result).is_ok()
        })
        .expect("启动收藏 worker 失败");
        let stale = request(11, 999, [8; 32]);
        sender.try_submit(stale.clone()).expect("提交陈旧请求失败");
        sender.close();
        worker.join().expect("收藏 worker 异常退出");

        let result = result_receiver.recv().expect("未收到失败结果");
        assert_eq!(result.mutation_token, stale.mutation_token);
        assert_eq!(result.panel_generation, stale.panel_generation);
        assert_eq!(result.id, stale.id);
        assert_eq!(result.content_hash, stale.content_hash);
        assert_eq!(result.is_pinned, stale.is_pinned);
        assert_eq!(result.outcome, Err(PinMutationFailure::IdentityChanged));
        drop(executor);
        fs::remove_dir_all(directory).expect("清理收藏桥测试目录失败");
    }

    /// 删除单槽满时必须立即拒绝第二个请求，关闭后也必须稳定拒绝。
    #[test]
    fn delete_bounded_channel_rejects_full_and_closed_without_blocking() {
        let (sender, _receiver) = delete_mutation_channel();
        sender
            .try_submit(delete_request(1, 1, [1; 32]))
            .expect("首个删除请求应进入单槽");
        assert_eq!(
            sender.try_submit(delete_request(2, 2, [2; 32])),
            Err(DeleteMutationSubmitError::Full)
        );
        sender.close();
        assert_eq!(
            sender.try_submit(delete_request(3, 3, [3; 32])),
            Err(DeleteMutationSubmitError::Closed)
        );
    }

    /// 关闭桥后已接受的删除仍须提交，重启存储后目标记录不能恢复。
    #[test]
    fn delete_close_drains_accepted_request_before_worker_exit() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动删除桥存储线程失败");
        let inserted = executor
            .upsert_text(TextUpsertInput {
                content_hash: [21; 32],
                text_content: "只在 SQLite 中保存的删除正文".to_owned(),
                preview_text: "删除桥预览".to_owned(),
                source_exe: None,
                source_app: None,
                copied_at: 1,
            })
            .expect("写入删除桥测试记录失败");
        let (sender, receiver) = delete_mutation_channel();
        let (result_sender, result_receiver) = sync_channel(1);
        let worker = start_delete_mutation_worker(executor.client(), None, receiver, move |result| {
            result_sender.send(result).is_ok()
        })
        .expect("启动删除 worker 失败");

        sender
            .try_submit(delete_request(
                19,
                u64::try_from(inserted.id).expect("测试 ID 应为正数"),
                inserted.content_hash,
            ))
            .expect("提交删除请求失败");
        sender.close();
        worker.join().expect("删除 worker 异常退出");

        let result = result_receiver.recv().expect("未收到删除结果");
        assert_eq!(result.mutation_token, 19);
        assert_eq!(result.panel_generation, 5);
        assert_eq!(result.id, u64::try_from(inserted.id).unwrap());
        assert_eq!(result.content_hash, inserted.content_hash);
        assert_eq!(result.outcome, Ok(()));
        let inserted_id = inserted.id;
        drop(executor);

        let reopened = StorageExecutor::open_at(&directory).expect("删除后重启存储线程失败");
        assert!(reopened
            .get_history_payload(inserted_id)
            .expect("重启后读取删除结果失败")
            .is_none());
        drop(reopened);
        fs::remove_dir_all(directory).expect("清理删除桥测试目录失败");
    }

    /// 删除桥必须把身份错配和非文本门禁压缩为两个稳定有限类别。
    #[test]
    fn delete_failure_mapping_is_finite_and_preserves_request_identity() {
        let directory = temporary_directory();
        {
            // 先由正式存储入口完成迁移，再用测试连接预置未来类型记录。
            let executor =
                StorageExecutor::open_at(&directory).expect("初始化删除失败映射数据库失败");
            drop(executor);
        }
        let non_text_id = {
            let database_path = directory.join("clipboard.db");
            let connection = Connection::open(&database_path).expect("打开删除失败映射数据库失败");
            connection
                .execute(
                    "INSERT INTO clipboard_items \
                     (item_type, preview_text, content_hash, created_at, copied_at) \
                     VALUES ('binary', '非文本预览', ?1, 1, 1)",
                    params![[22_u8; 32].as_slice()],
                )
                .expect("写入非文本记录失败");
            connection.last_insert_rowid()
        };
        let executor = StorageExecutor::open_at(&directory).expect("启动失败映射存储线程失败");
        let text = executor
            .upsert_text(TextUpsertInput {
                content_hash: [23; 32],
                text_content: "身份测试正文".to_owned(),
                preview_text: "身份测试预览".to_owned(),
                source_exe: None,
                source_app: None,
                copied_at: 2,
            })
            .expect("写入身份测试文本失败");
        let (sender, receiver) = delete_mutation_channel();
        let (result_sender, result_receiver) = sync_channel(2);
        let worker = start_delete_mutation_worker(executor.client(), None, receiver, move |result| {
            result_sender.send(result).is_ok()
        })
        .expect("启动失败映射删除 worker 失败");

        let stale = delete_request(
            31,
            u64::try_from(text.id).expect("文本测试 ID 应为正数"),
            [99; 32],
        );
        sender.try_submit(stale).expect("提交陈旧删除请求失败");
        let stale_result = result_receiver.recv().expect("未收到身份错配结果");
        assert_eq!(stale_result.mutation_token, stale.mutation_token);
        assert_eq!(stale_result.panel_generation, stale.panel_generation);
        assert_eq!(stale_result.id, stale.id);
        assert_eq!(stale_result.content_hash, stale.content_hash);
        assert_eq!(
            stale_result.outcome,
            Err(DeleteMutationFailure::IdentityChanged)
        );

        let non_text = delete_request(
            32,
            u64::try_from(non_text_id).expect("非文本测试 ID 应为正数"),
            [22; 32],
        );
        sender.try_submit(non_text).expect("提交非文本删除请求失败");
        sender.close();
        worker.join().expect("失败映射删除 worker 异常退出");
        let non_text_result = result_receiver.recv().expect("未收到非文本门禁结果");
        assert_eq!(non_text_result.mutation_token, non_text.mutation_token);
        assert_eq!(non_text_result.id, non_text.id);
        assert_eq!(
            non_text_result.outcome,
            Err(DeleteMutationFailure::NotDeletable)
        );
        drop(executor);
        fs::remove_dir_all(directory).expect("清理删除失败映射目录失败");
    }

    /// UI 结果接收端消失不能阻塞删除提交或 worker 退出。
    #[test]
    fn delete_emit_false_does_not_undo_committed_transaction() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动投递失败测试存储线程失败");
        let inserted = executor
            .upsert_text(TextUpsertInput {
                content_hash: [24; 32],
                text_content: "投递失败正文".to_owned(),
                preview_text: "投递失败预览".to_owned(),
                source_exe: None,
                source_app: None,
                copied_at: 3,
            })
            .expect("写入投递失败测试记录失败");
        let (sender, receiver) = delete_mutation_channel();
        let worker = start_delete_mutation_worker(executor.client(), None, receiver, |_result| false)
            .expect("启动投递失败删除 worker 失败");
        sender
            .try_submit(delete_request(
                41,
                u64::try_from(inserted.id).expect("测试 ID 应为正数"),
                inserted.content_hash,
            ))
            .expect("提交投递失败删除请求失败");
        sender.close();
        worker.join().expect("投递失败删除 worker 异常退出");
        let inserted_id = inserted.id;
        drop(executor);

        let reopened = StorageExecutor::open_at(&directory).expect("投递失败后重启存储线程失败");
        assert!(reopened
            .get_history_payload(inserted_id)
            .expect("投递失败后读取记录失败")
            .is_none());
        drop(reopened);
        fs::remove_dir_all(directory).expect("清理投递失败测试目录失败");
    }

    /// 清空单槽满时立即拒绝第二个请求，关闭后稳定拒绝新请求。
    #[test]
    fn clear_unpinned_channel_rejects_full_and_closed_without_blocking() {
        let (sender, _receiver) = clear_history_mutation_channel();
        sender
            .try_submit(clear_request(1))
            .expect("首个清空请求应进入单槽");
        assert_eq!(
            sender.try_submit(clear_request(2)),
            Err(ClearHistoryMutationSubmitError::Full)
        );
        sender.close();
        assert_eq!(
            sender.try_submit(clear_request(3)),
            Err(ClearHistoryMutationSubmitError::Closed)
        );
    }

    /// 关闭桥后已接受清空仍须提交，并无损返回删除数量和存储修订号。
    #[test]
    fn clear_unpinned_close_drains_request_and_preserves_revision() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动清空桥存储线程失败");
        let inserted = executor
            .upsert_text(TextUpsertInput {
                content_hash: [31; 32],
                text_content: "清空桥正文".to_owned(),
                preview_text: "清空桥预览".to_owned(),
                source_exe: None,
                source_app: None,
                copied_at: 1,
            })
            .expect("写入清空桥测试记录失败");
        let pinned = executor
            .upsert_text(TextUpsertInput {
                content_hash: [35; 32],
                text_content: "保留收藏正文".to_owned(),
                preview_text: "保留收藏预览".to_owned(),
                source_exe: None,
                source_app: None,
                copied_at: 2,
            })
            .expect("写入清空桥收藏记录失败");
        executor
            .set_history_pinned(crate::storage::SetPinnedInput {
                id: pinned.id,
                content_hash: pinned.content_hash,
                is_pinned: true,
            })
            .expect("设置清空桥收藏失败");
        {
            let connection =
                Connection::open(executor.database_path()).expect("打开清空桥混合数据库失败");
            connection
                .execute(
                    "INSERT INTO clipboard_items
                     (item_type, preview_text, content_hash, is_pinned, created_at, copied_at)
                     VALUES ('binary', '保留非文本', ?1, 0, 3, 3)",
                    params![[36_u8; 32].as_slice()],
                )
                .expect("写入清空桥图片行失败");
        }
        let (sender, receiver) = clear_history_mutation_channel();
        let (result_sender, result_receiver) = sync_channel(1);
        let worker =
            start_clear_history_mutation_worker(executor.client(), None, receiver, move |result| {
                result_sender.send(result).is_ok()
            })
            .expect("启动清空 worker 失败");

        sender
            .try_submit(clear_request(11))
            .expect("提交清空请求失败");
        sender.close();
        worker.join().expect("清空 worker 异常退出");

        let result = result_receiver.recv().expect("未收到清空结果");
        assert_eq!(result.mutation_token, 11);
        assert_eq!(result.panel_generation, 7);
        assert_eq!(result.scope, ClearHistoryScope::UnpinnedText);
        let success = result.outcome.expect("清空事务不应失败");
        assert_eq!(success.deleted_count, 1);
        assert_eq!(success.clear_revision, pinned.mutation_revision + 1);
        assert!(executor
            .get_history_payload(inserted.id)
            .expect("读取清空结果失败")
            .is_none());
        assert!(executor
            .get_history_payload(pinned.id)
            .expect("读取保留收藏失败")
            .is_some());
        assert_eq!(
            executor
                .status()
                .expect("读取未收藏路由结果失败")
                .clipboard_item_count,
            2
        );

        drop(executor);
        fs::remove_dir_all(directory).expect("清理清空桥测试目录失败");
    }

    /// 存储不可用时只返回固定失败类别，并完整回显请求身份。
    #[test]
    fn clear_unpinned_maps_storage_error_to_finite_failure() {
        let directory = temporary_directory();
        let mut executor = StorageExecutor::open_at(&directory).expect("启动失败映射存储线程失败");
        let client = executor.client();
        executor.begin_closing().expect("建立存储关闭态失败");
        let (sender, receiver) = clear_history_mutation_channel();
        let (result_sender, result_receiver) = sync_channel(1);
        let worker = start_clear_history_mutation_worker(client, None, receiver, move |result| {
            result_sender.send(result).is_ok()
        })
        .expect("启动失败映射清空 worker 失败");

        let request = clear_request(21);
        sender.try_submit(request).expect("提交失败映射请求失败");
        sender.close();
        worker.join().expect("失败映射清空 worker 异常退出");
        let result = result_receiver.recv().expect("未收到失败映射结果");
        assert_eq!(result.mutation_token, request.mutation_token);
        assert_eq!(result.panel_generation, request.panel_generation);
        assert_eq!(result.scope, request.scope);
        assert_eq!(
            result.outcome,
            Err(ClearHistoryMutationFailure::StorageUnavailable)
        );

        executor.finish_shutdown().expect("完成存储关闭失败");
        fs::remove_dir_all(directory).expect("清理失败映射目录失败");
    }

    /// UI 结果接收端消失不能阻塞清空事务或 worker 退出。
    #[test]
    fn clear_unpinned_emit_false_does_not_undo_transaction() {
        let directory = temporary_directory();
        let executor = StorageExecutor::open_at(&directory).expect("启动投递失败存储线程失败");
        let inserted = executor
            .upsert_text(TextUpsertInput {
                content_hash: [32; 32],
                text_content: "清空投递失败正文".to_owned(),
                preview_text: "清空投递失败预览".to_owned(),
                source_exe: None,
                source_app: None,
                copied_at: 1,
            })
            .expect("写入清空投递失败记录失败");
        let (sender, receiver) = clear_history_mutation_channel();
        let worker =
            start_clear_history_mutation_worker(executor.client(), None, receiver, |_result| false)
                .expect("启动投递失败清空 worker 失败");
        sender
            .try_submit(clear_request(31))
            .expect("提交投递失败清空请求失败");
        sender.close();
        worker.join().expect("投递失败清空 worker 异常退出");
        assert!(executor
            .get_history_payload(inserted.id)
            .expect("读取投递失败后的记录失败")
            .is_none());

        drop(executor);
        fs::remove_dir_all(directory).expect("清理投递失败清空目录失败");
    }

    /// 同一个 worker 必须按显式 All 范围删除收藏文本和非文本行，并原样回显范围。
    #[test]
    fn clear_history_worker_routes_explicit_all_scope() {
        let directory = temporary_directory();
        let database_path = directory.join("clipboard.db");
        let executor = StorageExecutor::open_at(&directory).expect("启动全量桥存储线程失败");
        let pinned = executor
            .upsert_text(TextUpsertInput {
                content_hash: [33; 32],
                text_content: "收藏正文".to_owned(),
                preview_text: "收藏预览".to_owned(),
                source_exe: None,
                source_app: None,
                copied_at: 1,
            })
            .expect("写入收藏测试文本失败");
        executor
            .set_history_pinned(crate::storage::SetPinnedInput {
                id: pinned.id,
                content_hash: pinned.content_hash,
                is_pinned: true,
            })
            .expect("设置桥测试收藏失败");
        {
            let connection = Connection::open(&database_path).expect("打开桥测试数据库失败");
            connection
                .execute(
                    "INSERT INTO clipboard_items
                     (item_type, preview_text, content_hash, is_pinned, created_at, copied_at)
                     VALUES ('binary', '非文本', ?1, 0, 2, 2)",
                    params![[34_u8; 32].as_slice()],
                )
                .expect("写入桥测试图片行失败");
        }
        let (sender, receiver) = clear_history_mutation_channel();
        let (result_sender, result_receiver) = sync_channel(1);
        let worker =
            start_clear_history_mutation_worker(executor.client(), None, receiver, move |result| {
                result_sender.send(result).is_ok()
            })
            .expect("启动双范围清空 worker 失败");

        let request = clear_request_for_scope(41, ClearHistoryScope::All);
        sender.try_submit(request).expect("提交全量清空请求失败");
        sender.close();
        worker.join().expect("双范围清空 worker 异常退出");

        let result = result_receiver.recv().expect("未收到全量清空结果");
        assert_eq!(result.scope, ClearHistoryScope::All);
        let success = result.outcome.expect("全量范围事务不应失败");
        assert_eq!(success.deleted_count, 2);
        assert_eq!(
            executor
                .status()
                .expect("读取全量桥结果失败")
                .clipboard_item_count,
            0
        );

        drop(executor);
        fs::remove_dir_all(directory).expect("清理全量桥测试目录失败");
    }
}

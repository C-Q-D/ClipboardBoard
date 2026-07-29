//! 此模块提供收藏状态变更的有界异步命令桥。
//!
//! UI 线程只执行非阻塞提交；单一后台 worker 顺序调用受控存储客户端。关闭会拒绝新请求，
//! 但保留并排空已经接受的请求，确保“点击收藏后立即退出”不会丢失已承诺的事务。

use std::{
    io,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};

use crate::storage::{SetPinnedInput, StorageClient, StorageError};

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

#[cfg(test)]
mod tests {
    //! 此测试模块验证单槽边界、关闭排空和有限结果映射。

    use std::{
        fs,
        path::PathBuf,
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc::sync_channel,
        },
    };

    use super::{
        pin_mutation_channel, start_pin_mutation_worker, PinMutationFailure, PinMutationRequest,
        PinMutationSubmitError,
    };
    use crate::storage::{StorageExecutor, TextUpsertInput};

    static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    /// 创建当前测试独占的 SQLite 目录。
    fn temporary_directory() -> PathBuf {
        let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "clipboard-board-fav02-{}-{sequence}",
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
}

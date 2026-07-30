//! 此文件实现配置专用工作线程、有界命令入口和可线性化关闭生命周期。

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
};

use super::{default_config_directory, persistence, AppSettings, SettingsError, SettingsSnapshot};

/// 配置命令队列容量；设置低频写入不需要无界积压。
const SETTINGS_QUEUE_CAPACITY: usize = 4;

/// worker 命令仅暴露快照、保存和关闭三种行为。
enum SettingsCommand {
    /// 返回当前权威内存快照。
    Snapshot {
        /// 单次同步回执。
        reply: SyncSender<Result<SettingsSnapshot, SettingsError>>,
    },
    /// 比较 revision 后持久化新配置。
    Save {
        /// 调用方最后观察到的 revision。
        expected_revision: u64,
        /// 待保存配置。
        settings: AppSettings,
        /// 单次同步回执。
        reply: SyncSender<Result<SettingsSnapshot, SettingsError>>,
    },
    /// 排在全部已准入命令后关闭 worker。
    Shutdown {
        /// worker 接收关闭命令的回执。
        reply: SyncSender<Result<(), SettingsError>>,
    },
    /// 测试专用：阻塞 worker 以确定性填满命令队列。
    #[cfg(test)]
    TestBlock {
        /// 通知测试 worker 已进入栅栏。
        entered: SyncSender<()>,
        /// 测试释放 worker 的接收端。
        release: Receiver<()>,
        /// 栅栏完成回执。
        reply: SyncSender<Result<(), SettingsError>>,
    },
    /// 测试专用：在成功保存前丢弃业务回执接收端。
    #[cfg(test)]
    TestSaveWithDroppedReply {
        /// 期望 revision。
        expected_revision: u64,
        /// 待保存配置。
        settings: AppSettings,
        /// 已被测试丢弃接收端的业务回执。
        reply: SyncSender<Result<SettingsSnapshot, SettingsError>>,
        /// worker 完成业务回执尝试后的确定性通知。
        completed: SyncSender<()>,
    },
    /// 测试专用：把进程内 revision 推进到指定值。
    #[cfg(test)]
    TestSetRevision {
        /// 新 revision。
        revision: u64,
        /// 设置完成回执。
        reply: SyncSender<()>,
    },
    /// 测试专用：让 worker 在未发送业务回执前 panic。
    #[cfg(test)]
    TestPanic,
}

/// 共享生命周期状态。
#[derive(Clone, Copy, Eq, PartialEq)]
enum SettingsLifecycle {
    /// 接受新命令。
    Open,
    /// 已拒绝新命令，正在排空。
    Closing,
    /// worker 已回收。
    Closed,
}

/// 所有克隆客户端共享的准入门禁。
struct SettingsShared {
    /// 有界命令发送端。
    sender: SyncSender<SettingsCommand>,
    /// 生命周期检查和入队共用的互斥锁。
    lifecycle: Mutex<SettingsLifecycle>,
    /// 先于锁竞争发布的关闭意图。
    closing_intent: AtomicBool,
}

/// 可克隆配置客户端；不拥有 worker join 权限。
#[derive(Clone)]
pub struct SettingsClient {
    /// 共享准入门禁。
    shared: Arc<SettingsShared>,
}

/// 配置工作线程所有者；负责建立关闭点并 join。
pub struct SettingsWorker {
    /// 共享命令入口。
    shared: Arc<SettingsShared>,
    /// 不可克隆线程句柄。
    worker: Option<JoinHandle<()>>,
    /// 显式配置目录，仅供诊断和测试。
    config_directory: PathBuf,
}

impl SettingsWorker {
    /// 使用默认 LOCALAPPDATA 配置目录启动 worker。
    pub fn start() -> Result<Self, SettingsError> {
        Self::start_at(default_config_directory()?)
    }

    /// 使用显式目录启动 worker；测试必须使用此入口。
    pub fn start_at(config_directory: impl AsRef<Path>) -> Result<Self, SettingsError> {
        let config_directory = config_directory.as_ref().to_path_buf();
        let worker_directory = config_directory.clone();
        let (sender, receiver) = sync_channel(SETTINGS_QUEUE_CAPACITY);
        let (ready_sender, ready_receiver) = sync_channel(1);
        let worker = thread::Builder::new()
            .name("clipboard-board-settings".to_owned())
            .spawn(move || worker_main(worker_directory, receiver, ready_sender))?;

        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                shared: Arc::new(SettingsShared {
                    sender,
                    lifecycle: Mutex::new(SettingsLifecycle::Open),
                    closing_intent: AtomicBool::new(false),
                }),
                worker: Some(worker),
                config_directory,
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                if worker.join().is_err() {
                    Err(SettingsError::WorkerPanicked)
                } else {
                    Err(SettingsError::ChannelClosed)
                }
            }
        }
    }

    /// 返回显式配置目录，不执行文件 IO。
    pub fn config_directory(&self) -> &Path {
        &self.config_directory
    }

    /// 签发共享同一 worker 的受控客户端。
    pub fn client(&self) -> SettingsClient {
        SettingsClient {
            shared: Arc::clone(&self.shared),
        }
    }

    /// 建立关闭线性化点；返回后所有客户端拒绝新命令。
    pub fn begin_closing(&mut self) -> Result<(), SettingsError> {
        self.shared.closing_intent.store(true, Ordering::Release);
        let mut lifecycle = self
            .shared
            .lifecycle
            .lock()
            .map_err(|_| SettingsError::ChannelClosed)?;
        match *lifecycle {
            SettingsLifecycle::Open => {
                *lifecycle = SettingsLifecycle::Closing;
                Ok(())
            }
            SettingsLifecycle::Closing => Err(SettingsError::SettingsClosing),
            SettingsLifecycle::Closed => Err(SettingsError::SettingsClosed),
        }
    }

    /// 排队关闭命令并 join；调用前必须先 begin_closing。
    pub fn finish_shutdown(&mut self) -> Result<(), SettingsError> {
        {
            let lifecycle = self
                .shared
                .lifecycle
                .lock()
                .map_err(|_| SettingsError::ChannelClosed)?;
            match *lifecycle {
                SettingsLifecycle::Open => return Err(SettingsError::ShutdownNotBegun),
                SettingsLifecycle::Closed => return Err(SettingsError::SettingsClosed),
                SettingsLifecycle::Closing => {}
            }
        }
        let (reply_sender, reply_receiver) = sync_channel(1);
        let send_result = self
            .shared
            .sender
            .send(SettingsCommand::Shutdown {
                reply: reply_sender,
            })
            .map_err(|_| SettingsError::ChannelClosed);
        let reply_result = send_result.and_then(|()| {
            reply_receiver
                .recv()
                .unwrap_or(Err(SettingsError::ChannelClosed))
        });
        let join_result = self
            .worker
            .take()
            .ok_or(SettingsError::SettingsClosed)?
            .join()
            .map_err(|_| SettingsError::WorkerPanicked);
        if let Ok(mut lifecycle) = self.shared.lifecycle.lock() {
            *lifecycle = SettingsLifecycle::Closed;
        }
        join_result?;
        reply_result
    }
}

impl Drop for SettingsWorker {
    /// 尽力回收 worker；显式关闭仍是获取错误结果的唯一方式。
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.begin_closing();
            let _ = self.finish_shutdown();
        }
    }
}

impl SettingsClient {
    /// 在同一生命周期锁内检查 Open 并完成有界队列入队。
    fn submit(&self, command: SettingsCommand) -> Result<(), SettingsError> {
        if self.shared.closing_intent.load(Ordering::Acquire) {
            return Err(self.lifecycle_error());
        }
        let lifecycle = self
            .shared
            .lifecycle
            .lock()
            .map_err(|_| SettingsError::ChannelClosed)?;
        match *lifecycle {
            SettingsLifecycle::Open if !self.shared.closing_intent.load(Ordering::Acquire) => self
                .shared
                .sender
                .send(command)
                .map_err(|_| SettingsError::ChannelClosed),
            SettingsLifecycle::Open | SettingsLifecycle::Closing => {
                Err(SettingsError::SettingsClosing)
            }
            SettingsLifecycle::Closed => Err(SettingsError::SettingsClosed),
        }
    }

    /// 根据共享状态返回稳定关闭错误。
    fn lifecycle_error(&self) -> SettingsError {
        match self.shared.lifecycle.lock() {
            Ok(lifecycle) if *lifecycle == SettingsLifecycle::Closed => {
                SettingsError::SettingsClosed
            }
            _ => SettingsError::SettingsClosing,
        }
    }

    /// 读取当前权威内存快照，不访问磁盘。
    ///
    /// 此同步方法可能阻塞等待有界队列和 worker 回执，禁止在 Slint 回调中直接调用。
    pub fn snapshot(&self) -> Result<SettingsSnapshot, SettingsError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        self.submit(SettingsCommand::Snapshot {
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .unwrap_or(Err(SettingsError::ChannelClosed))
    }

    /// 比较 revision 后保存，成功返回新快照。
    ///
    /// 此同步方法可能阻塞等待队列、文件 IO 和 worker 回执，禁止在 Slint 回调中直接调用。
    pub fn save(
        &self,
        expected_revision: u64,
        settings: AppSettings,
    ) -> Result<SettingsSnapshot, SettingsError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        self.submit(SettingsCommand::Save {
            expected_revision,
            settings,
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .unwrap_or(Err(SettingsError::OutcomeUnknown))
    }

    /// 测试专用：阻塞 worker 直到 release 到达。
    #[cfg(test)]
    fn test_block(
        &self,
        entered: SyncSender<()>,
        release: Receiver<()>,
    ) -> Result<(), SettingsError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        self.submit(SettingsCommand::TestBlock {
            entered,
            release,
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .unwrap_or(Err(SettingsError::ChannelClosed))
    }

    /// 测试专用：丢弃保存响应端并等待 worker 完成提交。
    #[cfg(test)]
    fn test_save_with_dropped_reply(
        &self,
        expected_revision: u64,
        settings: AppSettings,
    ) -> Result<(), SettingsError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        let (completed_sender, completed_receiver) = sync_channel(1);
        self.submit(SettingsCommand::TestSaveWithDroppedReply {
            expected_revision,
            settings,
            reply: reply_sender,
            completed: completed_sender,
        })?;
        drop(reply_receiver);
        completed_receiver
            .recv()
            .map_err(|_| SettingsError::ChannelClosed)
    }

    /// 测试专用：设置 revision 耗尽边界。
    #[cfg(test)]
    fn test_set_revision(&self, revision: u64) -> Result<(), SettingsError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        self.submit(SettingsCommand::TestSetRevision {
            revision,
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .map_err(|_| SettingsError::ChannelClosed)
    }

    /// 测试专用：触发 worker panic，供 join 错误优先级测试。
    #[cfg(test)]
    fn test_panic(&self) -> Result<(), SettingsError> {
        self.submit(SettingsCommand::TestPanic)
    }

    /// 测试专用：暴露取得生命周期锁和完成入队两个确定性时点。
    #[cfg(test)]
    fn test_snapshot_with_admission(
        &self,
        gate_entered: SyncSender<()>,
        admitted: SyncSender<()>,
    ) -> Result<SettingsSnapshot, SettingsError> {
        let (reply_sender, reply_receiver) = sync_channel(1);
        let lifecycle = self
            .shared
            .lifecycle
            .lock()
            .map_err(|_| SettingsError::ChannelClosed)?;
        if *lifecycle != SettingsLifecycle::Open {
            return Err(if *lifecycle == SettingsLifecycle::Closed {
                SettingsError::SettingsClosed
            } else {
                SettingsError::SettingsClosing
            });
        }
        gate_entered
            .send(())
            .map_err(|_| SettingsError::ChannelClosed)?;
        self.shared
            .sender
            .send(SettingsCommand::Snapshot {
                reply: reply_sender,
            })
            .map_err(|_| SettingsError::ChannelClosed)?;
        admitted
            .send(())
            .map_err(|_| SettingsError::ChannelClosed)?;
        drop(lifecycle);
        reply_receiver
            .recv()
            .unwrap_or(Err(SettingsError::ChannelClosed))
    }
}

/// worker 首先完成唯一磁盘加载，再进入串行命令循环。
fn worker_main(
    directory: PathBuf,
    receiver: Receiver<SettingsCommand>,
    ready: SyncSender<Result<(), SettingsError>>,
) {
    let mut loaded = match persistence::load(&directory) {
        Ok(loaded) => {
            let _ = ready.send(Ok(()));
            loaded
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    while let Ok(command) = receiver.recv() {
        match command {
            SettingsCommand::Snapshot { reply } => {
                let _ = reply.send(Ok(loaded.snapshot.clone()));
            }
            SettingsCommand::Save {
                expected_revision,
                settings,
                reply,
            } => {
                let result =
                    persistence::save(&directory, &mut loaded, expected_revision, settings);
                // Win32 成功后状态已经提交；回执断开不得回滚。
                let _ = reply.send(result);
            }
            SettingsCommand::Shutdown { reply } => {
                let _ = reply.send(Ok(()));
                break;
            }
            #[cfg(test)]
            SettingsCommand::TestBlock {
                entered,
                release,
                reply,
            } => {
                let _ = entered.send(());
                let result = release.recv().map_err(|_| SettingsError::ChannelClosed);
                let _ = reply.send(result);
            }
            #[cfg(test)]
            SettingsCommand::TestSaveWithDroppedReply {
                expected_revision,
                settings,
                reply,
                completed,
            } => {
                let result =
                    persistence::save(&directory, &mut loaded, expected_revision, settings);
                let _ = reply.send(result);
                let _ = completed.send(());
            }
            #[cfg(test)]
            SettingsCommand::TestSetRevision { revision, reply } => {
                loaded.snapshot = SettingsSnapshot::new(
                    loaded.snapshot.settings().clone(),
                    loaded.snapshot.source(),
                    revision,
                );
                let _ = reply.send(());
            }
            #[cfg(test)]
            SettingsCommand::TestPanic => panic!("ATOM-44 测试注入 worker panic"),
        }
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证回执丢失、revision 耗尽和关闭准入的并发语义。

    use std::{
        fs,
        path::PathBuf,
        process,
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc::sync_channel,
            OnceLock,
        },
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{SettingsError, SettingsWorker, SETTINGS_QUEUE_CAPACITY};
    use crate::settings::{AppSettings, HistorySettings, SettingsLoadSource};

    /// 测试临时根 token。
    static NEXT_TEST_TOKEN: AtomicU64 = AtomicU64::new(0);

    /// 返回进程级测试 nonce。
    fn test_nonce() -> u128 {
        static NONCE: OnceLock<u128> = OnceLock::new();
        *NONCE.get_or_init(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
                ^ u128::from(process::id())
        })
    }

    /// 使用 create_dir 在固定 64 次内独占创建配置测试根。
    fn temporary_directory(label: &str) -> PathBuf {
        for _ in 0..64 {
            let token = NEXT_TEST_TOKEN.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "clipboard-board-atom44-settings-core-{:032x}-{}-{token}-{label}",
                test_nonce(),
                process::id()
            ));
            match fs::create_dir(&directory) {
                Ok(()) => return directory,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("创建配置隔离根失败：{error}"),
            }
        }
        panic!("配置隔离根在有界重试内持续碰撞");
    }

    /// 构造指定历史上限的合法配置。
    fn settings(max_items: u32) -> AppSettings {
        AppSettings {
            history: HistorySettings {
                max_items,
                ..HistorySettings::default()
            },
        }
    }

    /// 显式关闭并清理临时根。
    fn shutdown(mut worker: SettingsWorker, directory: PathBuf) {
        worker.begin_closing().expect("建立关闭点失败");
        worker.finish_shutdown().expect("回收 worker 失败");
        fs::remove_dir_all(directory).expect("清理配置测试根失败");
    }

    /// Win32 成功后丢失回执仍会提交新快照，旧 revision 不得重试。
    #[test]
    fn dropped_success_reply_is_reconciled_through_snapshot() {
        let directory = temporary_directory("dropped-reply");
        let worker = SettingsWorker::start_at(&directory).expect("启动 worker 失败");
        let client = worker.client();
        client
            .test_save_with_dropped_reply(0, settings(2_222))
            .expect("等待丢失回执保存完成失败");
        let snapshot = client.snapshot().expect("对账快照失败");
        assert_eq!(snapshot.source(), SettingsLoadSource::Primary);
        assert_eq!(snapshot.revision(), 1);
        assert_eq!(snapshot.settings().history.max_items, 2_222);
        assert!(matches!(
            client.save(0, settings(3_333)),
            Err(SettingsError::RevisionConflict {
                expected: 0,
                actual: 1
            })
        ));
        shutdown(worker, directory);
    }

    /// revision 耗尽必须在创建配置文件前被拒绝。
    #[test]
    fn exhausted_revision_is_rejected_before_io() {
        let directory = temporary_directory("revision-exhausted");
        let worker = SettingsWorker::start_at(&directory).expect("启动 worker 失败");
        let client = worker.client();
        client
            .test_set_revision(u64::MAX)
            .expect("设置 revision 边界失败");
        assert!(matches!(
            client.save(u64::MAX, settings(2_222)),
            Err(SettingsError::RevisionExhausted)
        ));
        assert!(!directory.join("settings.json").exists());
        shutdown(worker, directory);
    }

    /// 满队列时已准入命令先排空，关闭意图拒绝后续命令且不死锁。
    #[test]
    fn full_queue_drains_admitted_commands_before_closing() {
        let directory = temporary_directory("full-queue");
        let worker = SettingsWorker::start_at(&directory).expect("启动 worker 失败");
        let (entered_sender, entered_receiver) = sync_channel(1);
        let (release_sender, release_receiver) = sync_channel(1);
        let blocking_client = worker.client();
        let blocker =
            thread::spawn(move || blocking_client.test_block(entered_sender, release_receiver));
        entered_receiver.recv().expect("worker 未进入阻塞栅栏");

        let mut queued = Vec::new();
        for _ in 0..SETTINGS_QUEUE_CAPACITY {
            let client = worker.client();
            let (gate_sender, gate_receiver) = sync_channel(1);
            let (admitted_sender, admitted_receiver) = sync_channel(1);
            let handle = thread::spawn(move || {
                client.test_snapshot_with_admission(gate_sender, admitted_sender)
            });
            gate_receiver.recv().expect("排队客户端未取得门禁");
            admitted_receiver.recv().expect("排队客户端未完成入队");
            queued.push(handle);
        }

        let extra_client = worker.client();
        let (extra_gate_sender, extra_gate_receiver) = sync_channel(1);
        let (extra_admitted_sender, extra_admitted_receiver) = sync_channel(1);
        let extra = thread::spawn(move || {
            extra_client.test_snapshot_with_admission(extra_gate_sender, extra_admitted_sender)
        });
        extra_gate_receiver.recv().expect("额外客户端未取得门禁");
        let shared = std::sync::Arc::clone(&worker.shared);
        let closing = thread::spawn(move || {
            let mut worker = worker;
            worker.begin_closing().expect("进入 Closing 失败");
            worker
        });
        while !shared.closing_intent.load(Ordering::Acquire) {
            thread::yield_now();
        }
        release_sender.send(()).expect("释放 worker 失败");
        blocker.join().unwrap().expect("阻塞命令失败");
        extra_admitted_receiver
            .recv()
            .expect("额外已准入命令未完成入队");
        for handle in queued {
            handle.join().unwrap().expect("已排队快照失败");
        }
        extra.join().unwrap().expect("额外已准入快照失败");
        let mut worker = closing.join().expect("关闭线程 panic");
        worker.finish_shutdown().expect("完成关闭失败");
        fs::remove_dir_all(directory).expect("清理满队列根失败");
    }

    /// 关闭线性化后 snapshot/save 均拒绝，完成关闭后稳定返回 Closed。
    #[test]
    fn clients_reject_snapshot_and_save_after_closing() {
        let directory = temporary_directory("closing-rejection");
        let mut worker = SettingsWorker::start_at(&directory).unwrap();
        let client = worker.client();
        worker.begin_closing().unwrap();
        assert!(matches!(
            client.snapshot(),
            Err(SettingsError::SettingsClosing)
        ));
        assert!(matches!(
            client.save(0, settings(2_222)),
            Err(SettingsError::SettingsClosing)
        ));
        worker.finish_shutdown().unwrap();
        assert!(matches!(
            client.snapshot(),
            Err(SettingsError::SettingsClosed)
        ));
        assert!(matches!(
            client.save(0, settings(2_222)),
            Err(SettingsError::SettingsClosed)
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    /// worker panic 时 finish_shutdown 优先返回 WorkerPanicked，而非通道断开。
    #[test]
    fn worker_panic_is_reported_before_shutdown_reply_failure() {
        let directory = temporary_directory("panic-priority");
        let mut worker = SettingsWorker::start_at(&directory).unwrap();
        worker.client().test_panic().unwrap();
        worker.begin_closing().unwrap();
        assert!(matches!(
            worker.finish_shutdown(),
            Err(SettingsError::WorkerPanicked)
        ));
        fs::remove_dir_all(directory).unwrap();
    }
}

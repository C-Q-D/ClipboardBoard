//! 此二进制入口负责创建主窗口并启动 Slint 事件循环。
//!
//! 当前入口先完成单实例判定，再创建 UI、初始化 SQLite、绑定弱窗口并启动热键、剪贴板
//! 结果泵和托盘消息线程。应用通过鼠标按钮显式写回系统剪贴板，不注入粘贴按键；
//! 图片历史和完整设置能力仍由后续原子接入。

#[cfg(windows)]
use clipboard_board::app::post_ui_event;
#[cfg(windows)]
use clipboard_board::app::{
    bind_app_window, bind_clear_history_mutation_sender, bind_copy_request_inbox,
    bind_delete_mutation_sender, bind_history_query_bridge, bind_pin_mutation_sender,
    bind_thumbnail_loader_sender,
};
#[cfg(windows)]
use clipboard_board::clipboard::{ClipboardCaptureInbox, ClipboardWriteExpectationStore};
#[cfg(windows)]
use clipboard_board::command::UiEvent;
#[cfg(windows)]
use clipboard_board::diagnostics::{self, DiagnosticEvent, ThreadState};
#[cfg(windows)]
use clipboard_board::history_bridge::{run_clipboard_pump_with_source_policy, ImageCaptureContext};
#[cfg(windows)]
use clipboard_board::history_mutation::{
    clear_history_mutation_channel, delete_mutation_channel, pin_mutation_channel,
    start_clear_history_mutation_worker, start_delete_mutation_worker, start_pin_mutation_worker,
    ClearHistoryMutationSender, DeleteMutationSender, PinMutationSender,
};
#[cfg(windows)]
use clipboard_board::history_query::{
    history_request_channel, history_result_channel, start_history_query_worker,
    HistoryRequestSender, HistoryResultReceiver,
};
#[cfg(windows)]
use clipboard_board::history_restore::load_startup_snapshot;
#[cfg(windows)]
use clipboard_board::image_pipeline::ImageWorker;
#[cfg(windows)]
use clipboard_board::image_storage::{parse_image_storage_preference, prepare_image_storage};
#[cfg(windows)]
use clipboard_board::platform::windows::startup::StartupSettingsOwner;
#[cfg(windows)]
use clipboard_board::platform::windows::{acquire_or_activate, HotkeyManager, SingleInstanceRole};
#[cfg(windows)]
use clipboard_board::privacy::{PrivacyRuntimeOwner, SettingsClientRpcAdapter, SystemPauseClock};
#[cfg(windows)]
use clipboard_board::settings::SettingsWorker;
#[cfg(windows)]
use clipboard_board::storage::{StorageClient, StorageExecutor};
#[cfg(windows)]
use clipboard_board::thumbnail_loader::ThumbnailLoader;
#[cfg(windows)]
use slint::ComponentHandle;
#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use std::thread::{self, JoinHandle};

/// Windows 运行时的统一资源回收协调器。
#[cfg(windows)]
struct RuntimeCleanup {
    /// 开机启动所有者必须先于 SettingsWorker 字段销毁，避免其 Drop 访问已关闭的 client。
    startup: Option<StartupSettingsOwner>,
    /// 尚未移交给 PrivacyRuntimeOwner 的 SettingsWorker。
    settings: Option<SettingsWorker>,
    /// 持有 controller、RPC helper 和 SettingsWorker 的隐私运行时。
    privacy: Option<PrivacyRuntimeOwner>,
    /// SQLite 执行器，必须在所有业务 worker 完成后关闭。
    storage: Option<StorageExecutor>,
    /// 图片资源拥有线程。
    image_worker: Option<ImageWorker>,
    /// 缩略图读取线程。
    thumbnail_loader: Option<ThumbnailLoader>,
    /// 全局热键消息线程。
    hotkey: Option<HotkeyManager>,
    /// 剪贴板捕获结果泵。
    capture_pump: Option<JoinHandle<()>>,
    /// 历史查询 worker。
    history_query_worker: Option<JoinHandle<()>>,
    /// 收藏 mutation worker。
    pin_mutation_worker: Option<JoinHandle<()>>,
    /// 删除 mutation worker。
    delete_mutation_worker: Option<JoinHandle<()>>,
    /// 清空历史 mutation worker。
    clear_history_worker: Option<JoinHandle<()>>,
    /// 捕获结果桥，先于 capture_pump join 关闭以唤醒等待者。
    capture_inbox: Option<ClipboardCaptureInbox>,
    /// 历史查询请求入口。
    history_requests: Option<HistoryRequestSender>,
    /// 历史查询结果入口。
    history_results: Option<HistoryResultReceiver>,
    /// 收藏请求入口。
    pin_mutations: Option<PinMutationSender>,
    /// 删除请求入口。
    delete_mutations: Option<DeleteMutationSender>,
    /// 清空历史请求入口。
    clear_history_mutations: Option<ClearHistoryMutationSender>,
}

#[cfg(windows)]
impl RuntimeCleanup {
    /// 创建空的资源槽；尚未启动的阶段保持 `None`，可安全走同一失败清理路径。
    fn new() -> Self {
        Self {
            startup: None,
            settings: None,
            privacy: None,
            storage: None,
            image_worker: None,
            thumbnail_loader: None,
            hotkey: None,
            capture_pump: None,
            history_query_worker: None,
            pin_mutation_worker: None,
            delete_mutation_worker: None,
            clear_history_worker: None,
            capture_inbox: None,
            history_requests: None,
            history_results: None,
            pin_mutations: None,
            delete_mutations: None,
            clear_history_mutations: None,
        }
    }

    /// 按固定逆序关闭所有已创建资源；每个 `Option` 槽只 take 一次。
    fn stop(&mut self) -> Result<(), String> {
        let mut first_error = None;
        let mut record = |result: Result<(), String>| {
            if first_error.is_none() {
                if let Err(error) = result {
                    first_error = Some(error);
                }
            }
        };

        // 先关闭所有 UI/业务入口，保证后续 join 不会再接收新请求。
        if let Some(sender) = self.clear_history_mutations.take() {
            sender.close();
        }
        if let Some(sender) = self.delete_mutations.take() {
            sender.close();
        }
        if let Some(sender) = self.pin_mutations.take() {
            sender.close();
        }
        if let Some(sender) = self.history_results.take() {
            sender.close();
        }
        if let Some(sender) = self.history_requests.take() {
            sender.close();
        }
        // 热键线程先注销 listener 并退出消息循环，随后关闭 inbox 唤醒捕获结果泵。
        if let Some(hotkey) = self.hotkey.take() {
            record(hotkey.stop().map_err(|error| error.to_string()));
        }
        if let Some(inbox) = self.capture_inbox.take() {
            inbox.close();
        }
        // inbox 关闭后再 join 捕获泵和业务 worker，避免结果泵永久等待唤醒令牌。
        if let Some(worker) = self.capture_pump.take() {
            record(join_worker(worker, "剪贴板结果泵线程异常退出"));
        }
        if let Some(worker) = self.history_query_worker.take() {
            record(join_worker(worker, "历史查询线程异常退出"));
        }
        if let Some(worker) = self.pin_mutation_worker.take() {
            record(join_worker(worker, "收藏变更线程异常退出"));
        }
        if let Some(worker) = self.delete_mutation_worker.take() {
            record(join_worker(worker, "删除变更线程异常退出"));
        }
        if let Some(worker) = self.clear_history_worker.take() {
            record(join_worker(worker, "清空历史线程异常退出"));
        }
        if let Some(loader) = self.thumbnail_loader.take() {
            record(loader.stop().map_err(|error| error.to_string()));
        }
        if let Some(worker) = self.image_worker.take() {
            record(worker.stop().map_err(|error| error.to_string()));
        }

        // StartupSettingsOwner 只持有 SettingsClient clone，必须先于拥有 SettingsWorker
        // 的 PrivacyRuntimeOwner 关闭，避免在途事务访问已关闭的 worker。
        if let Some(mut startup) = self.startup.take() {
            let shutdown_result = startup
                .begin_closing()
                .and_then(|()| startup.finish_shutdown())
                .map_err(|error| error.to_string());
            if let Err(error) = shutdown_result {
                // 未完成对账时不能继续关闭 SettingsWorker；保留 owner 供下一次
                // stop/Retry 重试，避免后台线程继续使用已停止的 SettingsClient。
                self.startup = Some(startup);
                record(Err(error));
                return first_error.map_or(Ok(()), Err);
            }
        }
        // ClipboardIO、图片和业务线程都已停止后，才关闭 privacy controller/RPC/Settings。
        if let Some(runtime) = self.privacy.take() {
            record(runtime.stop().map_err(|error| error.to_string()));
        }
        if let Some(mut settings) = self.settings.take() {
            record(
                settings
                    .begin_closing()
                    .and_then(|()| settings.finish_shutdown())
                    .map_err(|error| error.to_string()),
            );
        }
        if let Some(mut storage) = self.storage.take() {
            record(
                storage
                    .begin_closing()
                    .and_then(|()| storage.finish_shutdown())
                    .map_err(|error| error.to_string()),
            );
        }

        first_error.map_or(Ok(()), Err)
    }
}

#[cfg(windows)]
impl Drop for RuntimeCleanup {
    /// 启动阶段任意 `?` 早退也复用正常退出的同一资源收敛顺序。
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Join 一个已停止入口的业务线程，并将 panic 压缩为固定中文错误。
#[cfg(windows)]
fn join_worker(worker: JoinHandle<()>, message: &'static str) -> Result<(), String> {
    worker.join().map_err(|_| message.to_owned())
}

/// 启动 ClipboardBoard 的最小桌面窗口。
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 单实例检查必须早于 UI、SQLite 和热键初始化，二次进程只负责通知主实例。
    let _instance_guard = match acquire_or_activate()? {
        SingleInstanceRole::Primary(guard) => guard,
        SingleInstanceRole::Secondary => return Ok(()),
    };

    // 仅主实例初始化日志，并且早于 Win32 消息线程；事件只经过隐私字段白名单序列化。
    diagnostics::init();
    diagnostics::emit(DiagnosticEvent::thread_state(ThreadState::Starting));
    let mut cleanup = RuntimeCleanup::new();

    let window = clipboard_board::create_app_window()?;
    bind_app_window(&window);
    window.hide()?;

    // 配置与隐私运行时必须先于剪贴板 listener 建立，确保首个更新也经过暂停门禁。
    cleanup.settings = Some(SettingsWorker::start()?);
    let initial_settings = cleanup
        .settings
        .as_ref()
        .expect("SettingsWorker 已放入清理槽")
        .client()
        .snapshot()?;
    // 快捷键事务 worker 需要独立 SettingsClient；主线程只读取一次已验证启动快照。
    let hotkey_settings_client = cleanup
        .settings
        .as_ref()
        .expect("SettingsWorker 已放入清理槽")
        .client();
    // 来源记录策略在启动时冻结；运行中不让结果泵同步读取配置文件。
    let capture_source_app = initial_settings.settings().history.capture_source_app;
    // 设置快照已经过保存/加载校验，但启动仍复用同一个无副作用解析器，保证
    // 配置语义与图片 capability 初始化永远使用同一条路径规则。
    let image_storage_preference = parse_image_storage_preference(
        initial_settings
            .settings()
            .history
            .image_storage_root
            .as_deref(),
    )?;
    let settings = cleanup
        .settings
        .take()
        .expect("SettingsWorker 启动阶段只移交一次");
    let startup_owner = StartupSettingsOwner::start(settings.client())?;
    let startup_sender = startup_owner.sender();
    cleanup.startup = Some(startup_owner);
    // 启动阶段先完成一次只读对账；错配只展示，禁止自动修复后再继续初始化主程序。
    if let Ok(reply) = startup_sender.try_query() {
        if let Ok(result) = reply.recv() {
            // 只把稳定结果枚举交给 UI，禁止启动阶段把注册表路径或底层错误正文写入日志。
            let _ = post_ui_event(UiEvent::StartupStatus {
                transaction_id: result.transaction_id,
                generation: result.generation,
                kind: result.kind,
            });
        }
    }
    let settings_adapter = SettingsClientRpcAdapter::new(settings.client());
    let privacy_runtime = PrivacyRuntimeOwner::start_with(
        settings,
        initial_settings.clone(),
        Box::new(settings_adapter),
        Arc::new(SystemPauseClock::new()),
    )?;
    let privacy_gate = privacy_runtime.gate();
    let privacy_sender = privacy_runtime.sender();
    cleanup.privacy = Some(privacy_runtime);

    // 存储执行器必须在热键和剪贴板监听前唯一创建，并先完成启动恢复。
    cleanup.storage = Some(StorageExecutor::open()?);
    let startup_snapshot = load_startup_snapshot(
        cleanup
            .storage
            .as_mut()
            .expect("StorageExecutor 已放入清理槽"),
    )?;
    post_ui_event(UiEvent::ReplaceSnapshot(startup_snapshot))?;
    let prepared_images = prepare_image_storage(image_storage_preference)?;
    if let Some(fallback) = prepared_images.fallback() {
        // 回退诊断只记录稳定分类和操作，不记录请求目录或实际完整路径；后续
        // ImageWorker 从同一个 capability 快照取得实际生效根，禁止继续使用失败请求。
        eprintln!(
            "图片存储目录回退：kind={:?}, operation={}",
            fallback.reason().kind(),
            fallback.reason().operation()
        );
    }
    let image_worker = ImageWorker::start(prepared_images)?;
    let image_sender = image_worker.sender();
    let image_root = image_worker.root_snapshot().clone();
    cleanup.image_worker = Some(image_worker);
    let image_context = ImageCaptureContext::new(image_sender.clone(), image_root);
    let write_expectations = ClipboardWriteExpectationStore::new();
    let hotkey_manager = HotkeyManager::start_with_privacy_and_settings_and_startup(
        write_expectations.clone(),
        privacy_gate,
        privacy_sender,
        hotkey_settings_client,
        initial_settings.clone(),
        startup_sender,
    )?;
    let clipboard_inbox = hotkey_manager.clipboard_inbox();
    cleanup.capture_inbox = Some(clipboard_inbox.clone());
    cleanup.hotkey = Some(hotkey_manager);
    let storage_client = cleanup
        .storage
        .as_ref()
        .expect("StorageExecutor 已放入清理槽")
        .client();
    bind_copy_request_inbox(clipboard_inbox.clone());
    let capture_pump = start_clipboard_pump(
        clipboard_inbox,
        storage_client.clone(),
        write_expectations,
        image_context,
        capture_source_app,
    )?;
    cleanup.capture_pump = Some(capture_pump);
    let (history_requests, history_request_receiver) = history_request_channel();
    let (history_result_sender, history_results) = history_result_channel();
    bind_history_query_bridge(history_requests.clone(), history_results.clone());
    cleanup.history_requests = Some(history_requests.clone());
    cleanup.history_results = Some(history_results.clone());
    let history_query_worker = start_history_query_worker(
        storage_client.clone(),
        history_request_receiver,
        history_result_sender,
        || post_ui_event(UiEvent::HistoryQueryWake).is_ok(),
    )?;
    cleanup.history_query_worker = Some(history_query_worker);
    let (pin_mutations, pin_mutation_receiver) = pin_mutation_channel();
    bind_pin_mutation_sender(pin_mutations.clone());
    cleanup.pin_mutations = Some(pin_mutations.clone());
    let pin_mutation_worker =
        start_pin_mutation_worker(storage_client.clone(), pin_mutation_receiver, |result| {
            post_ui_event(UiEvent::PinMutationCompleted(result)).is_ok()
        })?;
    cleanup.pin_mutation_worker = Some(pin_mutation_worker);
    let (delete_mutations, delete_mutation_receiver) = delete_mutation_channel();
    bind_delete_mutation_sender(delete_mutations.clone());
    cleanup.delete_mutations = Some(delete_mutations.clone());
    let delete_mutation_worker = start_delete_mutation_worker(
        storage_client.clone(),
        Some(image_sender.clone()),
        delete_mutation_receiver,
        |result| post_ui_event(UiEvent::DeleteMutationCompleted(result)).is_ok(),
    )?;
    cleanup.delete_mutation_worker = Some(delete_mutation_worker);
    let (clear_history_mutations, clear_history_receiver) = clear_history_mutation_channel();
    bind_clear_history_mutation_sender(clear_history_mutations.clone());
    cleanup.clear_history_mutations = Some(clear_history_mutations.clone());
    let clear_history_worker = start_clear_history_mutation_worker(
        storage_client.clone(),
        Some(image_sender),
        clear_history_receiver,
        |result| post_ui_event(UiEvent::ClearHistoryMutationCompleted(result)).is_ok(),
    )?;
    cleanup.clear_history_worker = Some(clear_history_worker);
    let thumbnail_loader =
        ThumbnailLoader::start(|result| post_ui_event(UiEvent::ThumbnailLoaded(result)).is_ok())?;
    bind_thumbnail_loader_sender(thumbnail_loader.sender());
    cleanup.thumbnail_loader = Some(thumbnail_loader);
    diagnostics::emit(DiagnosticEvent::thread_state(ThreadState::Running));
    let event_loop_result = slint::run_event_loop_until_quit();
    diagnostics::emit(DiagnosticEvent::thread_state(ThreadState::Stopping));
    // 业务 worker 已在统一协调器中 join；释放主线程持有的最后一个 StorageClient 克隆，
    // 再建立 StorageExecutor 的关闭线性化点。
    drop(storage_client);
    let cleanup_result = cleanup.stop();
    diagnostics::emit(DiagnosticEvent::thread_state(ThreadState::Stopped));

    event_loop_result?;
    cleanup_result.map_err(std::io::Error::other)?;
    Ok(())
}

/// 启动结果桥消费线程；该线程先提交 SQLite，再投递 DTO，不触碰 Slint 对象。
#[cfg(windows)]
fn start_clipboard_pump(
    inbox: ClipboardCaptureInbox,
    storage: StorageClient,
    write_expectations: ClipboardWriteExpectationStore,
    image_context: ImageCaptureContext,
    capture_source_app: bool,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("clipboard-board-capture-pump".to_owned())
        .spawn(move || {
            run_clipboard_pump_with_source_policy(
                inbox,
                storage,
                write_expectations,
                Some(image_context),
                capture_source_app,
                |event| post_ui_event(event).is_ok(),
            );
        })
}

/// 生命周期协调器的阶段失败回归，避免 panic 线程句柄被重复 join 或遗留。
#[cfg(all(windows, test))]
mod runtime_cleanup_tests {
    use super::*;

    #[test]
    fn 阶段线程失败仍只收敛一次() {
        let mut cleanup = RuntimeCleanup::new();
        cleanup.capture_pump = Some(thread::spawn(|| panic!("测试阶段失败")));

        assert!(cleanup.stop().is_err());
        assert!(cleanup.capture_pump.is_none());
        // 第二次调用不会重复 join，且没有资源残留。
        assert!(cleanup.stop().is_ok());
    }
}

/// 非 Windows 目标仅保留骨架启动能力，正式热键实现由 Windows 平台模块提供。
#[cfg(not(windows))]
fn main() -> Result<(), slint::PlatformError> {
    let window = clipboard_board::create_app_window()?;
    slint::ComponentHandle::run(&window)
}

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
use clipboard_board::history_bridge::{run_clipboard_pump, ImageCaptureContext};
#[cfg(windows)]
use clipboard_board::history_mutation::{
    clear_history_mutation_channel, delete_mutation_channel, pin_mutation_channel,
    start_clear_history_mutation_worker, start_delete_mutation_worker, start_pin_mutation_worker,
};
#[cfg(windows)]
use clipboard_board::history_query::{
    history_request_channel, history_result_channel, start_history_query_worker,
};
#[cfg(windows)]
use clipboard_board::history_restore::load_startup_snapshot;
#[cfg(windows)]
use clipboard_board::image_pipeline::ImageWorker;
#[cfg(windows)]
use clipboard_board::image_storage::{prepare_image_storage, ImageStoragePreference};
#[cfg(windows)]
use clipboard_board::platform::windows::{acquire_or_activate, HotkeyManager, SingleInstanceRole};
#[cfg(windows)]
use clipboard_board::storage::{StorageClient, StorageExecutor};
#[cfg(windows)]
use clipboard_board::thumbnail_loader::ThumbnailLoader;
#[cfg(windows)]
use slint::ComponentHandle;
#[cfg(windows)]
use std::thread::{self, JoinHandle};

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

    let window = clipboard_board::create_app_window()?;
    bind_app_window(&window);
    window.hide()?;

    // 存储执行器必须在热键和剪贴板监听前唯一创建，并先完成启动恢复。
    let mut storage = StorageExecutor::open()?;
    let startup_snapshot = load_startup_snapshot(&mut storage)?;
    post_ui_event(UiEvent::ReplaceSnapshot(startup_snapshot))?;
    let prepared_images = prepare_image_storage(ImageStoragePreference::Default)?;
    let image_worker = ImageWorker::start(prepared_images)?;
    let image_context =
        ImageCaptureContext::new(image_worker.sender(), image_worker.root_snapshot().clone());
    let write_expectations = ClipboardWriteExpectationStore::new();
    let hotkey_manager = HotkeyManager::start_with_write_expectations(write_expectations.clone())?;
    let clipboard_inbox = hotkey_manager.clipboard_inbox();
    bind_copy_request_inbox(clipboard_inbox.clone());
    let capture_pump = match start_clipboard_pump(
        clipboard_inbox,
        storage.client(),
        write_expectations,
        image_context,
    ) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = hotkey_manager.stop();
            let _ = image_worker.stop();
            return Err(error.into());
        }
    };
    let (history_requests, history_request_receiver) = history_request_channel();
    let (history_result_sender, history_results) = history_result_channel();
    bind_history_query_bridge(history_requests.clone(), history_results.clone());
    let history_query_worker = match start_history_query_worker(
        storage.client(),
        history_request_receiver,
        history_result_sender,
        || post_ui_event(UiEvent::HistoryQueryWake).is_ok(),
    ) {
        Ok(handle) => handle,
        Err(error) => {
            history_requests.close();
            history_results.close();
            let _ = hotkey_manager.stop();
            let _ = capture_pump.join();
            let _ = image_worker.stop();
            return Err(error.into());
        }
    };
    let (pin_mutations, pin_mutation_receiver) = pin_mutation_channel();
    bind_pin_mutation_sender(pin_mutations.clone());
    let pin_mutation_worker =
        match start_pin_mutation_worker(storage.client(), pin_mutation_receiver, |result| {
            post_ui_event(UiEvent::PinMutationCompleted(result)).is_ok()
        }) {
            Ok(handle) => handle,
            Err(error) => {
                pin_mutations.close();
                history_requests.close();
                history_results.close();
                let _ = hotkey_manager.stop();
                let _ = capture_pump.join();
                let _ = image_worker.stop();
                let _ = history_query_worker.join();
                return Err(error.into());
            }
        };
    let (delete_mutations, delete_mutation_receiver) = delete_mutation_channel();
    bind_delete_mutation_sender(delete_mutations.clone());
    let delete_mutation_worker = match start_delete_mutation_worker(
        storage.client(),
        Some(image_worker.sender()),
        delete_mutation_receiver,
        |result| post_ui_event(UiEvent::DeleteMutationCompleted(result)).is_ok(),
    ) {
        Ok(handle) => handle,
        Err(error) => {
            delete_mutations.close();
            pin_mutations.close();
            history_requests.close();
            history_results.close();
            let _ = hotkey_manager.stop();
            let _ = capture_pump.join();
            let _ = image_worker.stop();
            let _ = history_query_worker.join();
            let _ = pin_mutation_worker.join();
            return Err(error.into());
        }
    };
    let (clear_history_mutations, clear_history_receiver) = clear_history_mutation_channel();
    bind_clear_history_mutation_sender(clear_history_mutations.clone());
    let clear_history_worker = match start_clear_history_mutation_worker(
        storage.client(),
        Some(image_worker.sender()),
        clear_history_receiver,
        |result| post_ui_event(UiEvent::ClearHistoryMutationCompleted(result)).is_ok(),
    ) {
        Ok(handle) => handle,
        Err(error) => {
            clear_history_mutations.close();
            delete_mutations.close();
            pin_mutations.close();
            history_requests.close();
            history_results.close();
            let _ = hotkey_manager.stop();
            let _ = capture_pump.join();
            let _ = image_worker.stop();
            let _ = history_query_worker.join();
            let _ = pin_mutation_worker.join();
            let _ = delete_mutation_worker.join();
            return Err(error.into());
        }
    };
    let thumbnail_loader = match ThumbnailLoader::start(|result| {
        post_ui_event(UiEvent::ThumbnailLoaded(result)).is_ok()
    }) {
        Ok(loader) => loader,
        Err(error) => {
            clear_history_mutations.close();
            delete_mutations.close();
            pin_mutations.close();
            history_requests.close();
            history_results.close();
            let _ = hotkey_manager.stop();
            let _ = capture_pump.join();
            let _ = history_query_worker.join();
            let _ = pin_mutation_worker.join();
            let _ = delete_mutation_worker.join();
            let _ = clear_history_worker.join();
            let _ = image_worker.stop();
            return Err(error.into());
        }
    };
    bind_thumbnail_loader_sender(thumbnail_loader.sender());
    diagnostics::emit(DiagnosticEvent::thread_state(ThreadState::Running));
    let event_loop_result = slint::run_event_loop_until_quit();
    diagnostics::emit(DiagnosticEvent::thread_state(ThreadState::Stopping));
    clear_history_mutations.close();
    delete_mutations.close();
    pin_mutations.close();
    history_requests.close();
    history_results.close();
    let hotkey_result = hotkey_manager.stop();
    // UI Quit 已先关闭复制入口；关闭线性化点前取出的在途请求在此 join 前完成，
    // 因此进程真正退出后不会继续写回系统剪贴板。
    let capture_pump_result = capture_pump
        .join()
        .map_err(|_| "剪贴板结果泵线程异常退出")
        .map(|_| ());
    let history_query_result = history_query_worker
        .join()
        .map_err(|_| "历史查询线程异常退出")
        .map(|_| ());
    let pin_mutation_result = pin_mutation_worker
        .join()
        .map_err(|_| "收藏变更线程异常退出")
        .map(|_| ());
    let delete_mutation_result = delete_mutation_worker
        .join()
        .map_err(|_| "删除变更线程异常退出")
        .map(|_| ());
    let clear_history_result = clear_history_worker
        .join()
        .map_err(|_| "清空历史线程异常退出")
        .map(|_| ());
    // 缩略图线程只读取已提交文件，UI 退出后先排空它，避免清理阶段仍向事件循环投递。
    let thumbnail_loader_result = thumbnail_loader.stop();
    // 捕获泵和图片 mutation 均已完成 finalize/回收，随后才能停止独占资产根的 ImageWorker。
    let image_worker_result = image_worker.stop();
    // 先关闭并 join 所有业务线程，再建立存储关闭线性化点，避免退出期丢失捕获或查询。
    let storage_result = storage
        .begin_closing()
        .and_then(|()| storage.finish_shutdown());
    diagnostics::emit(DiagnosticEvent::thread_state(ThreadState::Stopped));

    event_loop_result?;
    hotkey_result?;
    capture_pump_result?;
    image_worker_result?;
    history_query_result?;
    pin_mutation_result?;
    delete_mutation_result?;
    clear_history_result?;
    thumbnail_loader_result?;
    storage_result?;
    Ok(())
}

/// 启动结果桥消费线程；该线程先提交 SQLite，再投递 DTO，不触碰 Slint 对象。
#[cfg(windows)]
fn start_clipboard_pump(
    inbox: ClipboardCaptureInbox,
    storage: StorageClient,
    write_expectations: ClipboardWriteExpectationStore,
    image_context: ImageCaptureContext,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("clipboard-board-capture-pump".to_owned())
        .spawn(move || {
            run_clipboard_pump(
                inbox,
                storage,
                write_expectations,
                Some(image_context),
                |event| post_ui_event(event).is_ok(),
            );
        })
}

/// 非 Windows 目标仅保留骨架启动能力，正式热键实现由 Windows 平台模块提供。
#[cfg(not(windows))]
fn main() -> Result<(), slint::PlatformError> {
    let window = clipboard_board::create_app_window()?;
    slint::ComponentHandle::run(&window)
}

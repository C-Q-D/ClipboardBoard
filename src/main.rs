//! 此二进制入口负责创建主窗口并启动 Slint 事件循环。
//!
//! 当前入口先完成单实例判定，再创建 UI、初始化 SQLite、绑定弱窗口并启动热键、剪贴板
//! 结果泵和托盘消息线程。应用通过鼠标按钮显式写回系统剪贴板，不注入粘贴按键；
//! 图片历史和完整设置能力仍由后续原子接入。

#[cfg(windows)]
use clipboard_board::app::post_ui_event;
#[cfg(windows)]
use clipboard_board::app::{
    bind_app_window, bind_copy_request_inbox, bind_history_query_bridge, bind_pin_mutation_sender,
};
#[cfg(windows)]
use clipboard_board::clipboard::{ClipboardCaptureInbox, ClipboardWriteExpectationStore};
#[cfg(windows)]
use clipboard_board::command::UiEvent;
#[cfg(windows)]
use clipboard_board::diagnostics::{self, DiagnosticEvent, ThreadState};
#[cfg(windows)]
use clipboard_board::history_bridge::run_clipboard_pump;
#[cfg(windows)]
use clipboard_board::history_mutation::{pin_mutation_channel, start_pin_mutation_worker};
#[cfg(windows)]
use clipboard_board::history_query::{
    history_request_channel, history_result_channel, start_history_query_worker,
};
#[cfg(windows)]
use clipboard_board::history_restore::load_startup_snapshot;
#[cfg(windows)]
use clipboard_board::platform::windows::{acquire_or_activate, HotkeyManager, SingleInstanceRole};
#[cfg(windows)]
use clipboard_board::storage::{StorageClient, StorageExecutor};
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
    let write_expectations = ClipboardWriteExpectationStore::new();
    let hotkey_manager = HotkeyManager::start_with_write_expectations(write_expectations.clone())?;
    let clipboard_inbox = hotkey_manager.clipboard_inbox();
    bind_copy_request_inbox(clipboard_inbox.clone());
    let capture_pump =
        match start_clipboard_pump(clipboard_inbox, storage.client(), write_expectations) {
            Ok(handle) => handle,
            Err(error) => {
                let _ = hotkey_manager.stop();
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
                let _ = history_query_worker.join();
                return Err(error.into());
            }
        };
    diagnostics::emit(DiagnosticEvent::thread_state(ThreadState::Running));
    let event_loop_result = slint::run_event_loop_until_quit();
    diagnostics::emit(DiagnosticEvent::thread_state(ThreadState::Stopping));
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
    // 先关闭并 join 所有业务线程，再建立存储关闭线性化点，避免退出期丢失捕获或查询。
    let storage_result = storage
        .begin_closing()
        .and_then(|()| storage.finish_shutdown());
    diagnostics::emit(DiagnosticEvent::thread_state(ThreadState::Stopped));

    event_loop_result?;
    hotkey_result?;
    capture_pump_result?;
    history_query_result?;
    pin_mutation_result?;
    storage_result?;
    Ok(())
}

/// 启动结果桥消费线程；该线程先提交 SQLite，再投递 DTO，不触碰 Slint 对象。
#[cfg(windows)]
fn start_clipboard_pump(
    inbox: ClipboardCaptureInbox,
    storage: StorageClient,
    write_expectations: ClipboardWriteExpectationStore,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("clipboard-board-capture-pump".to_owned())
        .spawn(move || {
            run_clipboard_pump(inbox, storage, write_expectations, |event| {
                post_ui_event(event).is_ok()
            });
        })
}

/// 非 Windows 目标仅保留骨架启动能力，正式热键实现由 Windows 平台模块提供。
#[cfg(not(windows))]
fn main() -> Result<(), slint::PlatformError> {
    let window = clipboard_board::create_app_window()?;
    slint::ComponentHandle::run(&window)
}

//! 此模块创建 message-only HWND，并在其所属线程注册和处理 Alt+V 热键、剪贴板更新、单实例唤起及托盘消息。
//!
//! Win32 回调只负责把匹配的消息转成 UI 事件，或捕获 sequence/来源快照后交给
//! ClipboardIO worker；它不会直接读取剪贴板正文、访问 Slint 对象或操作存储状态。

use super::hotkey::{
    HotkeyError, HotkeyRuntimeSignal, HotkeyRuntimeState, HotkeySpec, HotkeyThreadAck,
    HotkeyThreadCommand, QueryActiveState, ThreadHotkeyState, ThreadTransactionState,
    HOTKEY_COMMAND_MESSAGE,
};
use super::tray::{handle_callback, TrayGuard, TRAY_CALLBACK_MESSAGE};
use crate::app::post_ui_event;
use crate::clipboard::{
    ClipboardCaptureInbox, ClipboardCaptureRequest, ClipboardIoWorker,
    ClipboardWriteExpectationStore,
};
use crate::command::UiEvent;
use crate::privacy::{PauseCommandSender, RecordingGate};
use std::cell::RefCell;
use std::ptr::{null, null_mut};
use std::sync::mpsc::{Receiver, SyncSender};

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, ERROR_HOTKEY_ALREADY_REGISTERED, HINSTANCE,
};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, GetClipboardSequenceNumber, RemoveClipboardFormatListener,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PeekMessageW,
    RegisterClassExW, TranslateMessage, HWND_MESSAGE, MSG, PM_NOREMOVE, WM_APP, WM_CLIPBOARDUPDATE,
    WM_HOTKEY, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

thread_local! {
    /// 消息线程独占的 ClipboardIO worker；避免把原生窗口句柄或 worker 所有权跨线程传递。
    static CLIPBOARD_WORKER: RefCell<Option<ClipboardIoWorker>> = const { RefCell::new(None) };
    /// 当前消息线程的运行时热键信号；WM_HOTKEY 过滤只读取同线程快照和共享 fail-closed 状态。
    static HOTKEY_SIGNAL: RefCell<Option<HotkeyRuntimeSignal>> = const { RefCell::new(None) };
}

/// Win32 注册的窗口类名称；message-only 窗口不出现在任务栏或屏幕上。
pub(crate) const WINDOW_CLASS_NAME: windows_sys::core::PCWSTR =
    windows_sys::core::w!("ClipboardBoardHotkey");

/// 单实例二次启动使用的进程间消息；消息不携带剪贴板正文或其他敏感数据。
pub(crate) const OPEN_PANEL_MESSAGE: u32 = WM_APP + 1;

/// 启动回执只携带线程 ID 和热键是否可用，不泄漏 HWND。
pub(crate) struct HotkeyStartup {
    /// 消息线程 ID，用于 PostThreadMessageW 唤醒。
    pub(crate) thread_id: u32,
    /// 首次 RegisterHotKey 是否成功。
    pub(crate) hotkey_available: bool,
}

/// RegisterHotKey/UnregisterHotKey 的最小注入 seam；生产实现绑定拥有 HWND 的消息线程，
/// 单元测试使用 FakeRegistrar 验证冲突和候选清理，不触碰真实桌面热键注册。
trait HotkeyRegistrar {
    /// 尝试登记候选规格。
    fn register(&mut self, spec: &HotkeySpec) -> Result<(), HotkeyError>;
    /// 注销指定 ID；返回 false 表示 Windows 状态不可证明已清理。
    fn unregister(&mut self, id: i32, registration_known: bool) -> UnregisterOutcome;
}

/// 注销后置状态；失败登记 stale 只适用于曾经确认注册过的 ID。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnregisterOutcome {
    /// 已成功注销。
    Removed,
    /// 已知未登记（例如 RegisterHotKey 失败后的补偿），不消耗 stale ID。
    NotFound,
    /// 可能仍被系统登记，必须登记 stale 禁止复用。
    Unknown,
}

/// 生产 RegisterHotKey 适配器；实例只在 message-only HWND 所属线程使用。
struct Win32HotkeyRegistrar {
    /// 不跨线程传递的隐藏窗口句柄。
    window: windows_sys::Win32::Foundation::HWND,
}

impl Win32HotkeyRegistrar {
    /// 绑定当前消息线程创建的 HWND。
    fn new(window: windows_sys::Win32::Foundation::HWND) -> Self {
        Self { window }
    }
}

impl HotkeyRegistrar for Win32HotkeyRegistrar {
    /// 在 HWND 所属线程调用 RegisterHotKey，并将冲突保留为领域错误。
    fn register(&mut self, spec: &HotkeySpec) -> Result<(), HotkeyError> {
        unsafe {
            if RegisterHotKey(self.window, spec.id, spec.modifiers, spec.virtual_key) == 0 {
                return Err(classify_registration_error(GetLastError(), &spec.label));
            }
        }
        Ok(())
    }

    /// 在同一 HWND 所属线程调用 UnregisterHotKey。
    fn unregister(&mut self, id: i32, registration_known: bool) -> UnregisterOutcome {
        if unsafe { UnregisterHotKey(self.window, id) != 0 } {
            UnregisterOutcome::Removed
        } else if registration_known {
            UnregisterOutcome::Unknown
        } else {
            UnregisterOutcome::NotFound
        }
    }
}

/// 在专用线程创建隐藏窗口、注册热键并运行消息泵。
///
/// 剪贴板、托盘、暂停门和热键命令分别拥有独立生命周期；显式传参可以保证
/// 线程启动时的所有权边界可审计，避免把窗口线程状态塞进隐式全局上下文。
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    hotkey: HotkeySpec,
    ready_sender: SyncSender<Result<HotkeyStartup, HotkeyError>>,
    command_receiver: Receiver<HotkeyThreadCommand>,
    signal: HotkeyRuntimeSignal,
    clipboard_inbox: ClipboardCaptureInbox,
    write_expectations: ClipboardWriteExpectationStore,
    recording_gate: RecordingGate,
    pause_commands: PauseCommandSender,
) -> Result<(), HotkeyError> {
    let thread_id = unsafe { GetCurrentThreadId() };

    // 先创建消息队列，再允许主线程在启动阶段投递停止消息。
    unsafe {
        let mut message = MSG::default();
        let _ = PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE);
    }

    let instance = unsafe { GetModuleHandleW(null()) };
    if instance.is_null() {
        let error = HotkeyError::Windows {
            operation: "GetModuleHandleW",
            code: unsafe { GetLastError() },
        };
        let _ = ready_sender.send(Err(error.clone()));
        return Err(error);
    }

    if let Err(error) = unsafe { register_window_class(instance as HINSTANCE) } {
        let _ = ready_sender.send(Err(error.clone()));
        return Err(error);
    }

    let window = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            WINDOW_CLASS_NAME,
            windows_sys::core::w!(""),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance as HINSTANCE,
            null(),
        )
    };
    if window.is_null() {
        let error = HotkeyError::Windows {
            operation: "CreateWindowExW",
            code: unsafe { GetLastError() },
        };
        let _ = ready_sender.send(Err(error.clone()));
        return Err(error);
    }

    // 首次注册冲突不再销毁 HWND；托盘和显式打开能力必须保留，进入 active=None。
    let mut registrar = Win32HotkeyRegistrar::new(window);
    let registration_result = registrar.register(&hotkey);
    let initial_active = registration_result.is_ok().then(|| hotkey.clone());
    signal.set(
        if initial_active.is_some() {
            HotkeyRuntimeState::ActiveOld
        } else {
            HotkeyRuntimeState::None
        },
        initial_active.as_ref().map_or(0, |spec| spec.id),
    );

    let clipboard_worker = match ClipboardIoWorker::start_with_gate(
        clipboard_inbox,
        write_expectations,
        recording_gate,
    ) {
        Ok(worker) => worker,
        Err(_) => {
            unsafe {
                if initial_active.is_some() {
                    let _ = registrar.unregister(hotkey.id, true);
                }
                let _ = DestroyWindow(window);
            }
            let error = HotkeyError::Windows {
                operation: "ClipboardIoWorker::start",
                code: 0,
            };
            let _ = ready_sender.send(Err(error.clone()));
            return Err(error);
        }
    };

    if let Err(error) = unsafe { register_clipboard_listener(window) } {
        let _ = clipboard_worker.stop();
        unsafe {
            if initial_active.is_some() {
                let _ = registrar.unregister(hotkey.id, true);
            }
            let _ = DestroyWindow(window);
        }
        let _ = ready_sender.send(Err(error.clone()));
        return Err(error);
    }

    // 托盘图标必须绑定到同一个 message-only HWND，确保回调和热键共享消息线程。
    let mut tray = match TrayGuard::create(window) {
        Ok(tray) => tray,
        Err(error) => {
            unsafe {
                let _ = RemoveClipboardFormatListener(window);
                if initial_active.is_some() {
                    let _ = registrar.unregister(hotkey.id, true);
                }
                let _ = DestroyWindow(window);
            }
            let _ = clipboard_worker.stop();
            let _ = ready_sender.send(Err(error.clone()));
            return Err(error);
        }
    };

    CLIPBOARD_WORKER.with(|slot| {
        *slot.borrow_mut() = Some(clipboard_worker);
    });

    HOTKEY_SIGNAL.with(|slot| {
        *slot.borrow_mut() = Some(signal.clone());
    });

    let mut thread_state = ThreadHotkeyState::new(initial_active.clone());
    if ready_sender
        .send(Ok(HotkeyStartup {
            thread_id,
            hotkey_available: initial_active.is_some(),
        }))
        .is_err()
    {
        let _ = tray.remove();
        // 若第一次 NIM_DELETE 失败，Drop 会在 DestroyWindow 前再尝试一次。
        drop(tray);
        let _ = stop_clipboard_worker();
        unsafe {
            let _ = RemoveClipboardFormatListener(window);
            if initial_active.is_some() {
                let _ = registrar.unregister(hotkey.id, true);
            }
            let _ = DestroyWindow(window);
        }
        return Err(HotkeyError::StartupChannelClosed);
    }

    let message_loop_result = message_loop(
        &pause_commands,
        &command_receiver,
        &mut registrar,
        &signal,
        &mut thread_state,
    );
    // 先停止更新通知，再回收 worker，确保退出阶段不再接受新的剪贴板事件。
    let listener_result = unsafe { unregister_clipboard_listener(window) };
    let worker_result = stop_clipboard_worker();
    // NIM_DELETE 必须发生在 DestroyWindow 之前；即使删除失败也继续注销热键和销毁窗口。
    let tray_result = tray.remove();
    // Drop 的兜底重试仍发生在 DestroyWindow 前，避免把通知数据绑定到已销毁 HWND。
    drop(tray);
    unsafe {
        if let Some(active) = thread_state.active.as_ref() {
            let _ = registrar.unregister(active.id, true);
        }
        if let Some((_, _, candidate_id, _)) = thread_state.candidate.as_ref() {
            let _ = registrar.unregister(*candidate_id, true);
        }
        for stale_id in &thread_state.stale_ids {
            let _ = registrar.unregister(*stale_id, true);
        }
        let _ = DestroyWindow(window);
    }
    message_loop_result
        .and(listener_result)
        .and(worker_result)
        .and(tray_result)
}

/// 注册剪贴板监听；失败时保留 Win32 错误码，启动流程不会留下半初始化窗口。
unsafe fn register_clipboard_listener(
    window: windows_sys::Win32::Foundation::HWND,
) -> Result<(), HotkeyError> {
    if AddClipboardFormatListener(window) == 0 {
        return Err(HotkeyError::Windows {
            operation: "AddClipboardFormatListener",
            code: GetLastError(),
        });
    }
    Ok(())
}

/// 注销剪贴板监听；即使消息泵已退出也必须显式释放监听关系。
unsafe fn unregister_clipboard_listener(
    window: windows_sys::Win32::Foundation::HWND,
) -> Result<(), HotkeyError> {
    if RemoveClipboardFormatListener(window) == 0 {
        return Err(HotkeyError::Windows {
            operation: "RemoveClipboardFormatListener",
            code: GetLastError(),
        });
    }
    Ok(())
}

/// 取出并停止消息线程绑定的 worker；返回值只用于清理阶段的有限错误传播。
fn stop_clipboard_worker() -> Result<(), HotkeyError> {
    let worker = CLIPBOARD_WORKER.with(|slot| slot.borrow_mut().take());
    worker
        .map(|worker| {
            worker.stop().map_err(|_| HotkeyError::Windows {
                operation: "ClipboardIoWorker::stop",
                code: 0,
            })
        })
        .unwrap_or(Ok(()))
}

/// 注册窗口类；类已存在时复用它，避免重复启动测试造成无意义失败。
unsafe fn register_window_class(instance: HINSTANCE) -> Result<(), HotkeyError> {
    let window_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: WINDOW_CLASS_NAME,
        ..WNDCLASSEXW::default()
    };

    if RegisterClassExW(&window_class) == 0 {
        let code = GetLastError();
        if code != ERROR_CLASS_ALREADY_EXISTS {
            return Err(HotkeyError::Windows {
                operation: "RegisterClassExW",
                code,
            });
        }
    }
    Ok(())
}

/// 处理隐藏窗口收到的消息；只有共享运行时信号确认的 active ID 才能产生 UI 事件。
unsafe extern "system" fn window_proc(
    window: windows_sys::Win32::Foundation::HWND,
    message: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    if let Some(event) = panel_event_for_runtime_message(message, wparam) {
        if let Err(error) = post_ui_event(event) {
            eprintln!("面板显示状态事件无法进入 UI 事件队列：{error}");
        }
        return 0;
    }

    if is_clipboard_update_message(message) {
        enqueue_clipboard_capture();
        return 0;
    }

    DefWindowProcW(window, message, wparam, lparam)
}

/// 只接受固定托盘回调编号，避免把任意 WM_APP 消息当作 Shell 通知。
fn is_tray_callback_message(message: u32) -> bool {
    message == TRAY_CALLBACK_MESSAGE
}

/// 将 RegisterHotKey 的错误码转换为不会被静默吞掉的领域错误。
fn classify_registration_error(code: u32, shortcut: &str) -> HotkeyError {
    if code == ERROR_HOTKEY_ALREADY_REGISTERED {
        HotkeyError::RegistrationConflict {
            shortcut: shortcut.to_owned(),
        }
    } else {
        HotkeyError::Windows {
            operation: "RegisterHotKey",
            code,
        }
    }
}

/// 只接受当前 active 热键的 WM_HOTKEY 消息；None/Unknown 和 candidate/stale ID 全部丢弃。
fn is_runtime_hotkey_message(
    message: u32,
    wparam: usize,
    state: HotkeyRuntimeState,
    active_id: i32,
) -> bool {
    message == WM_HOTKEY
        && matches!(
            state,
            HotkeyRuntimeState::ActiveOld | HotkeyRuntimeState::Candidate
        )
        && active_id > 0
        && wparam == active_id as usize
}

/// 只接受默认 Alt+V 的兼容测试入口；生产消息过滤使用线程共享 runtime signal。
#[cfg(test)]
fn is_default_hotkey_message(message: u32, wparam: usize) -> bool {
    is_runtime_hotkey_message(
        message,
        wparam,
        HotkeyRuntimeState::ActiveOld,
        super::hotkey::DEFAULT_HOTKEY_ID,
    )
}

/// 只接受系统定义的剪贴板更新消息，其他 WM_APP 消息不得触发读取。
fn is_clipboard_update_message(message: u32) -> bool {
    message == WM_CLIPBOARDUPDATE
}

/// 在消息线程捕获 sequence/来源快照，并把正文读取交给容量为一的 worker 队列。
fn enqueue_clipboard_capture() {
    let sequence = unsafe { GetClipboardSequenceNumber() };
    let source = super::source::capture_foreground_source_snapshot();
    let request = ClipboardCaptureRequest::new_with_snapshot(sequence, source);

    CLIPBOARD_WORKER.with(|slot| {
        let worker_slot = slot.borrow();
        let Some(worker) = worker_slot.as_ref() else {
            return;
        };
        // worker 会把成功结果或 sequence 失配错误发布到公共 inbox；消息线程不等待响应。
        let _ = worker.request_capture(request);
    });
}

/// 只接受固定的单实例唤起消息，避免把任意 WM_APP 消息当作业务命令。
fn is_open_panel_message(message: u32) -> bool {
    message == OPEN_PANEL_MESSAGE
}

/// 将原生消息映射为互不混淆的面板语义：二次启动幂等显示，热键切换显隐。
#[cfg(test)]
fn panel_event_for_message(
    message: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
) -> Option<UiEvent> {
    if is_open_panel_message(message) {
        Some(UiEvent::ShowPanel)
    } else if is_default_hotkey_message(message, wparam) {
        Some(UiEvent::OpenPanel)
    } else {
        None
    }
}

/// 使用当前线程共享的运行时信号转换热键消息；对账状态永远不产生 UI 事件。
fn panel_event_for_runtime_message(
    message: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
) -> Option<UiEvent> {
    if is_open_panel_message(message) {
        Some(UiEvent::ShowPanel)
    } else {
        HOTKEY_SIGNAL.with(|signal| {
            signal.borrow().as_ref().and_then(|signal| {
                is_runtime_hotkey_message(message, wparam, signal.state(), signal.active_id())
                    .then_some(UiEvent::OpenPanel)
            })
        })
    }
}

/// 拉取并分发消息，返回值 -1 被视为 Win32 错误，0 表示收到退出消息。
fn message_loop(
    pause_commands: &PauseCommandSender,
    command_receiver: &Receiver<HotkeyThreadCommand>,
    registrar: &mut impl HotkeyRegistrar,
    signal: &HotkeyRuntimeSignal,
    state: &mut ThreadHotkeyState,
) -> Result<(), HotkeyError> {
    loop {
        let mut message = MSG::default();
        let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        if result == -1 {
            return Err(HotkeyError::Windows {
                operation: "GetMessageW",
                code: unsafe { GetLastError() },
            });
        }
        if result == 0 {
            return Ok(());
        }

        if message.message == HOTKEY_COMMAND_MESSAGE {
            while let Ok(command) = command_receiver.try_recv() {
                handle_thread_command(command, registrar, signal, state);
            }
            continue;
        }

        if is_tray_callback_message(message.message)
            && handle_callback(message.hwnd, message.wParam, message.lParam, pause_commands)
        {
            continue;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// 在拥有 HWND 的线程执行全部 Register/Unregister 操作；其他线程只收发 DTO。
fn handle_thread_command(
    command: HotkeyThreadCommand,
    registrar: &mut impl HotkeyRegistrar,
    signal: &HotkeyRuntimeSignal,
    state: &mut ThreadHotkeyState,
) {
    match command {
        HotkeyThreadCommand::RegisterCandidate {
            transaction_id,
            generation,
            settings,
            reply,
        } => {
            if !state.accepts(transaction_id, generation) || state.candidate.is_some() {
                send_thread_ack(
                    &reply,
                    HotkeyThreadAck::Cancelled {
                        transaction_id,
                        generation,
                    },
                );
                return;
            }
            // 同一事务重复登记不能再次调用 RegisterHotKey；已登记事务只回放原候选，
            // 已发布/已取消事务则 fail-closed，避免迟到命令重新占用新 ID。
            if let Some((known_generation, transaction_state)) =
                state.transactions.get(&transaction_id).copied()
            {
                if known_generation != generation {
                    send_thread_ack(
                        &reply,
                        HotkeyThreadAck::Cancelled {
                            transaction_id,
                            generation,
                        },
                    );
                    return;
                }
                if transaction_state != ThreadTransactionState::NotFound {
                    send_thread_ack(
                        &reply,
                        HotkeyThreadAck::Cancelled {
                            transaction_id,
                            generation,
                        },
                    );
                    return;
                }
            }
            let Some(candidate_id) = state.allocate_id() else {
                send_thread_ack(
                    &reply,
                    HotkeyThreadAck::RegistrationFailed {
                        transaction_id,
                        generation,
                        error: HotkeyError::InvalidId,
                    },
                );
                return;
            };
            let Ok(spec) = HotkeySpec::from_settings(candidate_id, &settings) else {
                send_thread_ack(
                    &reply,
                    HotkeyThreadAck::RegistrationFailed {
                        transaction_id,
                        generation,
                        error: HotkeyError::InvalidSettings,
                    },
                );
                return;
            };
            let registration = registrar.register(&spec);
            match registration {
                Ok(()) => {
                    state.candidate = Some((transaction_id, generation, candidate_id, settings));
                    state.transactions.insert(
                        transaction_id,
                        (generation, ThreadTransactionState::CandidateRegistered),
                    );
                    send_thread_ack(
                        &reply,
                        HotkeyThreadAck::CandidateRegistered {
                            transaction_id,
                            generation,
                            candidate_id,
                        },
                    );
                }
                Err(error) => {
                    // RegisterHotKey 失败也显式走注销；Windows 通常不会留下登记，
                    // 但失败清理本身若不可证明成功，ID 必须登记 stale 禁止复用。
                    if matches!(
                        registrar.unregister(candidate_id, false),
                        UnregisterOutcome::Unknown
                    ) {
                        state.stale_ids.insert(candidate_id);
                    }
                    send_thread_ack(
                        &reply,
                        HotkeyThreadAck::RegistrationFailed {
                            transaction_id,
                            generation,
                            error,
                        },
                    );
                }
            }
        }
        HotkeyThreadCommand::PublishActive {
            transaction_id,
            generation,
            candidate_id,
            settings_revision,
            reply,
        } => {
            if matches!(
                state.transactions.get(&transaction_id),
                Some((known_generation, ThreadTransactionState::Published))
                    if *known_generation == generation
            ) {
                if state
                    .active
                    .as_ref()
                    .is_some_and(|active| active.id == candidate_id)
                {
                    send_thread_ack(
                        &reply,
                        HotkeyThreadAck::Published {
                            transaction_id,
                            generation,
                            active_id: candidate_id,
                        },
                    );
                } else {
                    send_thread_ack(
                        &reply,
                        HotkeyThreadAck::Cancelled {
                            transaction_id,
                            generation,
                        },
                    );
                }
                return;
            }
            if !state.accepts(transaction_id, generation) {
                send_thread_ack(
                    &reply,
                    HotkeyThreadAck::Cancelled {
                        transaction_id,
                        generation,
                    },
                );
                return;
            }
            let Some((candidate_transaction, candidate_generation, id, settings)) =
                state.candidate.clone()
            else {
                send_thread_ack(
                    &reply,
                    HotkeyThreadAck::Cancelled {
                        transaction_id,
                        generation,
                    },
                );
                return;
            };
            if candidate_transaction != transaction_id
                || candidate_generation != generation
                || id != candidate_id
            {
                send_thread_ack(
                    &reply,
                    HotkeyThreadAck::Cancelled {
                        transaction_id,
                        generation,
                    },
                );
                return;
            }
            let Ok(spec) = HotkeySpec::from_settings(candidate_id, &settings) else {
                send_thread_ack(
                    &reply,
                    HotkeyThreadAck::Cancelled {
                        transaction_id,
                        generation,
                    },
                );
                return;
            };
            let old_active = state.active.replace(spec);
            state.candidate = None;
            state.transactions.insert(
                transaction_id,
                (generation, ThreadTransactionState::Published),
            );
            state.active_state = QueryActiveState::Candidate;
            state.active_revision = settings_revision;
            if let Some(old_active) = old_active {
                if old_active.id != candidate_id
                    && matches!(
                        registrar.unregister(old_active.id, true),
                        UnregisterOutcome::Unknown
                    )
                {
                    state.stale_ids.insert(old_active.id);
                }
            }
            // active 替换和旧 ID 注销完成后，先在线程内切换过滤信号，再发送回执；
            // 这样消息队列中任何紧随其后的旧 WM_HOTKEY 都不会穿透到 UI。
            signal.set(HotkeyRuntimeState::Candidate, candidate_id);
            send_thread_ack(
                &reply,
                HotkeyThreadAck::Published {
                    transaction_id,
                    generation,
                    active_id: candidate_id,
                },
            );
        }
        HotkeyThreadCommand::QueryTransaction {
            transaction_id,
            reply,
        } => {
            let (generation, transaction) = state
                .transactions
                .get(&transaction_id)
                .copied()
                .unwrap_or((0, ThreadTransactionState::NotFound));
            let (active_state, active_id) = match state.active_state {
                QueryActiveState::Unknown => (QueryActiveState::Unknown, 0),
                QueryActiveState::None => (QueryActiveState::None, 0),
                active_state @ (QueryActiveState::Old | QueryActiveState::Candidate) => (
                    active_state,
                    state.active.as_ref().map_or(0, |active| active.id),
                ),
            };
            let candidate_id = state.candidate.as_ref().map_or(0, |entry| entry.2);
            send_thread_ack(
                &reply,
                HotkeyThreadAck::Query {
                    transaction_id,
                    result: super::hotkey::ThreadQueryResult {
                        transaction,
                        active_state,
                        active_id,
                        candidate_id,
                        generation,
                    },
                },
            );
        }
        HotkeyThreadCommand::CancelTransaction {
            transaction_id,
            generation,
            reply,
        } => {
            // 已发布事务不能被迟到取消命令降级；保留当前 active 和 Query 的 Published 证据。
            if matches!(
                state.transactions.get(&transaction_id),
                Some((known_generation, ThreadTransactionState::Published))
                    if *known_generation == generation
            ) {
                send_thread_ack(
                    &reply,
                    HotkeyThreadAck::Cancelled {
                        transaction_id,
                        generation,
                    },
                );
                return;
            }
            let candidate = state
                .candidate
                .clone()
                .filter(|entry| entry.0 == transaction_id && entry.1 == generation);
            if let Some((_, _, candidate_id, _)) = candidate {
                let outcome = registrar.unregister(candidate_id, true);
                if matches!(outcome, UnregisterOutcome::Unknown) {
                    state.stale_ids.insert(candidate_id);
                }
                state.candidate = None;
            }
            state.cancel(transaction_id, generation);
            // Unknown 事务可能已经把新配置写入磁盘；取消迟到命令不能重新启用旧 ID。
            // 只有在信号本来可证明为旧 active/None 时才恢复对应运行态。
            if matches!(signal.state(), HotkeyRuntimeState::Unknown)
                || matches!(state.active_state, QueryActiveState::Unknown)
            {
                signal.set(HotkeyRuntimeState::Unknown, 0);
            } else {
                signal.set(
                    if state.active.is_some() {
                        HotkeyRuntimeState::ActiveOld
                    } else {
                        HotkeyRuntimeState::None
                    },
                    state.active.as_ref().map_or(0, |active| active.id),
                );
            }
            send_thread_ack(
                &reply,
                HotkeyThreadAck::Cancelled {
                    transaction_id,
                    generation,
                },
            );
        }
        HotkeyThreadCommand::DropCandidate {
            transaction_id,
            generation,
            candidate_id,
            reply,
        } => {
            let matches = state.candidate.as_ref().is_some_and(|entry| {
                entry.0 == transaction_id && entry.1 == generation && entry.2 == candidate_id
            });
            if !matches || !state.accepts(transaction_id, generation) {
                send_thread_ack(
                    &reply,
                    HotkeyThreadAck::Cancelled {
                        transaction_id,
                        generation,
                    },
                );
                return;
            }
            let outcome = registrar.unregister(candidate_id, true);
            let success = matches!(outcome, UnregisterOutcome::Removed);
            if matches!(outcome, UnregisterOutcome::Unknown) {
                state.stale_ids.insert(candidate_id);
            }
            state.candidate = None;
            state.transactions.insert(
                transaction_id,
                (generation, ThreadTransactionState::Cancelled),
            );
            send_thread_ack(
                &reply,
                HotkeyThreadAck::CandidateDropped {
                    transaction_id,
                    generation,
                    success,
                },
            );
        }
        HotkeyThreadCommand::Shutdown { reply } => {
            state.shutting_down = true;
            let mut stale_count = state.stale_ids.len();
            if let Some(active) = state.active.take() {
                if matches!(
                    registrar.unregister(active.id, true),
                    UnregisterOutcome::Unknown
                ) && state.stale_ids.insert(active.id)
                {
                    stale_count += 1;
                }
            }
            if let Some((_, _, candidate_id, _)) = state.candidate.take() {
                if matches!(
                    registrar.unregister(candidate_id, true),
                    UnregisterOutcome::Unknown
                ) && state.stale_ids.insert(candidate_id)
                {
                    stale_count += 1;
                }
            }
            for stale_id in state.stale_ids.iter().copied().collect::<Vec<_>>() {
                let _ = registrar.unregister(stale_id, true);
            }
            state.active_state = QueryActiveState::None;
            state.active_revision = 0;
            signal.set(HotkeyRuntimeState::None, 0);
            send_thread_ack(&reply, HotkeyThreadAck::ShutdownComplete { stale_count });
        }
    }
}

/// 发送消息线程回执；调用方关闭时丢弃回执不应阻塞 HWND 消息泵。
fn send_thread_ack(reply: &SyncSender<HotkeyThreadAck>, ack: HotkeyThreadAck) {
    let _ = reply.try_send(ack);
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证热键 ID 过滤和冲突错误映射，不依赖桌面上的其他热键占用者。

    use super::TRAY_CALLBACK_MESSAGE;
    use super::{
        classify_registration_error, handle_thread_command, is_clipboard_update_message,
        is_default_hotkey_message, is_open_panel_message, is_runtime_hotkey_message,
        is_tray_callback_message, panel_event_for_message, HotkeyRegistrar, UnregisterOutcome,
        OPEN_PANEL_MESSAGE,
    };
    use crate::command::UiEvent;
    use crate::platform::windows::hotkey::{
        default_hotkey_spec, HotkeyError, HotkeyRuntimeSignal, HotkeyRuntimeState, HotkeyThreadAck,
        HotkeyThreadCommand, ThreadHotkeyState, ThreadTransactionState,
    };
    use crate::settings::HotkeySettings;
    use std::collections::BTreeSet;
    use std::sync::mpsc;
    use windows_sys::Win32::Foundation::ERROR_HOTKEY_ALREADY_REGISTERED;
    use windows_sys::Win32::UI::WindowsAndMessaging::{WM_CLIPBOARDUPDATE, WM_HOTKEY};

    /// 记录 Register/Unregister 调用的假注册器；测试只验证协议和清理语义，
    /// 不在开发机上申请真实全局快捷键，避免被其他程序状态污染。
    struct FakeRegistrar {
        /// 当前被假设已经注册的进程内 ID。
        registered: BTreeSet<i32>,
        /// 可选的登记失败原因。
        register_error: Option<HotkeyError>,
        /// 注销动作的可控结果。
        unregister_outcome: UnregisterOutcome,
    }

    impl FakeRegistrar {
        /// 创建默认成功注册、成功注销的假注册器。
        fn new() -> Self {
            Self {
                registered: BTreeSet::new(),
                register_error: None,
                unregister_outcome: UnregisterOutcome::Removed,
            }
        }
    }

    impl HotkeyRegistrar for FakeRegistrar {
        /// 按测试配置返回冲突，成功时记录候选 ID。
        fn register(
            &mut self,
            spec: &crate::platform::windows::hotkey::HotkeySpec,
        ) -> Result<(), HotkeyError> {
            if let Some(error) = &self.register_error {
                return Err(error.clone());
            }
            self.registered.insert(spec.id);
            Ok(())
        }

        /// 按配置模拟成功、已知未登记或状态未知的注销结果。
        fn unregister(&mut self, id: i32, _registration_known: bool) -> UnregisterOutcome {
            match self.unregister_outcome {
                UnregisterOutcome::Removed => {
                    self.registered.remove(&id);
                    UnregisterOutcome::Removed
                }
                UnregisterOutcome::NotFound => UnregisterOutcome::NotFound,
                UnregisterOutcome::Unknown => UnregisterOutcome::Unknown,
            }
        }
    }

    /// 只有固定 ID 的 WM_HOTKEY 才能进入 UI 事件转换分支。
    #[test]
    fn 只接受默认热键消息() {
        assert!(is_default_hotkey_message(WM_HOTKEY, 0x4342));
        assert!(!is_default_hotkey_message(WM_HOTKEY, 0x4343));
        assert!(!is_default_hotkey_message(WM_HOTKEY + 1, 0x4342));
    }

    /// 运行时只允许 active ID；candidate、stale、None 和 Unknown 都必须被过滤。
    #[test]
    fn 动态热键消息只接受当前_active() {
        assert!(is_runtime_hotkey_message(
            WM_HOTKEY,
            0x4342,
            HotkeyRuntimeState::ActiveOld,
            0x4342
        ));
        assert!(is_runtime_hotkey_message(
            WM_HOTKEY,
            0x7001,
            HotkeyRuntimeState::Candidate,
            0x7001
        ));
        for (state, active_id, wparam) in [
            (HotkeyRuntimeState::None, 0, 0x4342),
            (HotkeyRuntimeState::Unknown, 0, 0x4342),
            (HotkeyRuntimeState::ActiveOld, 0x4342, 0x7001),
            (HotkeyRuntimeState::Candidate, 0x7001, 0x4342),
        ] {
            assert!(!is_runtime_hotkey_message(
                WM_HOTKEY, wparam, state, active_id
            ));
        }
    }

    /// 二次启动只能使用固定消息编号唤起主实例，其他消息必须被忽略。
    #[test]
    fn 只接受固定打开消息() {
        assert!(is_open_panel_message(OPEN_PANEL_MESSAGE));
        assert!(!is_open_panel_message(OPEN_PANEL_MESSAGE + 1));
    }

    /// 二次启动必须幂等显示，Alt+V 必须切换，不能因共享消息窗口而混淆行为。
    #[test]
    fn 原生消息区分幂等显示与热键切换() {
        assert_eq!(
            panel_event_for_message(OPEN_PANEL_MESSAGE, 0),
            Some(UiEvent::ShowPanel)
        );
        assert_eq!(
            panel_event_for_message(WM_HOTKEY, 0x4342),
            Some(UiEvent::OpenPanel)
        );
        assert_eq!(panel_event_for_message(WM_HOTKEY, 0x4343), None);
    }

    /// 只有固定托盘回调消息才进入托盘处理器。
    #[test]
    fn 只接受固定托盘消息() {
        assert!(is_tray_callback_message(TRAY_CALLBACK_MESSAGE));
        assert!(!is_tray_callback_message(TRAY_CALLBACK_MESSAGE + 1));
    }

    /// 只有 WM_CLIPBOARDUPDATE 才能进入捕获队列，避免普通消息误触发读取。
    #[test]
    fn 只接受剪贴板更新消息() {
        assert!(is_clipboard_update_message(WM_CLIPBOARDUPDATE));
        assert!(!is_clipboard_update_message(WM_CLIPBOARDUPDATE + 1));
    }

    /// Win32 的热键占用错误必须转换成带快捷键名称的明确错误。
    #[test]
    fn 热键冲突错误映射明确() {
        assert_eq!(
            classify_registration_error(ERROR_HOTKEY_ALREADY_REGISTERED, "Alt + V"),
            HotkeyError::RegistrationConflict {
                shortcut: "Alt + V".to_owned()
            }
        );
        assert_eq!(
            classify_registration_error(5, "Alt + V"),
            HotkeyError::Windows {
                operation: "RegisterHotKey",
                code: 5
            }
        );
    }

    /// 候选注册成功后必须由消息线程保存候选及其事务状态，ID 不能沿用旧 active。
    #[test]
    fn 假注册器登记候选并发布后切换运行信号() {
        let mut registrar = FakeRegistrar::new();
        let signal = HotkeyRuntimeSignal::new();
        let mut state = ThreadHotkeyState::new(Some(default_hotkey_spec()));
        let settings = HotkeySettings::default();
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);

        handle_thread_command(
            HotkeyThreadCommand::RegisterCandidate {
                transaction_id: 11,
                generation: 3,
                settings: settings.clone(),
                reply: reply_sender,
            },
            &mut registrar,
            &signal,
            &mut state,
        );
        let candidate_id = match reply_receiver.recv().expect("候选登记回执") {
            HotkeyThreadAck::CandidateRegistered { candidate_id, .. } => candidate_id,
            ack => panic!("预期候选登记成功，实际为 {ack:?}"),
        };
        assert_eq!(
            state.candidate.as_ref().map(|candidate| candidate.2),
            Some(candidate_id)
        );
        assert!(registrar.registered.contains(&candidate_id));

        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        handle_thread_command(
            HotkeyThreadCommand::PublishActive {
                transaction_id: 11,
                generation: 3,
                candidate_id,
                settings_revision: 9,
                reply: reply_sender,
            },
            &mut registrar,
            &signal,
            &mut state,
        );
        assert!(matches!(
            reply_receiver.recv().expect("发布回执"),
            HotkeyThreadAck::Published { active_id, .. } if active_id == candidate_id
        ));
        assert_eq!(signal.state(), HotkeyRuntimeState::Candidate);
        assert_eq!(signal.active_id(), candidate_id);
        assert_eq!(
            state.active.as_ref().map(|active| active.id),
            Some(candidate_id)
        );
        assert_eq!(
            state.transactions.get(&11),
            Some(&(3, ThreadTransactionState::Published))
        );
    }

    /// RegisterHotKey 明确冲突时，补偿注销返回 NotFound，不应把尚未登记的 ID 污染为 stale。
    #[test]
    fn 假注册器冲突不误记_stale_id() {
        let mut registrar = FakeRegistrar::new();
        registrar.register_error = Some(HotkeyError::RegistrationConflict {
            shortcut: "Alt + V".to_owned(),
        });
        registrar.unregister_outcome = UnregisterOutcome::NotFound;
        let signal = HotkeyRuntimeSignal::new();
        let mut state = ThreadHotkeyState::new(Some(default_hotkey_spec()));
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);

        handle_thread_command(
            HotkeyThreadCommand::RegisterCandidate {
                transaction_id: 12,
                generation: 4,
                settings: HotkeySettings::default(),
                reply: reply_sender,
            },
            &mut registrar,
            &signal,
            &mut state,
        );
        assert!(matches!(
            reply_receiver.recv().expect("冲突回执"),
            HotkeyThreadAck::RegistrationFailed {
                error: HotkeyError::RegistrationConflict { .. },
                ..
            }
        ));
        assert!(state.stale_ids.is_empty());
        assert!(state.candidate.is_none());
    }

    /// 候选注销结果未知时必须写入 stale，阻止进程内 ID 被再次分配。
    #[test]
    fn 假注册器注销未知时标记_stale_id() {
        let mut registrar = FakeRegistrar::new();
        let signal = HotkeyRuntimeSignal::new();
        let mut state = ThreadHotkeyState::new(Some(default_hotkey_spec()));
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        handle_thread_command(
            HotkeyThreadCommand::RegisterCandidate {
                transaction_id: 13,
                generation: 5,
                settings: HotkeySettings::default(),
                reply: reply_sender,
            },
            &mut registrar,
            &signal,
            &mut state,
        );
        let candidate_id = match reply_receiver.recv().expect("候选登记回执") {
            HotkeyThreadAck::CandidateRegistered { candidate_id, .. } => candidate_id,
            ack => panic!("预期候选登记成功，实际为 {ack:?}"),
        };
        registrar.unregister_outcome = UnregisterOutcome::Unknown;
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        handle_thread_command(
            HotkeyThreadCommand::DropCandidate {
                transaction_id: 13,
                generation: 5,
                candidate_id,
                reply: reply_sender,
            },
            &mut registrar,
            &signal,
            &mut state,
        );
        assert!(matches!(
            reply_receiver.recv().expect("候选清理回执"),
            HotkeyThreadAck::CandidateDropped { success: false, .. }
        ));
        assert!(state.candidate.is_none());
        assert!(state.stale_ids.contains(&candidate_id));
    }

    /// 取消事务必须留下 tombstone；迟到的 PublishActive 只能被拒绝，不能重新启用旧 ID。
    #[test]
    fn 取消事务后迟到发布被拒绝() {
        let mut registrar = FakeRegistrar::new();
        let signal = HotkeyRuntimeSignal::new();
        let mut state = ThreadHotkeyState::new(Some(default_hotkey_spec()));
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        handle_thread_command(
            HotkeyThreadCommand::RegisterCandidate {
                transaction_id: 14,
                generation: 6,
                settings: HotkeySettings::default(),
                reply: reply_sender,
            },
            &mut registrar,
            &signal,
            &mut state,
        );
        let candidate_id = match reply_receiver.recv().expect("候选登记回执") {
            HotkeyThreadAck::CandidateRegistered { candidate_id, .. } => candidate_id,
            ack => panic!("预期候选登记成功，实际为 {ack:?}"),
        };
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        handle_thread_command(
            HotkeyThreadCommand::CancelTransaction {
                transaction_id: 14,
                generation: 6,
                reply: reply_sender,
            },
            &mut registrar,
            &signal,
            &mut state,
        );
        assert!(matches!(
            reply_receiver.recv().expect("取消回执"),
            HotkeyThreadAck::Cancelled { .. }
        ));
        assert_eq!(
            state.transactions.get(&14),
            Some(&(6, ThreadTransactionState::Cancelled))
        );

        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        handle_thread_command(
            HotkeyThreadCommand::PublishActive {
                transaction_id: 14,
                generation: 6,
                candidate_id,
                settings_revision: 10,
                reply: reply_sender,
            },
            &mut registrar,
            &signal,
            &mut state,
        );
        assert!(matches!(
            reply_receiver.recv().expect("迟到发布回执"),
            HotkeyThreadAck::Cancelled { .. }
        ));
        assert_eq!(
            state.active.as_ref().map(|active| active.id),
            Some(default_hotkey_spec().id)
        );
        assert_eq!(signal.state(), HotkeyRuntimeState::ActiveOld);
    }
}

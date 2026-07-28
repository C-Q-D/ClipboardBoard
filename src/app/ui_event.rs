//! 此模块提供唯一的 UI 事件投递入口，并将可变 UI 状态限制在事件循环线程。
//!
//! `thread_local!` 是这里的刻意选择：它让后台线程拿不到 UI 状态的可变引用，
//! 只有 `invoke_from_event_loop` 执行的闭包才会触碰 reducer。后续接入 AppWindow
//! 时，Slint 属性和模型更新也必须继续放在这个闭包内。

use crate::command::{UiEvent, UiSnapshot};
use crate::AppWindow;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::thread::{self, ThreadId};

/// UI 线程独占的内部状态，外部线程不能直接取得其实例或引用。
#[derive(Default)]
struct UiState {
    snapshot: UiSnapshot,
    panel_visible: bool,
    applied_event_count: u64,
    applied_on_thread: Option<ThreadId>,
}

impl UiState {
    /// 在 UI 事件循环线程内应用一个事件并记录线程证据。
    fn apply(&mut self, event: UiEvent) {
        self.applied_event_count += 1;
        self.applied_on_thread = Some(thread::current().id());

        match event {
            UiEvent::OpenPanel => self.panel_visible = true,
            UiEvent::ReplaceSnapshot(snapshot) => self.snapshot = snapshot,
            UiEvent::SetPanelVisible(visible) => self.panel_visible = visible,
        }
    }

    /// 复制出不可变观测结果，避免把内部可变引用暴露给调用方。
    fn snapshot(&self) -> UiStateSnapshot {
        UiStateSnapshot {
            snapshot: self.snapshot.clone(),
            panel_visible: self.panel_visible,
            applied_event_count: self.applied_event_count,
            applied_on_thread: self.applied_on_thread,
        }
    }
}

thread_local! {
    /// 每个线程各自持有状态；只有运行 Slint 事件循环的线程会收到后台提交的事件。
    static UI_STATE: RefCell<UiState> = RefCell::new(UiState::default());
    /// UI 线程持有的弱窗口引用，避免事件入口形成窗口强引用环。
    static UI_WINDOW: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
}

/// 可安全跨线程读取的 UI 状态快照，不包含任何 UI 引用或 Slint 对象。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiStateSnapshot {
    /// 当前历史展示快照。
    pub snapshot: UiSnapshot,
    /// 当前看板可见性。
    pub panel_visible: bool,
    /// 已由 UI reducer 应用的事件数量。
    pub applied_event_count: u64,
    /// 最后一次 reducer 执行所在的线程，用于测试线程所有权。
    pub applied_on_thread: Option<ThreadId>,
}

/// 在 UI 线程登记主窗口弱引用，后续 `OpenPanel` 事件只在 UI 闭包内升级它。
pub fn bind_app_window(window: &AppWindow) {
    UI_WINDOW.with(|target| {
        *target.borrow_mut() = Some(window.as_weak());
    });
}

/// 将后台结果排入 Slint 事件循环；项目中所有后台到 UI 的路径都必须调用此函数。
///
/// 该函数只接受拥有型 `UiEvent`，不会同步执行 reducer。返回 `Ok(())` 只代表事件
/// 已成功进入队列，实际状态更新要等事件循环运行到该闭包后才发生。
pub fn post_ui_event(event: UiEvent) -> Result<(), slint::EventLoopError> {
    slint::invoke_from_event_loop(move || {
        let should_open_panel = matches!(&event, UiEvent::OpenPanel);
        UI_STATE.with(|state| state.borrow_mut().apply(event));

        if should_open_panel {
            UI_WINDOW.with(|target| {
                let weak_window = target.borrow().clone();
                if let Some(window) = weak_window.and_then(|weak| weak.upgrade()) {
                    if let Err(error) = window.show() {
                        eprintln!("无法显示剪贴板看板：{error}");
                    }
                }
            });
        }
    })
}

/// 读取当前线程的 UI 状态快照；生产调用方应只在 UI 事件闭包内使用此函数。
pub fn ui_state_snapshot() -> UiStateSnapshot {
    UI_STATE.with(|state| state.borrow().snapshot())
}

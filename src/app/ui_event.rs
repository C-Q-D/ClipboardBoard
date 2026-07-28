//! 此模块提供唯一的 UI 事件投递入口，并将可变 UI 状态限制在事件循环线程。
//!
//! `thread_local!` 是这里的刻意选择：它让后台线程拿不到 UI 状态的可变引用，
//! 只有 `invoke_from_event_loop` 执行的闭包才会触碰 reducer。窗口显示、隐藏、位置和
//! 目标窗口快照也必须在这个 UI 线程闭包内完成，避免原生消息线程直接碰 Slint 对象。

use crate::command::{UiEvent, UiSnapshot};
use crate::AppWindow;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::thread::{self, ThreadId};
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use crate::platform::windows::window::{
    capture_target, center_position, cursor_work_area, move_panel, panel_hwnd, panel_size,
    PanelTarget,
};
#[cfg(windows)]
use slint::PhysicalPosition;

/// reducer 应用事件后交给 UI 窗口的最小副作用集合。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UiAction {
    /// 显示面板，并在显示前完成目标快照与工作区定位。
    Show,
    /// 隐藏面板；实际调用必须仍在 UI 线程执行。
    Hide,
    /// 该事件只改变数据或已经过期，不触发窗口副作用。
    None,
    /// 退出 Slint 事件循环；只允许第一次 Quit 事件触发。
    Quit,
}

/// UI 线程独占的内部状态，外部线程不能直接取得其实例或引用。
#[derive(Default)]
struct UiState {
    snapshot: UiSnapshot,
    panel_visible: bool,
    /// 每次成功处理打开请求都会递增，用来隔离旧的 Esc/失焦事件。
    panel_generation: u64,
    /// 退出请求的一次性闩锁；置位后拒绝所有后续 UI 事件。
    quitting: bool,
    #[cfg(windows)]
    /// 仅在 UI 线程持有的目标身份；后续粘贴前必须重新查询并比较。
    panel_target: Option<PanelTarget>,
    applied_event_count: u64,
    applied_on_thread: Option<ThreadId>,
}

impl UiState {
    /// 在 UI 事件循环线程内应用一个事件并记录线程证据。
    fn apply(&mut self, event: UiEvent) -> UiAction {
        self.applied_event_count += 1;
        self.applied_on_thread = Some(thread::current().id());

        // 退出后不再允许旧热键、托盘或后台结果改变 UI 状态，避免清理阶段重新打开窗口。
        if self.quitting {
            return UiAction::None;
        }

        match event {
            UiEvent::OpenPanel => {
                if self.panel_visible {
                    self.panel_visible = false;
                    #[cfg(windows)]
                    {
                        self.panel_target = None;
                    }
                    UiAction::Hide
                } else {
                    // 饱和递增保证长时间运行后不会回到零，从而避免旧事件碰巧匹配新代次。
                    self.panel_generation = self.panel_generation.saturating_add(1).max(1);
                    self.panel_visible = true;
                    UiAction::Show
                }
            }
            UiEvent::ShowPanel => {
                if self.panel_visible {
                    UiAction::None
                } else {
                    self.panel_generation = self.panel_generation.saturating_add(1).max(1);
                    self.panel_visible = true;
                    UiAction::Show
                }
            }
            UiEvent::Quit => {
                self.quitting = true;
                self.panel_visible = false;
                #[cfg(windows)]
                {
                    self.panel_target = None;
                }
                UiAction::Quit
            }
            UiEvent::HidePanel { generation } => {
                if self.panel_visible && generation == self.panel_generation {
                    self.panel_visible = false;
                    #[cfg(windows)]
                    {
                        self.panel_target = None;
                    }
                    UiAction::Hide
                } else {
                    // 旧代次的失焦回调只能被记录，不能关闭新一轮面板。
                    UiAction::None
                }
            }
            UiEvent::ReplaceSnapshot(snapshot) => {
                self.snapshot = snapshot;
                UiAction::None
            }
        }
    }

    /// 返回当前打开代次，回调据此生成不会误伤新面板的关闭事件。
    fn panel_generation(&self) -> u64 {
        self.panel_generation
    }

    #[cfg(windows)]
    /// 保存当前打开代次的目标身份；显示失败时也必须清空，避免残留句柄被复用。
    fn set_panel_target(&mut self, target: Option<PanelTarget>) {
        if self.panel_visible {
            self.panel_target = target;
        }
    }

    /// 窗口显示失败时回滚可见状态，但保留代次单调性。
    fn mark_show_failed(&mut self) {
        self.panel_visible = false;
        #[cfg(windows)]
        {
            self.panel_target = None;
        }
    }

    /// 复制出不可变观测结果，避免把内部可变引用暴露给调用方。
    fn snapshot(&self) -> UiStateSnapshot {
        UiStateSnapshot {
            snapshot: self.snapshot.clone(),
            panel_visible: self.panel_visible,
            panel_generation: self.panel_generation,
            quitting: self.quitting,
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
    /// 当前面板打开代次；只用于验证关闭事件是否仍属于当前实例。
    pub panel_generation: u64,
    /// 是否已经接受退出请求；用于验证退出闩锁拒绝迟到事件。
    pub quitting: bool,
    /// 已由 UI reducer 应用的事件数量。
    pub applied_event_count: u64,
    /// 最后一次 reducer 执行所在的线程，用于测试线程所有权。
    pub applied_on_thread: Option<ThreadId>,
}

/// 在 UI 线程登记主窗口弱引用，并把 Slint 的 Esc/失焦回调接入代次关闭协议。
pub fn bind_app_window(window: &AppWindow) {
    UI_WINDOW.with(|target| {
        *target.borrow_mut() = Some(window.as_weak());
    });

    window.on_panel_dismiss_requested(|| {
        let generation = current_panel_generation();
        if let Err(error) = post_ui_event(UiEvent::HidePanel { generation }) {
            eprintln!("面板关闭事件无法进入 UI 事件队列：{error}");
        }
    });
}

/// 将后台结果排入 Slint 事件循环；项目中所有后台到 UI 的路径都必须调用此函数。
///
/// 该函数只接受拥有型 `UiEvent`，不会同步执行 reducer。返回 `Ok(())` 只代表事件
/// 已成功进入队列，实际状态更新要等事件循环运行到该闭包后才发生。
pub fn post_ui_event(event: UiEvent) -> Result<(), slint::EventLoopError> {
    slint::invoke_from_event_loop(move || {
        let action = UI_STATE.with(|state| state.borrow_mut().apply(event));

        if action == UiAction::Quit {
            // 退出调用必须在 Slint 事件线程执行，后台 Win32 回调只负责投递事件。
            if let Err(error) = slint::quit_event_loop() {
                eprintln!("退出 Slint 事件循环失败：{error}");
            }
            return;
        }

        UI_WINDOW.with(|target| {
            let weak_window = target.borrow().clone();
            let Some(window) = weak_window.and_then(|weak| weak.upgrade()) else {
                return;
            };

            match action {
                UiAction::Show => {
                    #[cfg(windows)]
                    prepare_panel_show();

                    match window.show() {
                        Ok(()) => {
                            #[cfg(windows)]
                            schedule_panel_position(&window, current_panel_generation(), 3);
                        }
                        Err(error) => {
                            UI_STATE.with(|state| state.borrow_mut().mark_show_failed());
                            eprintln!("无法显示剪贴板看板：{error}");
                        }
                    }
                }
                UiAction::Hide => {
                    if let Err(error) = window.hide() {
                        eprintln!("无法隐藏剪贴板看板：{error}");
                    }
                }
                UiAction::None => {}
                // Quit 已在上方提前返回；此分支仅用于让枚举匹配保持显式完整。
                UiAction::Quit => unreachable!("退出动作必须在窗口副作用前处理"),
            }
        });
    })
}

/// 读取当前面板代次；Slint 回调在 UI 线程运行，因此不需要跨线程锁。
pub fn current_panel_generation() -> u64 {
    UI_STATE.with(|state| state.borrow().panel_generation())
}

#[cfg(windows)]
/// 在 UI 线程显示面板前保存目标窗口，并按鼠标所在显示器工作区定位。
fn prepare_panel_show() {
    let target = capture_target(panel_hwnd());
    UI_STATE.with(|state| state.borrow_mut().set_panel_target(target));
}

#[cfg(windows)]
/// 在窗口真正显示后设置物理坐标，避免部分 Windows 后端用默认位置覆盖预定位结果。
fn position_panel(window: &AppWindow) -> bool {
    let slint_size = window.window().size();
    let (width, height) = panel_size().unwrap_or((slint_size.width, slint_size.height));
    if let Some(area) = cursor_work_area() {
        let position = center_position(area, width, height);
        // Winit 的首次显示可能异步创建 HWND；找到原生窗口后只保留一个物理位置来源。
        let moved = move_panel(position);
        if moved {
            return true;
        }
        // HWND 尚未创建时先写入 Slint 属性，定时器下一轮会用 Win32 位置覆盖它。
        window
            .window()
            .set_position(PhysicalPosition::new(position.x, position.y));
    }
    false
}

#[cfg(windows)]
/// 在面板 HWND 真正创建后重试物理定位，并用代次防止旧定时器移动新一轮或已隐藏的面板。
fn schedule_panel_position(window: &AppWindow, generation: u64, remaining_attempts: u8) {
    let weak_window = window.as_weak();
    slint::Timer::single_shot(Duration::from_millis(16), move || {
        let is_current = UI_STATE.with(|state| {
            let state = state.borrow();
            state.panel_visible && state.panel_generation() == generation
        });
        if !is_current {
            return;
        }

        let Some(window) = weak_window.upgrade() else {
            return;
        };
        if !position_panel(&window) && remaining_attempts > 0 {
            schedule_panel_position(&window, generation, remaining_attempts - 1);
        }
    });
}

/// 读取当前线程的 UI 状态快照；生产调用方应只在 UI 事件闭包内使用此函数。
pub fn ui_state_snapshot() -> UiStateSnapshot {
    UI_STATE.with(|state| state.borrow().snapshot())
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证面板代次协议，确保旧的关闭事件不会误关闭新面板。

    use super::{UiAction, UiState};
    use crate::command::UiEvent;

    /// 打开两轮面板后，第一轮的关闭事件必须被 reducer 拒绝。
    #[test]
    fn 过期关闭事件不会关闭新代次() {
        let mut state = UiState::default();

        assert_eq!(state.apply(UiEvent::OpenPanel), UiAction::Show);
        let first_generation = state.panel_generation();
        assert_eq!(
            state.apply(UiEvent::HidePanel {
                generation: first_generation
            }),
            UiAction::Hide
        );
        assert_eq!(state.apply(UiEvent::OpenPanel), UiAction::Show);
        let second_generation = state.panel_generation();
        assert!(second_generation > first_generation);

        assert_eq!(
            state.apply(UiEvent::HidePanel {
                generation: first_generation,
            }),
            UiAction::None
        );
        assert!(state.panel_visible);
        assert_eq!(
            state.apply(UiEvent::HidePanel {
                generation: second_generation,
            }),
            UiAction::Hide
        );
        assert!(!state.panel_visible);
    }

    /// 托盘打开必须是幂等显示，不能像热键一样把已显示面板再次隐藏。
    #[test]
    fn 托盘打开幂等显示面板() {
        let mut state = UiState::default();

        assert_eq!(state.apply(UiEvent::ShowPanel), UiAction::Show);
        let generation = state.panel_generation();
        assert_eq!(state.apply(UiEvent::ShowPanel), UiAction::None);
        assert!(state.panel_visible);
        assert_eq!(state.panel_generation(), generation);
    }

    /// 第一次退出后，迟到的打开和关闭事件都必须被 reducer 拒绝。
    #[test]
    fn 退出闩锁拒绝后续事件() {
        let mut state = UiState::default();

        assert_eq!(state.apply(UiEvent::Quit), UiAction::Quit);
        assert!(state.quitting);
        assert!(!state.panel_visible);
        assert_eq!(state.apply(UiEvent::ShowPanel), UiAction::None);
        assert_eq!(state.apply(UiEvent::Quit), UiAction::None);
        assert_eq!(
            state.apply(UiEvent::HidePanel { generation: 1 }),
            UiAction::None
        );
        assert!(!state.panel_visible);
    }
}

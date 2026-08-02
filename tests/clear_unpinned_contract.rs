//! 此集成测试验证“清理未收藏”从底部统一入口进入覆盖层、二次确认和 pending 禁用契约。
//!
//! 测试只观察 Slint 真实命中区和回调；删除范围、事务提交、修订号隔离和失败保留由
//! `ui_event`、`history_mutation` 与存储层的定向测试负责。

use clipboard_board::create_app_window;
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition};
use std::cell::Cell;
use std::rc::Rc;

/// 底部状态行的“清理历史”按钮坐标；它不随确认内容改变，因此是稳定的统一入口接缝。
const CLEANUP_ENTRY: (f32, f32) = (250.0, 475.0);
/// 覆盖层初始菜单中“清空未收藏”的真实命中区中心。
const MENU_UNPINNED: (f32, f32) = (252.0, 297.0);
/// 未收藏确认区的取消和确认按钮中心。
const UNPINNED_CANCEL: (f32, f32) = (400.0, 307.0);
const UNPINNED_CONFIRM: (f32, f32) = (480.0, 307.0);

/// 在指定逻辑坐标发送一次完整左键点击，确保命中区经过真实窗口事件分发。
fn click(window: &clipboard_board::AppWindow, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    window
        .window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    window.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    window
        .window()
        .dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
}

/// 通过固定底部入口打开统一清理覆盖层，避免测试绕过产品真实入口直接设置开关。
fn open_cleanup_panel(window: &clipboard_board::AppWindow) {
    click(window, CLEANUP_ENTRY.0, CLEANUP_ENTRY.1);
    assert!(
        window.get_cleanup_panel_open(),
        "底部清理入口必须真实打开覆盖层"
    );
}

/// 清理未收藏必须先展示确认；取消和确认拥有独立回调，pending 时所有清理入口均不可重复提交。
#[test]
fn 清空未收藏需要二次确认且处理中禁用入口() {
    i_slint_backend_testing::init_integration_test_with_mock_time();
    let window = create_app_window().expect("测试窗口应成功创建");

    let requests = Rc::new(Cell::new(0_u32));
    let requests_for_callback = Rc::clone(&requests);
    window.on_clear_unpinned_requested(move || {
        requests_for_callback.set(requests_for_callback.get() + 1);
    });
    let cancellations = Rc::new(Cell::new(0_u32));
    let cancellations_for_callback = Rc::clone(&cancellations);
    window.on_clear_unpinned_cancelled(move || {
        cancellations_for_callback.set(cancellations_for_callback.get() + 1);
    });
    let confirmations = Rc::new(Cell::new(0_u32));
    let confirmations_for_callback = Rc::clone(&confirmations);
    window.on_clear_unpinned_confirmed(move || {
        confirmations_for_callback.set(confirmations_for_callback.get() + 1);
    });

    window.show().expect("测试窗口应成功显示");
    open_cleanup_panel(&window);
    click(&window, MENU_UNPINNED.0, MENU_UNPINNED.1);
    assert_eq!(requests.get(), 1);
    assert_eq!(cancellations.get(), 0);
    assert_eq!(confirmations.get(), 0);

    window.set_clear_unpinned_confirmation_visible(true);
    click(&window, UNPINNED_CANCEL.0, UNPINNED_CANCEL.1);
    click(&window, UNPINNED_CONFIRM.0, UNPINNED_CONFIRM.1);
    assert_eq!(cancellations.get(), 1);
    assert_eq!(confirmations.get(), 1);
    assert!(
        !window.get_cleanup_panel_open(),
        "确认提交后覆盖层必须允许本地关闭"
    );

    // 直接重开只用于构造 pending 状态；按钮仍须由 history-mutation-pending 统一门禁。
    window.set_clear_unpinned_confirmation_visible(false);
    window.set_clear_unpinned_pending(true);
    window.set_history_mutation_pending(true);
    window.set_cleanup_panel_open(true);
    click(&window, MENU_UNPINNED.0, MENU_UNPINNED.1);
    assert_eq!(requests.get(), 1);
    window.set_clear_unpinned_confirmation_visible(true);
    click(&window, UNPINNED_CONFIRM.0, UNPINNED_CONFIRM.1);
    assert_eq!(confirmations.get(), 1);
}

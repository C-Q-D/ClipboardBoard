//! 此集成测试验证“清理未收藏”入口、二次确认和 pending 禁用的界面契约。
//!
//! 测试只观察 Slint 回调；删除范围、事务提交、修订号隔离和失败保留由
//! `ui_event`、`history_mutation` 与存储层的定向测试负责。

use clipboard_board::create_app_window;
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition};
use std::cell::Cell;
use std::rc::Rc;

/// 在指定逻辑坐标发送一次完整左键点击。
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

/// 清理入口只请求打开确认区，取消和确认拥有独立回调，pending 时入口不可重复提交。
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
    click(&window, 380.0, 128.0);
    assert_eq!(requests.get(), 1);
    assert_eq!(cancellations.get(), 0);
    assert_eq!(confirmations.get(), 0);

    window.set_clear_unpinned_confirmation_visible(true);
    click(&window, 419.0, 176.0);
    click(&window, 490.0, 176.0);
    assert_eq!(cancellations.get(), 1);
    assert_eq!(confirmations.get(), 1);

    window.set_clear_unpinned_confirmation_visible(false);
    window.set_clear_unpinned_pending(true);
    window.set_history_mutation_pending(true);
    click(&window, 380.0, 128.0);
    assert_eq!(requests.get(), 1);
    window.set_clear_unpinned_confirmation_visible(true);
    click(&window, 490.0, 176.0);
    assert_eq!(confirmations.get(), 1);
}

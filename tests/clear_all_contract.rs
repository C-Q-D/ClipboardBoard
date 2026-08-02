//! 此集成测试验证“清空全部”从统一清理覆盖层进入、精确短语门禁和 pending 禁用契约。
//!
//! 测试只观察 Slint 真实命中区和回调；事务范围、修订号、迟到结果和失败回滚由
//! `storage`、`history_mutation` 与 UI reducer 的定向测试负责。

use clipboard_board::create_app_window;
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, SharedString};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// 底部状态行和覆盖层菜单的固定命中区；确认弹层不会改变统一入口的位置。
const CLEANUP_ENTRY: (f32, f32) = (250.0, 475.0);
const MENU_UNPINNED: (f32, f32) = (252.0, 297.0);
const MENU_ALL: (f32, f32) = (358.0, 297.0);
/// 清空全部确认区输入框与按钮中心。
const ALL_CONFIRM_INPUT: (f32, f32) = (300.0, 250.0);
const ALL_CANCEL: (f32, f32) = (400.0, 301.0);
const ALL_CONFIRM: (f32, f32) = (480.0, 301.0);

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

/// 通过固定底部入口打开统一清理覆盖层，验证测试使用的是完整用户路径。
fn open_cleanup_panel(window: &clipboard_board::AppWindow) {
    click(window, CLEANUP_ENTRY.0, CLEANUP_ENTRY.1);
    assert!(
        window.get_cleanup_panel_open(),
        "底部清理入口必须真实打开覆盖层"
    );
}

/// 清空全部必须逐字匹配确认短语；输入错误、取消和 pending 均不得产生危险提交。
#[test]
fn 清空全部需要精确文字强确认且处理中禁用入口() {
    i_slint_backend_testing::init_integration_test_with_mock_time();
    let window = create_app_window().expect("测试窗口应成功创建");

    let requests = Rc::new(Cell::new(0_u32));
    let requests_for_callback = Rc::clone(&requests);
    window.on_clear_all_requested(move || {
        requests_for_callback.set(requests_for_callback.get() + 1);
    });
    let cancellations = Rc::new(Cell::new(0_u32));
    let cancellations_for_callback = Rc::clone(&cancellations);
    window.on_clear_all_cancelled(move || {
        cancellations_for_callback.set(cancellations_for_callback.get() + 1);
    });
    let confirmations = Rc::new(RefCell::new(Vec::<String>::new()));
    let confirmations_for_callback = Rc::clone(&confirmations);
    window.on_clear_all_confirmed(move |text| {
        confirmations_for_callback
            .borrow_mut()
            .push(text.to_string());
    });
    let unpinned_requests = Rc::new(Cell::new(0_u32));
    let unpinned_requests_for_callback = Rc::clone(&unpinned_requests);
    window.on_clear_unpinned_requested(move || {
        unpinned_requests_for_callback.set(unpinned_requests_for_callback.get() + 1);
    });

    window.show().expect("测试窗口应成功显示");
    open_cleanup_panel(&window);
    click(&window, MENU_ALL.0, MENU_ALL.1);
    assert_eq!(requests.get(), 1);

    window.set_clear_all_confirmation_visible(true);
    window.set_clear_all_confirmation_text(SharedString::from("清空全部 "));
    click(&window, ALL_CONFIRM_INPUT.0, ALL_CONFIRM_INPUT.1);
    click(&window, ALL_CONFIRM.0, ALL_CONFIRM.1);
    assert!(confirmations.borrow().is_empty());

    window.set_clear_all_confirmation_text(SharedString::from("清空全部"));
    click(&window, ALL_CONFIRM.0, ALL_CONFIRM.1);
    assert_eq!(confirmations.borrow().as_slice(), &["清空全部".to_owned()]);
    assert!(
        !window.get_cleanup_panel_open(),
        "确认提交后覆盖层必须允许本地关闭"
    );

    // 重新显示确认区验证取消命中区仍然独立，不与确认按钮共享回调。
    window.set_cleanup_panel_open(true);
    window.set_clear_all_confirmation_visible(true);
    click(&window, ALL_CANCEL.0, ALL_CANCEL.1);
    assert_eq!(cancellations.get(), 1);

    // pending 由统一 history-mutation-pending 门禁控制，不能从任一范围绕过。
    window.set_clear_all_confirmation_visible(false);
    window.set_clear_all_pending(true);
    window.set_history_mutation_pending(true);
    window.set_cleanup_panel_open(true);
    click(&window, MENU_ALL.0, MENU_ALL.1);
    assert_eq!(requests.get(), 1);
    click(&window, MENU_UNPINNED.0, MENU_UNPINNED.1);
    assert_eq!(unpinned_requests.get(), 0);
}

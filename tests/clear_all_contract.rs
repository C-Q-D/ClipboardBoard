//! 此集成测试验证“清空全部”的可见文字入口、精确短语门禁和 pending 禁用契约。
//!
//! 测试只观察 Slint 的独立命中区与固定文案源码；事务范围、修订号和迟到结果由
//! storage、history_mutation 与 UI reducer 的定向测试负责。

use clipboard_board::{create_app_window, ClipboardCard};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
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

/// 危险入口只打开确认；错误文字禁用确认，精确文字才发送，pending 时入口不可重复触发。
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
    let pins = Rc::new(Cell::new(0_u32));
    let pins_for_callback = Rc::clone(&pins);
    window.on_pin_item_requested(move |_index| {
        pins_for_callback.set(pins_for_callback.get() + 1);
    });
    let deletes = Rc::new(Cell::new(0_u32));
    let deletes_for_callback = Rc::clone(&deletes);
    window.on_delete_item_requested(move |_index| {
        deletes_for_callback.set(deletes_for_callback.get() + 1);
    });
    window.set_cards(ModelRc::new(VecModel::from(vec![ClipboardCard {
        preview: SharedString::from("互斥测试"),
        source: SharedString::from("测试来源"),
        relative_time: SharedString::from("刚刚"),
        is_pinned: false,
        pin_pending: false,
        delete_pending: false,
    }])));

    window.show().expect("测试窗口应成功显示");
    click(&window, 490.0, 128.0);
    assert_eq!(requests.get(), 1);

    window.set_clear_all_confirmation_visible(true);
    window.set_clear_all_confirmation_text(SharedString::from("清空全部 "));
    click(&window, 488.0, 216.0);
    assert!(confirmations.borrow().is_empty());

    window.set_clear_all_confirmation_text(SharedString::from("清空全部"));
    click(&window, 488.0, 216.0);
    assert_eq!(confirmations.borrow().as_slice(), &["清空全部".to_owned()]);

    click(&window, 405.0, 216.0);
    assert_eq!(cancellations.get(), 1);

    window.set_clear_all_confirmation_visible(false);
    window.set_clear_all_pending(true);
    window.set_history_mutation_pending(true);
    click(&window, 490.0, 128.0);
    assert_eq!(requests.get(), 1);
    click(&window, 380.0, 128.0);
    click(&window, 458.0, 260.0);
    click(&window, 410.0, 260.0);
    assert_eq!(unpinned_requests.get(), 0);
    assert_eq!(pins.get(), 0);
    assert_eq!(deletes.get(), 0);
    window.set_clear_all_confirmation_visible(true);
    window.set_clear_all_confirmation_text(SharedString::from("清空全部"));
    click(&window, 488.0, 216.0);
    assert_eq!(confirmations.borrow().len(), 1);

    let source = include_str!("../ui/app-window.slint");
    assert!(source.contains("text: clear-all-pending ? \"清空中…\" : \"清空全部\""));
    assert!(source.contains("将删除全部记录，包括已收藏记录"));
    assert!(source.contains("clear-all-confirmation-text == \"清空全部\""));
}

//! 此集成测试验证删除按钮拥有独立命中区，并在事务 pending 时禁用重复点击。
//!
//! 测试只观察 Slint 回调索引；稳定身份、事务后移除和旧查询隔离由 UI reducer
//! 与删除桥的定向单元测试负责。

use clipboard_board::{create_app_window, ClipboardCard};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
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

/// 构造只含摘要和删除视觉状态的测试卡片。
fn card(label: &str, delete_pending: bool) -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from(label),
        source: SharedString::from("测试来源"),
        relative_time: SharedString::from("刚刚"),
        is_pinned: false,
        pin_pending: false,
        delete_pending,
    }
}

/// 普通删除按钮发送对应索引，pending 按钮不发送，且均不触发选择、收藏或复制。
#[test]
fn 删除按钮命中区独立且处理中禁用重复点击() {
    i_slint_backend_testing::init_integration_test_with_mock_time();
    let window = create_app_window().expect("测试窗口应成功创建");
    window.set_cards(ModelRc::new(VecModel::from(vec![
        card("可删除", false),
        card("处理中", true),
    ])));

    let deletes = Rc::new(RefCell::new(Vec::new()));
    let deletes_for_callback = Rc::clone(&deletes);
    window.on_delete_item_requested(move |index| {
        deletes_for_callback.borrow_mut().push(index);
    });
    let selections = Rc::new(RefCell::new(Vec::new()));
    let selections_for_callback = Rc::clone(&selections);
    window.on_card_selection_requested(move |index| {
        selections_for_callback.borrow_mut().push(index);
    });
    let pins = Rc::new(RefCell::new(Vec::new()));
    let pins_for_callback = Rc::clone(&pins);
    window.on_pin_item_requested(move |index| {
        pins_for_callback.borrow_mut().push(index);
    });
    let copies = Rc::new(RefCell::new(Vec::new()));
    let copies_for_callback = Rc::clone(&copies);
    window.on_copy_item_requested(move |index| {
        copies_for_callback.borrow_mut().push(index);
    });

    window.show().expect("测试窗口应成功显示");
    click(&window, 410.0, 260.0);
    click(&window, 410.0, 366.0);

    assert_eq!(deletes.borrow().as_slice(), &[0]);
    assert!(selections.borrow().is_empty());
    assert!(pins.borrow().is_empty());
    assert!(copies.borrow().is_empty());
}

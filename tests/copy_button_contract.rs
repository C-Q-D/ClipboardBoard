//! 此集成测试验证显式复制按钮与卡片主体拥有互不重叠的鼠标命中区。
//!
//! 测试只观察 Slint 回调和面板级按键契约；稳定身份与后台 latest-wins 邮箱由
//! `ui_event` 和 `io_worker` 的定向单元测试验证。

use clipboard_board::{create_app_window, ClipboardCard};
use slint::platform::{Key, PointerEventButton, WindowEvent};
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

/// 向测试窗口发送一次完整按键，确保修饰键状态不会泄漏到后续断言。
fn send_key(window: &clipboard_board::AppWindow, key: Key) {
    let text: SharedString = key.into();
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

/// 构造不含完整正文的卡片摘要。
fn card(label: &str) -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from(label),
        source: SharedString::from("测试来源"),
        relative_time: SharedString::from("刚刚"),
        is_pinned: false,
        pin_pending: false,
    }
}

/// 点击不同复制按钮只发送一次对应索引，不触发卡片选择；Enter 无动作而 Esc 仍关闭。
#[test]
fn 复制按钮命中区独立且面板按键契约不变() {
    i_slint_backend_testing::init_integration_test_with_mock_time();
    let window = create_app_window().expect("测试窗口应成功创建");
    window.set_cards(ModelRc::new(VecModel::from(vec![
        card("第一条"),
        card("第二条"),
    ])));

    let copies = Rc::new(RefCell::new(Vec::new()));
    let copies_for_callback = Rc::clone(&copies);
    window.on_copy_item_requested(move |index| {
        copies_for_callback.borrow_mut().push(index);
    });
    let selections = Rc::new(RefCell::new(Vec::new()));
    let selections_for_callback = Rc::clone(&selections);
    window.on_card_selection_requested(move |index| {
        selections_for_callback.borrow_mut().push(index);
    });
    let dismiss_count = Rc::new(Cell::new(0_u32));
    let dismiss_for_callback = Rc::clone(&dismiss_count);
    window.on_panel_dismiss_requested(move || {
        dismiss_for_callback.set(dismiss_for_callback.get() + 1);
    });

    window.show().expect("测试窗口应成功显示");
    click(&window, 508.0, 260.0);
    click(&window, 508.0, 366.0);
    assert_eq!(copies.borrow().as_slice(), &[0, 1]);
    assert!(selections.borrow().is_empty());

    send_key(&window, Key::Return);
    assert_eq!(copies.borrow().as_slice(), &[0, 1]);
    assert_eq!(dismiss_count.get(), 0);
    send_key(&window, Key::Escape);
    assert_eq!(dismiss_count.get(), 1);
}

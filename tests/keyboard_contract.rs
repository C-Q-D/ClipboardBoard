//! 此集成测试验证面板级键盘捕获先于搜索框等内部控件执行。
//!
//! 测试验证 WCB-INT-01 的键盘契约，以及 WCB-INT-03 的失焦驻留和原生关闭拒绝；
//! 上下键不得改变当前选择，Enter 组合不得触发复制，Esc 仍负责关闭面板。

use clipboard_board::{app::bind_app_window, create_app_window};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, SharedString};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// 向测试窗口发送一次完整按键，确保修饰键状态在断言后被正确释放。
fn send_key(window: &clipboard_board::AppWindow, key: Key) {
    let text: SharedString = key.into();
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

/// 在顶部工具栏的固定搜索框区域发送一次鼠标点击，使 LineEdit 成为真实的按键目标。
fn focus_search_input(window: &clipboard_board::AppWindow) {
    // shell 内部坐标为 x=168、y=8；叠加窗口 14px 外边距和 shell 14px padding 后，
    // 该点位于当前 560px 看板的 LineEdit 内，不依赖文本或品牌区域命中。
    let position = LogicalPosition::new(300.0, 55.0);
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

/// 向当前焦点元素发送普通文本按键，用编辑回调证明搜索框确实获得了焦点。
fn send_text(window: &clipboard_board::AppWindow, text: &str) {
    for character in text.chars() {
        let text = SharedString::from(character);
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text });
    }
}

/// 验证搜索框持有焦点时，Esc 与 Enter 仍由面板处理；上下键不再改变选择。
#[test]
fn 内部焦点遵守面板级键盘契约() {
    i_slint_backend_testing::init_integration_test_with_mock_time();
    let window = create_app_window().expect("测试窗口应成功创建");
    bind_app_window(&window);

    let dismiss_count = Rc::new(Cell::new(0_u32));
    let dismiss_count_for_callback = Rc::clone(&dismiss_count);
    window.on_panel_dismiss_requested(move || {
        dismiss_count_for_callback.set(dismiss_count_for_callback.get() + 1);
    });

    let copy_count = Rc::new(Cell::new(0_u32));
    let copy_count_for_callback = Rc::clone(&copy_count);
    window.on_copy_item_requested(move |_| {
        copy_count_for_callback.set(copy_count_for_callback.get() + 1);
    });

    let edited_text = Rc::new(RefCell::new(String::new()));
    let edited_text_for_callback = Rc::clone(&edited_text);
    window.on_search_text_changed(move |text| {
        *edited_text_for_callback.borrow_mut() = text.to_string();
    });

    // 用可观察的窗口属性固定一个当前鼠标选择结果；箭头键只能交回焦点控件，不能改写它。
    window.set_selected_index(1);
    window.show().expect("测试窗口应成功显示");
    window
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(false));
    assert!(window.window().is_visible());
    window.window().dispatch_event(WindowEvent::CloseRequested);
    assert!(window.window().is_visible());
    // 搜索框位于固定顶部工具区；先点击并输入字符，以可观察回调证明内部 LineEdit 已获焦。
    focus_search_input(&window);
    send_text(&window, "x");
    assert_eq!(edited_text.borrow().as_str(), "x");

    send_key(&window, Key::Return);
    let control: SharedString = Key::Control.into();
    let shift: SharedString = Key::Shift.into();
    let enter: SharedString = Key::Return.into();
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: control.clone(),
    });
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: enter.clone(),
    });
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: enter.clone(),
    });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: control });
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: shift.clone(),
    });
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: enter.clone(),
    });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: enter });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: shift });
    assert_eq!(dismiss_count.get(), 0);
    assert_eq!(copy_count.get(), 0);
    assert_eq!(edited_text.borrow().as_str(), "x");

    send_key(&window, Key::UpArrow);
    send_key(&window, Key::DownArrow);
    assert_eq!(window.get_selected_index(), 1);
    assert_eq!(edited_text.borrow().as_str(), "x");

    send_key(&window, Key::Escape);
    assert_eq!(dismiss_count.get(), 1);
}

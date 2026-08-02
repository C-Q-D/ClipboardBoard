//! 此集成测试验证收藏按钮具有独立鼠标命中区，并在 pending 时禁用重复点击。
//!
//! 测试只观察 Slint 回调索引；稳定身份、事务提交后更新和迟到结果隔离由
//! `ui_event` 与 `history_mutation` 的定向单元测试负责。

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

/// 构造只含摘要和收藏视觉状态的测试卡片。
fn card(label: &str, is_pinned: bool, pin_pending: bool) -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from(label),
        source: SharedString::from("测试来源"),
        relative_time: SharedString::from("刚刚"),
        is_pinned,
        pin_pending,
        delete_pending: false,
        // 收藏命中区测试不依赖图片和复制行为，但必须填满当前 UI DTO 的安全默认值。
        is_image: false,
        copy_enabled: true,
        image_width: 0,
        image_height: 0,
        thumbnail: Default::default(),
        thumbnail_loaded: false,
        thumbnail_failed: false,
    }
}

/// 普通收藏按钮发送对应索引，pending 按钮不发送，且两者都不触发选择或复制。
#[test]
fn 收藏按钮命中区独立且处理中禁用重复点击() {
    i_slint_backend_testing::init_integration_test_with_mock_time();
    let window = create_app_window().expect("测试窗口应成功创建");
    window.set_cards(ModelRc::new(VecModel::from(vec![
        card("可收藏", false, false),
        card("处理中", true, true),
    ])));

    let pins = Rc::new(RefCell::new(Vec::new()));
    let pins_for_callback = Rc::clone(&pins);
    window.on_pin_item_requested(move |index| {
        pins_for_callback.borrow_mut().push(index);
    });
    let selections = Rc::new(RefCell::new(Vec::new()));
    let selections_for_callback = Rc::clone(&selections);
    window.on_card_selection_requested(move |index| {
        selections_for_callback.borrow_mut().push(index);
    });
    let copies = Rc::new(RefCell::new(Vec::new()));
    let copies_for_callback = Rc::clone(&copies);
    window.on_copy_item_requested(move |index| {
        copies_for_callback.borrow_mut().push(index);
    });

    window.show().expect("测试窗口应成功显示");
    // 当前 560px 看板的 legacy 命中区需要从真实事件结果校正：首条非 pending
    // 收藏命中点为 x=458，第二条 pending 命中点为 x=430；两行高度相差 106px。
    const PIN_BUTTON_X: f32 = 458.0;
    const PENDING_PIN_BUTTON_X: f32 = 430.0;
    click(&window, PIN_BUTTON_X, 260.0);
    click(&window, PENDING_PIN_BUTTON_X, 366.0);

    assert_eq!(
        pins.borrow().as_slice(),
        &[0],
        "收藏回调记录：{:?}",
        pins.borrow()
    );
    assert!(
        selections.borrow().is_empty(),
        "选择回调记录：{:?}",
        selections.borrow()
    );
    assert!(
        copies.borrow().is_empty(),
        "复制回调记录：{:?}",
        copies.borrow()
    );
}

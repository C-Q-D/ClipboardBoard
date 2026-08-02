//! 此集成测试验证真实 Slint 卡片代理把鼠标点击转换为可见索引。
//!
//! 测试只覆盖 WCB-INT-05 的卡片点击边界：卡片区域发送索引，空白区域不发送，
//! 记录身份与异步代次校验由 UI reducer 的单元测试负责。

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

/// 构造只含摘要的测试卡片，避免把完整剪贴板正文带入组件树。
fn card(label: &str) -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from(label),
        source: SharedString::from("测试来源"),
        relative_time: SharedString::from("刚刚"),
        is_pinned: false,
        pin_pending: false,
        delete_pending: false,
        // 选择边界测试只观察摘要命中，图片和复制字段使用稳定安全默认值。
        is_image: false,
        copy_enabled: true,
        image_width: 0,
        image_height: 0,
        thumbnail: Default::default(),
        thumbnail_loaded: false,
        thumbnail_failed: false,
    }
}

/// 点击首、中、末卡片必须返回各自索引，卡片间隙和预留操作区不能伪造选择事件。
#[test]
fn 卡片点击产生可见索引且空白区无动作() {
    i_slint_backend_testing::init_integration_test_with_mock_time();
    let window = create_app_window().expect("测试窗口应成功创建");
    window.set_cards(ModelRc::new(VecModel::from(vec![
        card("第一条"),
        card("第二条"),
        card("第三条"),
    ])));

    let selected = Rc::new(RefCell::new(Vec::new()));
    let selected_for_callback = Rc::clone(&selected);
    window.on_card_selection_requested(move |index| {
        selected_for_callback.borrow_mut().push(index);
    });

    window.show().expect("测试窗口应成功显示");
    // 720×520 双栏布局中左栏历史列表从约 242px 开始，每张文本代理外层高度为 78px。
    click(&window, 100.0, 250.0);
    click(&window, 100.0, 328.0);
    // 第一张卡片背景后的 8px 透明间隔和左栏右侧预留操作区都不属于选择点击区。
    click(&window, 100.0, 321.0);
    assert_eq!(selected.borrow().as_slice(), &[0, 1]);

    // 视口不足以同时显示第三项时，沿用生产 setter 滚动一个文本行高后再点击末项；
    // 测试仍验证真实 TouchArea 产生的稳定索引，而不是直接调用选择回调。
    window.set_history_viewport_y(-78.0);
    click(&window, 100.0, 410.0);
    assert_eq!(selected.borrow().as_slice(), &[0, 1, 2]);
    click(&window, 280.0, 250.0);

    assert_eq!(selected.borrow().as_slice(), &[0, 1, 2]);
}

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
    let cards = (0..8)
        .map(|index| card(&format!("第{index}条")))
        .collect::<Vec<_>>();
    window.set_cards(ModelRc::new(VecModel::from(cards)));

    let selected = Rc::new(RefCell::new(Vec::new()));
    let selected_for_callback = Rc::clone(&selected);
    window.on_card_selection_requested(move |index| {
        selected_for_callback.borrow_mut().push(index);
    });

    window.show().expect("测试窗口应成功显示");
    // 720×520 双栏布局中左栏历史列表从约 242px 开始，每张文本代理外层高度为 78px。
    click(&window, 100.0, 250.0);
    click(&window, 100.0, 328.0);
    // 当前 mock 布局的第一项背景结束于约 y=271，第二项从约 y=280 开始；
    // y=275 落在两项之间的真实透明间隔，不能使用旧高度夹具中的 y=321。
    click(&window, 100.0, 275.0);
    assert_eq!(selected.borrow().as_slice(), &[0, 1]);

    // 视口不足以同时显示第三项时，沿用生产 setter 滚动一个文本行高后再点击末项；
    // 测试仍验证真实 TouchArea 产生的稳定索引，而不是直接调用选择回调。
    window.set_history_viewport_y(-78.0);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    // 78px 文本外层在该 mock 视口中第三项的可见背景覆盖约 y=280..344；
    // 选取其内部坐标，避免旧 106px 夹具的 y=350 落入透明间隔。
    click(&window, 100.0, 330.0);
    assert_eq!(selected.borrow().as_slice(), &[0, 1, 2]);
    // 最终左栏宽度为 264px；x=310 已在分隔线右侧，不应伪造历史行选择。
    click(&window, 310.0, 250.0);

    assert_eq!(selected.borrow().as_slice(), &[0, 1, 2]);
}

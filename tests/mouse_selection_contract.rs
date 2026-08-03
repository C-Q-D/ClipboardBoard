//! 此集成测试验证真实 Slint 卡片代理把鼠标点击转换为可见索引。
//!
//! 测试覆盖首项、次项、视觉底部留白和滚动后的卡片边界；分隔线外的点击不发送选择，
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
    // 当前 720×520 双栏布局中历史首行约从 y=142 开始，每个文本代理外层固定 78px。
    click(&window, 100.0, 175.0);
    click(&window, 100.0, 253.0);
    // 首行背景在约 y=212 结束，但其外层底部 8px 仍属于首行命中区，不能被视觉留白吞掉。
    click(&window, 100.0, 218.0);
    assert_eq!(selected.borrow().as_slice(), &[0, 1, 0]);

    // 视口不足以同时显示第三项时，沿用生产 setter 滚动一个文本行高后再点击末项；
    // 测试仍验证真实 TouchArea 产生的稳定索引，而不是直接调用选择回调。
    window.set_history_viewport_y(-78.0);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    // 78px 文本外层在该 mock 视口中第三项覆盖约 y=242..320，选择其内部坐标。
    click(&window, 100.0, 253.0);
    assert_eq!(selected.borrow().as_slice(), &[0, 1, 0, 2]);
    // 最终左栏宽度为 264px；x=310 已在分隔线右侧，不应伪造历史行选择。
    click(&window, 310.0, 250.0);

    assert_eq!(selected.borrow().as_slice(), &[0, 1, 0, 2]);
}

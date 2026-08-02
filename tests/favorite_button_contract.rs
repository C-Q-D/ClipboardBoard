//! 此集成测试验证收藏操作已经迁移到右栏，并保持 pending 禁用契约。
//!
//! 测试通过真实鼠标事件先选择第二项，再点击右栏“取消收藏”；回调不携带索引，
//! 收藏目标的明确 `is_pinned` 由 UI 线程从完整快照冻结。

use clipboard_board::{create_app_window, ClipboardCard};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// 在指定逻辑坐标发送一次完整左键点击，确保命中真实控件而非直接调用回调。
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

/// 构造只包含摘要的收藏卡片；pending 状态由 Rust mutation 结果同步，不在按钮中切换。
fn card(label: &str, is_pinned: bool, pin_pending: bool) -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from(label),
        source: SharedString::from("测试来源"),
        relative_time: SharedString::from("刚刚"),
        is_pinned,
        pin_pending,
        delete_pending: false,
        is_image: false,
        copy_enabled: true,
        image_width: 0,
        image_height: 0,
        thumbnail: Default::default(),
        thumbnail_loaded: false,
        thumbnail_failed: false,
    }
}

/// 选择第二项后同步投影，模拟生产 reducer 对完整快照选中项的单向绑定。
fn bind_selection_projection(
    window: &clipboard_board::AppWindow,
    cards: &[ClipboardCard],
    selected: Rc<RefCell<Vec<i32>>>,
) {
    let weak_window = window.as_weak();
    let cards = cards.to_vec();
    window.on_card_selection_requested(move |index| {
        selected.borrow_mut().push(index);
        let Some(window) = weak_window.upgrade() else {
            return;
        };
        let Some(card) = cards.get(index as usize) else {
            return;
        };
        window.set_selected_index(index);
        window.set_selected_card(card.clone());
        window.set_has_selected_card(true);
    });
}

/// 右栏收藏命中当前第二项；pending 后重复点击必须被真实 TouchArea 禁止。
#[test]
fn 先选择第二项再点击右栏收藏且处理中禁用重复点击() {
    i_slint_backend_testing::init_integration_test_with_mock_time();
    let window = create_app_window().expect("测试窗口应成功创建");
    let cards = vec![card("第一条", false, false), card("第二条", true, false)];
    window.set_cards(ModelRc::new(VecModel::from(cards.clone())));

    let selected = Rc::new(RefCell::new(Vec::new()));
    bind_selection_projection(&window, &cards, Rc::clone(&selected));
    let pins = Rc::new(Cell::new(0_u32));
    let pins_for_callback = Rc::clone(&pins);
    window.on_selected_pin_requested(move || {
        pins_for_callback.set(pins_for_callback.get() + 1);
    });

    window.show().expect("测试窗口应成功显示");
    click(&window, 100.0, 328.0);
    click(&window, 332.0, 465.0);
    assert_eq!(selected.borrow().as_slice(), &[1]);
    assert_eq!(pins.get(), 1);

    window.set_selected_card(card("第二条", true, true));
    click(&window, 332.0, 465.0);
    assert_eq!(pins.get(), 1);
}

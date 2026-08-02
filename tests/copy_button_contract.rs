//! 此集成测试验证复制操作已经迁移到右栏，并且必须先选择当前记录。
//!
//! 测试通过真实鼠标事件先选择第二项，再点击右栏按钮；按钮回调无索引参数，
//! 稳定 ID/哈希的冻结和后台正文读取由 `ui_event` 与 Windows 复制桥负责。

use clipboard_board::{create_app_window, ClipboardCard};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// 在指定逻辑坐标发送一次完整左键点击，覆盖真实 TouchArea 的按下和释放路径。
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

/// 构造不含完整正文的文本摘要；右栏复制按钮只发送用户意图，不携带正文。
fn card(label: &str) -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from(label),
        source: SharedString::from("测试来源"),
        relative_time: SharedString::from("刚刚"),
        is_pinned: false,
        pin_pending: false,
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

/// 选择第二项后同步更新测试投影，模拟生产 Rust reducer 已消费选择事件的结果。
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

/// 复制必须作用于鼠标刚选中的第二项，且右栏回调不能重新携带易变的 local index。
#[test]
fn 先选择第二项再点击右栏复制() {
    i_slint_backend_testing::init_integration_test_with_mock_time();
    let window = create_app_window().expect("测试窗口应成功创建");
    let cards = vec![card("第一条"), card("第二条")];
    window.set_cards(ModelRc::new(VecModel::from(cards.clone())));

    let selected = Rc::new(RefCell::new(Vec::new()));
    bind_selection_projection(&window, &cards, Rc::clone(&selected));
    let copies = Rc::new(Cell::new(0_u32));
    let copies_for_callback = Rc::clone(&copies);
    window.on_selected_copy_requested(move || {
        copies_for_callback.set(copies_for_callback.get() + 1);
    });

    window.show().expect("测试窗口应成功显示");
    // 左栏文本行仍为 78px；先点击第二项，再点击右栏底部主按钮。
    click(&window, 100.0, 328.0);
    click(&window, 580.0, 440.0);

    assert_eq!(selected.borrow().as_slice(), &[1]);
    assert_eq!(copies.get(), 1);
}

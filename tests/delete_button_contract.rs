//! 此集成测试验证删除操作已经迁移到右栏，并保持事务 pending 禁用契约。
//!
//! 测试通过真实鼠标事件先选择第二项，再点击右栏删除；删除回调不携带索引，
//! 事务身份和成功前保持可见由 UI reducer 与删除桥负责。

use clipboard_board::{create_app_window, ClipboardCard};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// 在指定逻辑坐标发送一次完整左键点击，覆盖真实 TouchArea 命中区。
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

/// 构造只含摘要的删除卡片；pending 时卡片仍保留，按钮只显示稳定中文状态。
fn card(label: &str, delete_pending: bool) -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from(label),
        source: SharedString::from("测试来源"),
        relative_time: SharedString::from("刚刚"),
        is_pinned: false,
        pin_pending: false,
        delete_pending,
        is_image: false,
        copy_enabled: true,
        image_width: 0,
        image_height: 0,
        thumbnail: Default::default(),
        thumbnail_loaded: false,
        thumbnail_failed: false,
    }
}

/// 选择第二项后同步右栏摘要，模拟生产 reducer 对完整快照选中项的投影。
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

/// 右栏删除命中当前第二项；事务 pending 后重复点击必须被禁用。
#[test]
fn 先选择第二项再点击右栏删除且处理中禁用重复点击() {
    i_slint_backend_testing::init_integration_test_with_mock_time();
    let window = create_app_window().expect("测试窗口应成功创建");
    let cards = vec![card("第一条", false), card("第二条", false)];
    window.set_cards(ModelRc::new(VecModel::from(cards.clone())));

    let selected = Rc::new(RefCell::new(Vec::new()));
    bind_selection_projection(&window, &cards, Rc::clone(&selected));
    let deletes = Rc::new(Cell::new(0_u32));
    let deletes_for_callback = Rc::clone(&deletes);
    window.on_selected_delete_requested(move || {
        deletes_for_callback.set(deletes_for_callback.get() + 1);
    });

    window.show().expect("测试窗口应成功显示");
    click(&window, 100.0, 328.0);
    click(&window, 400.0, 465.0);
    assert_eq!(selected.borrow().as_slice(), &[1]);
    assert_eq!(deletes.get(), 1);

    window.set_selected_card(card("第二条", true));
    click(&window, 400.0, 465.0);
    assert_eq!(deletes.get(), 1);
}

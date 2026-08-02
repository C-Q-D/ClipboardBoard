//! 此集成测试验证清理覆盖层的真实命中、底层事件隔离、生命周期重置和布局独立性。
//!
//! 测试只使用 Slint 测试后端与内存卡片模型，不访问剪贴板、数据库、日志或默认应用目录。

use clipboard_board::app::{bind_app_window, post_ui_event};
use clipboard_board::command::UiEvent;
use clipboard_board::{create_app_window, ClipboardCard};
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{
    ComponentHandle, LogicalPosition, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString,
    VecModel,
};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Once;
use std::time::Duration;

/// 底部状态行的统一清理入口和覆盖层初始菜单取消按钮的稳定坐标。
const CLEANUP_ENTRY: (f32, f32) = (250.0, 475.0);
const MENU_CANCEL: (f32, f32) = (484.0, 297.0);

/// 测试后端是进程级平台；单个测试内的所有场景共享一次初始化结果。
static TEST_BACKEND: Once = Once::new();

/// 安装带 mock time 的 Slint 测试平台，不重复替换全局平台。
fn init_test_backend() {
    TEST_BACKEND.call_once(|| {
        slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
            mock_time: true,
            threading: true,
            renderer_name: Some(SharedString::from("software")),
        })))
        .expect("测试平台只能初始化一次");
    });
}

/// 在指定逻辑坐标发送一次完整左键点击，确保测试覆盖真实窗口命中路径。
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

/// 发送一次真实 Esc 按键；面板级捕获应把它转换为带当前代次的 Hide 事件。
fn send_escape(window: &clipboard_board::AppWindow) {
    let text: SharedString = Key::Escape.into();
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

/// 构造不携带正文的最小文本摘要，用于比较覆盖层开关前后的真实历史布局。
fn card(label: &str) -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from(label),
        source: SharedString::from("覆盖层测试"),
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

/// 在左栏未被居中确认框覆盖的 x=40 位置定位首张卡片表面，比较真实像素首行而非源码文字。
fn first_card_surface_top(snapshot: &SharedPixelBuffer<Rgba8Pixel>) -> usize {
    for y in 150..360 {
        let offset = (y * snapshot.width() as usize + 40) * 4;
        if snapshot.as_bytes()[offset..offset + 4] == [21, 21, 26, 255] {
            return y;
        }
    }
    panic!("测试快照中未找到首张卡片表面");
}

/// 覆盖层必须消费透明背景点击，不能让筛选区收到穿透事件；初始菜单取消只关闭自身。
#[test]
fn 清理覆盖层阻断穿透并保持布局且按生命周期重置() {
    init_test_backend();
    let window = create_app_window().expect("测试窗口应成功创建");

    let filters = Rc::new(Cell::new(0_u32));
    let filters_for_callback = Rc::clone(&filters);
    window.on_search_filter_requested(move |_| {
        filters_for_callback.set(filters_for_callback.get() + 1);
    });

    window.show().expect("测试窗口应成功显示");
    click(&window, CLEANUP_ENTRY.0, CLEANUP_ENTRY.1);
    assert!(window.get_cleanup_panel_open());

    // 覆盖层打开时点击下方筛选区，底层业务回调不得被触发。
    click(&window, 60.0, 112.0);
    assert_eq!(filters.get(), 0);

    // 空白遮罩不承担关闭语义，避免误触导致危险操作流程丢失。
    click(&window, 40.0, 160.0);
    assert!(window.get_cleanup_panel_open());

    click(&window, MENU_CANCEL.0, MENU_CANCEL.1);
    assert!(!window.get_cleanup_panel_open());

    // 本地取消后再次点击底部入口仍可打开新覆盖层，不残留旧确认子状态。
    click(&window, CLEANUP_ENTRY.0, CLEANUP_ENTRY.1);
    assert!(window.get_cleanup_panel_open());

    // 覆盖层是 shell 的最后一个子项，开关不能改变历史视口、内容高度或可见高度。
    window.set_cleanup_panel_open(false);
    window.set_cards(ModelRc::new(VecModel::from(vec![
        card("第一条"),
        card("第二条"),
    ])));
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    let before_snapshot = window.window().take_snapshot().expect("覆盖层前快照失败");
    let before_card_top = first_card_surface_top(&before_snapshot);
    let before = (
        window.get_history_visible_height(),
        window.get_history_legacy_visible_height(),
        window.get_history_viewport_height(),
        window.get_history_viewport_y(),
    );
    window.set_cleanup_panel_open(true);
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    let during_snapshot = window.window().take_snapshot().expect("覆盖层中快照失败");
    assert_eq!(
        first_card_surface_top(&during_snapshot),
        before_card_top,
        "打开覆盖层不得改变首张卡片的真实像素起始行"
    );
    let during = (
        window.get_history_visible_height(),
        window.get_history_legacy_visible_height(),
        window.get_history_viewport_height(),
        window.get_history_viewport_y(),
    );
    assert_eq!(during, before, "打开覆盖层不得改变历史布局和视口");
    window.set_cleanup_panel_open(false);
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    let after_snapshot = window.window().take_snapshot().expect("覆盖层后快照失败");
    assert_eq!(
        first_card_surface_top(&after_snapshot),
        before_card_top,
        "关闭覆盖层不得改变首张卡片的真实像素起始行"
    );
    let after = (
        window.get_history_visible_height(),
        window.get_history_legacy_visible_height(),
        window.get_history_viewport_height(),
        window.get_history_viewport_y(),
    );
    assert_eq!(after, before, "关闭覆盖层也不得改变历史布局和视口");

    // Show、Alt+V 和 Esc 生命周期必须清掉纯展示开关；重开不能残留旧会话状态。
    let lifecycle_window = create_app_window().expect("生命周期测试窗口应成功创建");
    bind_app_window(&lifecycle_window);
    lifecycle_window.set_cleanup_panel_open(true);
    post_ui_event(UiEvent::ShowPanel).expect("Show 事件应成功入队");
    let show_window = lifecycle_window.as_weak();
    slint::invoke_from_event_loop(move || {
        let window = show_window
            .upgrade()
            .expect("Show 生命周期中窗口必须仍然存在");
        assert!(!window.get_cleanup_panel_open());
        window.set_cleanup_panel_open(true);
        // OpenPanel 是 Alt+V 的切换语义：当前可见时隐藏并清掉覆盖层。
        post_ui_event(UiEvent::OpenPanel).expect("Alt+V 隐藏事件应成功入队");
        let alt_hide_window = window.as_weak();
        slint::invoke_from_event_loop(move || {
            let window = alt_hide_window
                .upgrade()
                .expect("Alt+V 隐藏生命周期中窗口必须仍然存在");
            assert!(!window.get_cleanup_panel_open());
            // 同一切换语义重新显示；Show 分支也必须保持遮罩关闭。
            post_ui_event(UiEvent::OpenPanel).expect("Alt+V 重新显示事件应成功入队");
            let alt_show_window = window.as_weak();
            slint::invoke_from_event_loop(move || {
                let window = alt_show_window
                    .upgrade()
                    .expect("Alt+V 重新显示生命周期中窗口必须仍然存在");
                assert!(!window.get_cleanup_panel_open());
                window.set_cleanup_panel_open(true);
                send_escape(&window);

                let esc_hide_window = window.as_weak();
                slint::invoke_from_event_loop(move || {
                    let window = esc_hide_window
                        .upgrade()
                        .expect("Esc 隐藏生命周期中窗口必须仍然存在");
                    assert!(!window.get_cleanup_panel_open());
                    post_ui_event(UiEvent::OpenPanel).expect("Esc 重新显示事件应成功入队");

                    let esc_show_window = window.as_weak();
                    slint::invoke_from_event_loop(move || {
                        let window = esc_show_window
                            .upgrade()
                            .expect("Esc 重新显示生命周期中窗口必须仍然存在");
                        assert!(!window.get_cleanup_panel_open());
                        slint::quit_event_loop().expect("测试事件循环应能正常退出");
                    })
                    .expect("Esc 重新显示断言回调应成功入队");
                })
                .expect("Esc 隐藏断言回调应成功入队");
            })
            .expect("Alt+V 重新显示断言回调应成功入队");
        })
        .expect("Alt+V 隐藏断言回调应成功入队");
    })
    .expect("Show 断言回调应成功入队");
    slint::run_event_loop().expect("生命周期测试事件循环应正常结束");
}

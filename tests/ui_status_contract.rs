//! 此集成测试验证 UIR-15 的固定状态区、历史空态叠加层和右栏 mutation 状态槽。
//!
//! 测试只使用 Slint 软件后端和内存摘要模型；它通过真实窗口点击重试入口，并用真实快照
//! 区分连续卡片表面与文字抗锯齿像素，不访问剪贴板、数据库、文件路径或构建产物。

use clipboard_board::{create_app_window, ClipboardCard};
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, SharedString, VecModel};
use std::cell::Cell;
use std::rc::Rc;

/// 底部状态行在 720×520 看板中的真实左侧重试命中点；清理按钮位于同一固定行右侧。
const RETRY_POINT: (f32, f32) = (100.0, 475.0);

/// 初始化软件后端，使状态文字、连续表面和真实 TouchArea 都能被同一测试观察。
fn init_test_backend() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: true,
        renderer_name: Some(SharedString::from("software")),
    })))
    .expect("测试平台只能初始化一次");
}

/// 在指定逻辑坐标发送一次完整左键点击，确保重试回调来自真实命中区而非直接调用回调。
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

/// 推进软件布局但不进入真实事件循环，保证每次状态断言都对应已经提交的 UI 帧。
fn update_layout() {
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
}

/// 构造只含受限摘要的文本卡片；测试不会把完整正文或稳定身份之外的数据送进 UI。
fn card(index: usize) -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from(format!("状态测试-{index}")),
        source: SharedString::from("状态测试"),
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

/// 统计指定区域内的浅色真实绘制，专门用于判断空态文字是否位于列表视口内。
fn readable_pixels(
    snapshot: &slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    x_start: usize,
    x_end: usize,
    y_start: usize,
    y_end: usize,
) -> usize {
    let bytes = snapshot.as_bytes();
    let stride = snapshot.width() as usize * 4;
    (y_start..y_end)
        .flat_map(|y| (x_start..x_end).map(move |x| (x, y)))
        .filter(|(x, y)| {
            let offset = y * stride + x * 4;
            bytes[offset] > 70
                && bytes[offset + 1] > 70
                && bytes[offset + 2] > 70
                && bytes[offset + 3] == 255
        })
        .count()
}

/// 只统计同色邻域组成的连续卡片表面，避免把空态文字抗锯齿误当成空卡片。
fn solid_surface_pixels(
    snapshot: &slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    x_start: usize,
    x_end: usize,
    y_start: usize,
    y_end: usize,
    expected: [u8; 4],
) -> usize {
    let pixel = |x: usize, y: usize| {
        let offset = (y * snapshot.width() as usize + x) * 4;
        let bytes = snapshot.as_bytes();
        [
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]
    };
    (y_start.saturating_add(1)..y_end.saturating_sub(1))
        .flat_map(|y| (x_start.saturating_add(1)..x_end.saturating_sub(1)).map(move |x| (x, y)))
        .filter(|(x, y)| {
            pixel(*x, *y) == expected
                && (-1_i32..=1).all(|dy| {
                    (-1_i32..=1).all(|dx| {
                        pixel((*x as i32 + dx) as usize, (*y as i32 + dy) as usize) == expected
                    })
                })
        })
        .count()
}

/// 空态、分页、搜索失败和 mutation 状态都只能换内容，不能改变列表几何或绕过真实重试门禁。
#[test]
fn 固定状态区与空态叠加不改变历史几何() {
    init_test_backend();
    let window = create_app_window().expect("状态测试窗口应成功创建");
    window.show().expect("状态测试窗口应成功显示");
    update_layout();

    let retry_requests = Rc::new(Cell::new(0_u32));
    let retry_requests_for_callback = Rc::clone(&retry_requests);
    window.on_retry_history_page_requested(move || {
        retry_requests_for_callback.set(retry_requests_for_callback.get() + 1);
    });

    // 初始空历史没有模型项；文字只是视口叠加层，且不能伪造一张卡片表面。
    assert_eq!(window.get_history_active_logical_count(), 0);
    assert_eq!(window.get_history_model_length(), 0);
    let empty_visible_height = window.get_history_visible_height();
    let empty_viewport_height = window.get_history_viewport_height();
    assert!(empty_visible_height > 0.0);
    let empty_snapshot = window.window().take_snapshot().expect("初始空态快照失败");
    assert!(
        readable_pixels(&empty_snapshot, 28, 292, 220, 350) > 20,
        "初始空态必须在历史视口内真实绘制反馈"
    );
    assert_eq!(
        solid_surface_pixels(&empty_snapshot, 28, 292, 200, 350, [21, 21, 26, 255]),
        0,
        "初始空态不能通过透明模型或伪造卡片提供占位"
    );

    // 工具栏内的启动反馈和分页状态行都固定占位，不能改变列表可见高度。
    window.set_startup_status(SharedString::from(
        "启动失败：这是一段必须在工具栏内单行裁剪的长反馈文本",
    ));
    update_layout();
    assert_eq!(window.get_history_visible_height(), empty_visible_height);
    assert_eq!(window.get_history_viewport_height(), empty_viewport_height);

    window.set_startup_status(SharedString::default());
    window.set_history_next_page_loading(true);
    update_layout();
    assert_eq!(window.get_history_visible_height(), empty_visible_height);
    assert_eq!(window.get_history_viewport_height(), empty_viewport_height);

    // retry=false 时没有重试 TouchArea；retry=true 时同一底部状态行必须真实可点击。
    window.set_history_next_page_loading(false);
    window.set_history_retry_required(false);
    update_layout();
    click(&window, RETRY_POINT.0, RETRY_POINT.1);
    assert_eq!(retry_requests.get(), 0, "retry 未启用时不能提交续页重试");

    window.set_history_retry_required(true);
    update_layout();
    assert_eq!(window.get_history_visible_height(), empty_visible_height);
    click(&window, RETRY_POINT.0, RETRY_POINT.1);
    assert_eq!(
        retry_requests.get(),
        1,
        "retry 启用时必须由真实 TouchArea 提交一次"
    );

    // 搜索空结果仍然没有模型项；搜索错误则保留旧卡片，不能用错误状态清空历史。
    window.set_history_retry_required(false);
    window.set_search_status(SharedString::from("empty"));
    update_layout();
    assert_eq!(window.get_history_active_logical_count(), 0);
    assert_eq!(window.get_history_model_length(), 0);
    assert_eq!(window.get_history_visible_height(), empty_visible_height);

    window.set_cards(ModelRc::new(VecModel::from(vec![card(0)])));
    window.set_search_status(SharedString::from("error"));
    update_layout();
    let error_snapshot = window.window().take_snapshot().expect("搜索失败快照失败");
    assert_eq!(window.get_history_active_logical_count(), 1);
    assert_eq!(window.get_history_model_length(), 1);
    assert!(
        solid_surface_pixels(&error_snapshot, 28, 292, 200, 350, [21, 21, 26, 255]) > 1_000,
        "搜索失败时旧卡片必须保留真实表面"
    );
    let card_visible_height = window.get_history_visible_height();
    let card_viewport_height = window.get_history_viewport_height();

    window.set_search_status(SharedString::from("loading"));
    update_layout();
    assert_eq!(window.get_history_visible_height(), card_visible_height);
    assert_eq!(window.get_history_viewport_height(), card_viewport_height);

    // 搜索恢复后追加新捕获，首项仍由真实模型绘制，不能残留错误叠加层遮住它。
    window.set_search_status(SharedString::from("idle"));
    window.set_cards(ModelRc::new(VecModel::from(vec![card(1), card(0)])));
    update_layout();
    assert_eq!(window.get_history_active_logical_count(), 2);
    assert!(
        solid_surface_pixels(
            &window.window().take_snapshot().expect("新捕获快照失败"),
            28,
            292,
            200,
            350,
            [21, 21, 26, 255]
        ) > 1_000,
        "新捕获首项必须在视口顶部形成连续真实表面"
    );

    // geometry 模式下 bounded window 可以暂时为空，但逻辑数量为 30 时绝不能显示空历史文案。
    window.set_geometry_mode(true);
    window.set_history_logical_count(30);
    window.set_geometry_content_height(30.0 * 78.0);
    window.set_window_cards(ModelRc::new(VecModel::from(Vec::<ClipboardCard>::new())));
    window.set_search_status(SharedString::from("idle"));
    update_layout();
    let geometry_snapshot = window
        .window()
        .take_snapshot()
        .expect("geometry 空窗口快照失败");
    assert_eq!(window.get_history_active_logical_count(), 30);
    assert_eq!(window.get_history_model_length(), 0);
    assert_eq!(
        readable_pixels(&geometry_snapshot, 28, 292, 220, 350),
        0,
        "bounded window 暂空但逻辑历史非空时不能误显示空态文案"
    );

    // 右栏 mutation 状态使用固定槽位；错误和 pending 都不能挤压左侧历史视口。
    window.set_geometry_mode(false);
    window.set_cards(ModelRc::new(VecModel::from(vec![card(0)])));
    window.set_has_selected_card(true);
    window.set_selected_card(card(0));
    window.set_pin_error_visible(false);
    window.set_history_mutation_pending(false);
    update_layout();
    let action_visible_height = window.get_history_visible_height();
    window.set_pin_error_visible(true);
    update_layout();
    assert_eq!(window.get_history_visible_height(), action_visible_height);
    assert!(
        readable_pixels(
            &window
                .window()
                .take_snapshot()
                .expect("mutation 错误快照失败"),
            305,
            692,
            406,
            426
        ) > 0,
        "收藏错误必须在操作栏上方固定槽位真实绘制"
    );
    window.set_pin_error_visible(false);
    window.set_history_mutation_pending(true);
    update_layout();
    assert_eq!(window.get_history_visible_height(), action_visible_height);
    assert!(
        readable_pixels(
            &window
                .window()
                .take_snapshot()
                .expect("mutation pending 快照失败"),
            305,
            692,
            406,
            426
        ) > 0,
        "mutation pending 必须复用固定状态槽位"
    );
}

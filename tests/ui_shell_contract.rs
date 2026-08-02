//! 此集成测试用真实软件渲染验证 UIR-03 的连续外框、UIR-05 的双栏边界和列表首项几何。
//!
//! 测试只采样窗口背景、shell 表面和真实卡片绘制，不读取源码字符串，也不访问剪贴板、
//! 数据库或默认应用目录；卡片只使用受限摘要和安全默认字段。

use clipboard_board::app::set_window_commit;
use clipboard_board::command::{
    UiClipboardItem, UiClipboardItemKind, WindowCommitBuilder, WindowCommitPayload, WindowOffset,
};
use clipboard_board::{create_app_window, ClipboardCard};
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{
    ComponentHandle, Image, LogicalPosition, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString,
    VecModel,
};
use std::cell::RefCell;
use std::rc::Rc;

/// 在指定逻辑坐标发送一次完整左键点击，验证筛选和历史卡片都走真实 TouchArea。
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

/// 按软件快照读取单个 RGBA 像素，避免把颜色判断分散到测试主体。
fn pixel(snapshot: &slint::SharedPixelBuffer<slint::Rgba8Pixel>, x: usize, y: usize) -> [u8; 4] {
    let offset = (y * snapshot.width() as usize + x) * 4;
    let bytes = snapshot.as_bytes();
    [
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]
}

/// 统计指定区域内与稳定卡片表面完全一致的像素，证明真实首项已经绘制。
fn surface_pixels(
    snapshot: &slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    x_start: usize,
    x_end: usize,
    y_start: usize,
    y_end: usize,
    expected: [u8; 4],
) -> usize {
    (y_start..y_end)
        .flat_map(|y| (x_start..x_end).map(move |x| (x, y)))
        .filter(|(x, y)| pixel(snapshot, *x, *y) == expected)
        .count()
}

/// 统计缩略图状态文字和线性图标的高亮像素，避免依赖字体抗锯齿后的单一颜色。
fn light_pixels(
    snapshot: &slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    x_start: usize,
    x_end: usize,
    y_start: usize,
    y_end: usize,
) -> usize {
    (y_start..y_end)
        .flat_map(|y| (x_start..x_end).map(move |x| (x, y)))
        .filter(|(x, y)| {
            let [red, green, blue, alpha] = pixel(snapshot, *x, *y);
            alpha == 255 && red > 80 && green > 80 && blue > 80
        })
        .count()
}

/// 构造单张文本卡片；shell 契约只需要稳定的最小展示数据。
fn card() -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from("首条真实卡片"),
        source: SharedString::from("shell 测试"),
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

/// 构造图片三态夹具；缩略图句柄只在 loaded=true 时进入组件树，失败态不伪造空图片。
fn image_card(thumbnail_loaded: bool, thumbnail_failed: bool, thumbnail: Image) -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from("图片摘要"),
        source: SharedString::from("图片测试来源"),
        relative_time: SharedString::from("刚刚"),
        is_pinned: false,
        pin_pending: false,
        delete_pending: false,
        is_image: true,
        copy_enabled: true,
        image_width: 1920,
        image_height: 1080,
        thumbnail,
        thumbnail_loaded,
        thumbnail_failed,
    }
}

/// 构造不透明缩略图，确保 loaded 状态的 cover 绘制可以由快照直接证明。
fn solid_thumbnail() -> Image {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(4, 4);
    for pixel in buffer.make_mut_bytes().chunks_exact_mut(4) {
        pixel.copy_from_slice(&[84, 132, 196, 255]);
    }
    Image::from_rgba8(buffer)
}

/// 空历史保留窗口外框；四个筛选可命中；填充卡片后首项位于左栏历史槽顶部。
#[test]
fn 连续外框隔离窗口背景且首项没有整卡空白() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: true,
        renderer_name: Some(SharedString::from("software")),
    })))
    .expect("测试平台只能初始化一次");

    let window = create_app_window().expect("测试窗口应成功创建");
    let filters = Rc::new(RefCell::new(Vec::new()));
    let filters_for_callback = Rc::clone(&filters);
    window.on_search_filter_requested(move |filter| {
        filters_for_callback.borrow_mut().push(filter);
    });
    window.show().expect("测试窗口应成功显示");
    let empty_snapshot = window.window().take_snapshot().expect("空历史快照失败");

    assert_eq!(empty_snapshot.width(), 720);
    assert_eq!(empty_snapshot.height(), 520);

    // #09090B 是窗口外背景；#101014 是 shell 内表面，采样点避开边框和文字。
    for (x, y) in [(0, 0), (719, 0), (0, 519), (719, 519)] {
        assert_eq!(pixel(&empty_snapshot, x, y), [9, 9, 11, 255]);
    }
    assert_eq!(pixel(&empty_snapshot, 7, 320), [9, 9, 11, 255]);
    assert_eq!(pixel(&empty_snapshot, 20, 20), [16, 16, 20, 255]);
    assert_eq!(pixel(&empty_snapshot, 700, 500), [16, 16, 20, 255]);

    // 左栏四项筛选位于全局 x=28..292、y=94..130；索引顺序必须原样透传。
    for x in [60.0, 132.0, 204.0, 276.0] {
        click(&window, x, 112.0);
    }
    assert_eq!(filters.borrow().as_slice(), &[0, 1, 2, 3]);
    assert!(!filters.borrow().is_empty(), "筛选契约必须真实执行至少一次");

    // 264px 左栏与右侧占位之间必须有连续 1px 分隔线，不能互相覆盖。
    let divider_pixels = surface_pixels(&empty_snapshot, 292, 293, 94, 488, [42, 41, 49, 255]);
    assert!(
        divider_pixels > 300,
        "左右栏分隔线绘制不足：{divider_pixels}"
    );
    let right_surface_pixels =
        surface_pixels(&empty_snapshot, 340, 692, 110, 488, [21, 21, 26, 255]);
    assert!(
        right_surface_pixels > 10_000,
        "右侧预览占位没有形成独立表面：{right_surface_pixels}"
    );

    let empty_card_pixels = surface_pixels(&empty_snapshot, 28, 292, 200, 350, [21, 21, 26, 255]);
    assert_eq!(
        empty_card_pixels, 0,
        "空历史不能用透明卡片或整卡占位，发现 {empty_card_pixels} 个卡片表面像素"
    );

    window.set_cards(ModelRc::new(VecModel::from(vec![card()])));
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    let filled_snapshot = window.window().take_snapshot().expect("首项快照失败");
    let first_card_pixels = surface_pixels(&filled_snapshot, 28, 292, 200, 350, [21, 21, 26, 255]);
    assert!(
        first_card_pixels > 1_000,
        "首张真实卡片没有在历史区域顶部形成连续表面，仅发现 {first_card_pixels} 个像素"
    );

    // UIR-06：legacy 模型与显式 WindowCommit 使用同一摘要、同一选中态和同一点击命中区。
    window.set_selected_index(0);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    let legacy_selected_snapshot = window
        .window()
        .take_snapshot()
        .expect("legacy 选中态快照失败");
    let legacy_selected_pixels = surface_pixels(
        &legacy_selected_snapshot,
        28,
        292,
        200,
        400,
        [43, 41, 54, 255],
    );

    let item = UiClipboardItem {
        id: 1,
        preview: "首条真实卡片".to_owned(),
        source: "shell 测试".to_owned(),
        relative_time: "刚刚".to_owned(),
        content_hash: [3; 32],
        copy_count: 0,
        is_pinned: false,
        kind: UiClipboardItemKind::Text,
    };
    let mut builder = WindowCommitBuilder::new(12, 1, 1).expect("测试 nonce 必须非零");
    assert!(builder.set_window(WindowCommitPayload {
        start: 0,
        total_count: 1,
        total_height: 106,
        visible_height: 246,
        clamped_viewport_y: 0,
        origin_token: Some(9),
        cards: vec![item],
        offsets: vec![WindowOffset {
            absolute_index: 0,
            id: 1,
            content_hash: [3; 32],
            top: 0,
            height: 106,
        }],
    }));
    assert!(builder.ready());
    let commit = builder.publish_commit_stamp().expect("应发布窗口提交");

    let geometry = create_app_window().expect("geometry 测试窗口应成功创建");
    geometry.set_selected_index(0);
    assert!(set_window_commit(&geometry, commit));
    geometry.show().expect("geometry 测试窗口应成功显示");
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    let geometry_snapshot = geometry
        .window()
        .take_snapshot()
        .expect("geometry 选中态快照失败");
    let geometry_selected_pixels =
        surface_pixels(&geometry_snapshot, 28, 292, 200, 400, [43, 41, 54, 255]);

    assert!(
        legacy_selected_pixels > 1_000,
        "legacy 选中表面像素不足：{legacy_selected_pixels}"
    );
    assert!(
        geometry_selected_pixels > 1_000,
        "geometry 选中表面像素不足：{geometry_selected_pixels}"
    );
    assert_eq!(
        legacy_selected_pixels, geometry_selected_pixels,
        "双路径共享视觉的选中表面像素必须一致"
    );
    assert_eq!(geometry.get_history_model_length(), 1);
    assert!(geometry.get_history_visible_height() > 0.0);
    assert_eq!(
        geometry.get_history_legacy_visible_height(),
        0.0,
        "geometry 模式隐藏的 legacy ListView 不能占用高度"
    );

    let legacy_selected = Rc::new(RefCell::new(Vec::new()));
    let legacy_selected_for_callback = Rc::clone(&legacy_selected);
    window.on_card_selection_requested(move |index| {
        legacy_selected_for_callback.borrow_mut().push(index);
    });
    let geometry_selected = Rc::new(RefCell::new(Vec::new()));
    let geometry_selected_for_callback = Rc::clone(&geometry_selected);
    geometry.on_card_selection_requested(move |index| {
        geometry_selected_for_callback.borrow_mut().push(index);
    });
    // 290px 位于两条路径首行共同的内容中心，避开路径外层的 12px sibling 间距。
    click(&window, 100.0, 290.0);
    click(&geometry, 100.0, 290.0);
    assert_eq!(legacy_selected.borrow().as_slice(), &[0]);
    assert_eq!(geometry_selected.borrow().as_slice(), &[0]);

    // UIR-08：三种图片缩略图状态都必须在固定 92px 行内留下真实可见证据。
    window.set_selected_index(-1);
    window.set_cards(ModelRc::new(VecModel::from(vec![image_card(
        false,
        false,
        Image::default(),
    )])));
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    assert_eq!(window.get_history_viewport_height(), 92.0);
    let loading_snapshot = window.window().take_snapshot().expect("图片加载中快照失败");

    window.set_cards(ModelRc::new(VecModel::from(vec![image_card(
        false,
        true,
        Image::default(),
    )])));
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    assert_eq!(window.get_history_viewport_height(), 92.0);
    let failed_snapshot = window.window().take_snapshot().expect("图片失败快照失败");

    window.set_cards(ModelRc::new(VecModel::from(vec![image_card(
        true,
        false,
        solid_thumbnail(),
    )])));
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    assert_eq!(window.get_history_viewport_height(), 92.0);
    let loaded_snapshot = window
        .window()
        .take_snapshot()
        .expect("图片加载成功快照失败");

    assert!(
        light_pixels(&loading_snapshot, 42, 94, 260, 326) > 20,
        "加载中占位没有真实绘制"
    );
    assert!(
        light_pixels(&failed_snapshot, 42, 94, 260, 326) > 20,
        "失败态文案或图标没有真实绘制"
    );
    assert!(
        surface_pixels(&loaded_snapshot, 42, 94, 255, 330, [84, 132, 196, 255]) > 1_000,
        "已加载缩略图没有在 52×52 盒中真实绘制"
    );
}

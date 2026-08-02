//! 此集成测试用真实软件渲染验证 UIR-03 的连续外框和列表首项几何。
//!
//! 测试只采样窗口背景、shell 表面和真实卡片绘制，不读取源码字符串，也不访问剪贴板、
//! 数据库或默认应用目录；卡片只使用受限摘要和安全默认字段。

use clipboard_board::{create_app_window, ClipboardCard};
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

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

/// 空历史保留窗口外框；填充一张卡片后首项直接出现在同一历史槽顶部。
#[test]
fn 连续外框隔离窗口背景且首项没有整卡空白() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: true,
        renderer_name: Some(SharedString::from("software")),
    })))
    .expect("测试平台只能初始化一次");

    let window = create_app_window().expect("测试窗口应成功创建");
    window.show().expect("测试窗口应成功显示");
    let empty_snapshot = window.window().take_snapshot().expect("空历史快照失败");

    // #09090B 是窗口外背景；#101014 是 shell 内表面，采样点避开边框和文字。
    for (x, y) in [(0, 0), (559, 0), (0, 639), (559, 639)] {
        assert_eq!(pixel(&empty_snapshot, x, y), [9, 9, 11, 255]);
    }
    assert_eq!(pixel(&empty_snapshot, 7, 320), [9, 9, 11, 255]);
    assert_eq!(pixel(&empty_snapshot, 20, 20), [16, 16, 20, 255]);
    assert_eq!(pixel(&empty_snapshot, 520, 620), [16, 16, 20, 255]);

    let empty_card_pixels = surface_pixels(&empty_snapshot, 28, 532, 180, 330, [21, 21, 26, 255]);
    assert_eq!(
        empty_card_pixels, 0,
        "空历史不能用透明卡片或整卡占位，发现 {empty_card_pixels} 个卡片表面像素"
    );

    window.set_cards(ModelRc::new(VecModel::from(vec![card()])));
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    let filled_snapshot = window.window().take_snapshot().expect("首项快照失败");
    let first_card_pixels = surface_pixels(&filled_snapshot, 28, 532, 180, 330, [21, 21, 26, 255]);
    assert!(
        first_card_pixels > 1_000,
        "首张真实卡片没有在历史区域顶部形成连续表面，仅发现 {first_card_pixels} 个像素"
    );
}

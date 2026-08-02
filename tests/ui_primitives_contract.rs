//! 此集成测试用真实软件渲染验证 UIR-02 的图标和按钮基础组件。
//!
//! 测试不读取源码字符串来伪造证据，而是实例化由 Slint 编译出的测试画廊，
//! 检查六种图标确实留下绘制像素、按钮拥有 36px 命中区，并验证 disabled 的
//! TouchArea 不会调用回调。

use clipboard_board::PrimitiveGallery;
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, Rgba8Pixel, SharedPixelBuffer, SharedString};

/// 在测试窗口中发送一次完整左键点击。
fn click(window: &PrimitiveGallery, x: f32, y: f32) {
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

/// 统计区域内明显区别于深色画廊背景的像素，证明图标或按钮确实被软件渲染。
fn drawn_pixels(
    snapshot: &SharedPixelBuffer<Rgba8Pixel>,
    x_start: usize,
    x_end: usize,
    y_start: usize,
    y_end: usize,
) -> usize {
    let pixels = snapshot.as_bytes();
    let stride = snapshot.width() as usize * 4;
    (y_start..y_end)
        .flat_map(|y| (x_start..x_end).map(move |x| (x, y)))
        .filter(|(x, y)| {
            let offset = y * stride + x * 4;
            pixels[offset] > 40 || pixels[offset + 1] > 40 || pixels[offset + 2] > 40
        })
        .count()
}

/// 六种图标和两种按钮都可绘制，按钮命中区统一且 disabled 不触发回调。
#[test]
fn 图标边界和按钮状态可渲染且禁用不回调() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: true,
        renderer_name: Some(SharedString::from("software")),
    })))
    .expect("测试平台只能初始化一次");

    let window = PrimitiveGallery::new().expect("基础组件测试画廊应成功创建");
    let enabled_clicks = std::rc::Rc::new(std::cell::Cell::new(0_u32));
    let enabled_clicks_for_callback = std::rc::Rc::clone(&enabled_clicks);
    window.on_enabled_button_clicked(move || {
        enabled_clicks_for_callback.set(enabled_clicks_for_callback.get() + 1);
    });
    let disabled_clicks = std::rc::Rc::new(std::cell::Cell::new(0_u32));
    let disabled_clicks_for_callback = std::rc::Rc::clone(&disabled_clicks);
    window.on_disabled_button_clicked(move || {
        disabled_clicks_for_callback.set(disabled_clicks_for_callback.get() + 1);
    });

    assert_eq!(window.get_enabled_icon_hit_width(), 36.0);
    assert_eq!(window.get_enabled_icon_hit_height(), 36.0);
    window.show().expect("基础组件测试画廊应成功显示");
    let snapshot = window.window().take_snapshot().expect("软件渲染快照失败");
    assert_eq!(snapshot.width(), 344);
    assert_eq!(snapshot.height(), 144);

    // 六个 16×16 视觉盒分别对应文本、图片、收藏空心/实心、复制和删除图标。
    for x_start in [8, 32, 56, 80, 104, 128] {
        let pixels = drawn_pixels(&snapshot, x_start, x_start + 16, 8, 24);
        assert!(pixels > 0, "图标区域 {x_start} 没有软件渲染像素");
    }

    // 普通、primary、danger 和 disabled 文本按钮都必须有可见绘制边界。
    for (x_start, x_end) in [(8, 80), (88, 160), (168, 240), (248, 336)] {
        let pixels = drawn_pixels(&snapshot, x_start, x_end, 96, 132);
        assert!(pixels > 0, "按钮区域 {x_start}..{x_end} 没有软件渲染像素");
    }

    // 第一枚 IconButton 可点击，第二枚明确 disabled；两者命中区均为 36×36。
    click(&window, 26.0, 66.0);
    click(&window, 70.0, 66.0);
    assert_eq!(
        enabled_clicks.get(),
        1,
        "enabled 回调次数为 {}，disabled 回调次数为 {}",
        enabled_clicks.get(),
        disabled_clicks.get()
    );
    assert_eq!(
        disabled_clicks.get(),
        0,
        "disabled 回调次数为 {}，enabled 回调次数为 {}",
        disabled_clicks.get(),
        enabled_clicks.get()
    );
}

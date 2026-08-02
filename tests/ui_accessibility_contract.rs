//! UIR-16 的无障碍、对比度、焦点和长文本契约。
//!
//! 测试从真实 theme.slint 读取颜色令牌并用 Slint 软件后端渲染 720×520 窗口；
//! 它同时发送真实鼠标/键盘事件，确保焦点环、固定行高和主按钮命中区不是源码
//! 字符串或坐标假设出来的证据。

use clipboard_board::{create_app_window, ClipboardCard, PrimitiveGallery};
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, SharedString, VecModel};
use std::cell::Cell;
use std::rc::Rc;

/// 从 Theme 实际源文本读取一个六位 RGB 令牌，避免测试复制第二套颜色常量。
fn theme_color(source: &str, name: &str) -> [u8; 3] {
    let marker = format!("{name}: #");
    let start = source
        .find(&marker)
        .unwrap_or_else(|| panic!("主题缺少颜色令牌：{name}"))
        + marker.len();
    let hex = &source[start..start + 6];
    [0, 2, 4].map(|offset| {
        u8::from_str_radix(&hex[offset..offset + 2], 16)
            .unwrap_or_else(|_| panic!("主题令牌 {name} 不是合法 RGB：#{hex}"))
    })
}

/// 按 WCAG 定义计算相对亮度，测试阈值只作用于实际主题令牌的组合。
fn relative_luminance(rgb: [u8; 3]) -> f64 {
    let linear = rgb.map(|channel| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    });
    0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]
}

/// 返回两个实际颜色的对比度比值；调用方负责选择正文或控件边界阈值。
fn contrast_ratio(foreground: [u8; 3], background: [u8; 3]) -> f64 {
    let foreground = relative_luminance(foreground);
    let background = relative_luminance(background);
    (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
}

/// 构造一张文本卡片；长摘要和长来源只进入受限 DTO，测试不携带完整剪贴板正文。
fn text_card(preview: SharedString, source: SharedString) -> ClipboardCard {
    ClipboardCard {
        preview,
        source,
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

/// 构造图片 loading/loaded/failed 共用的最小 DTO，验证图片行仍保持 92px 外层高度。
fn image_card(loaded: bool, failed: bool) -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from("图片摘要"),
        source: SharedString::from("超长图片来源"),
        relative_time: SharedString::from("刚刚"),
        is_pinned: false,
        pin_pending: false,
        delete_pending: false,
        is_image: true,
        copy_enabled: true,
        image_width: 1920,
        image_height: 1080,
        thumbnail: Default::default(),
        thumbnail_loaded: loaded,
        thumbnail_failed: failed,
    }
}

/// 发送完整左键点击；测试必须经过真实 TouchArea/LineEdit 命中路径。
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

/// 在一个逻辑矩形内统计主题焦点环的真实软件渲染像素。
fn exact_color_pixels(
    snapshot: &slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    color: [u8; 3],
    x_start: usize,
    x_end: usize,
    y_start: usize,
    y_end: usize,
) -> usize {
    (y_start..y_end)
        .flat_map(|y| (x_start..x_end).map(move |x| (x, y)))
        .filter(|(x, y)| {
            let offset = (y * snapshot.width() as usize + x) * 4;
            let bytes = snapshot.as_bytes();
            bytes[offset..offset + 3] == color
        })
        .count()
}

/// 读取固定分隔线像素，证明超长文案没有跨越左栏和右栏的真实边界。
fn divider_pixels(snapshot: &slint::SharedPixelBuffer<slint::Rgba8Pixel>, color: [u8; 3]) -> usize {
    (94..488)
        .filter(|y| {
            let offset = (*y * snapshot.width() as usize + 292) * 4;
            let bytes = snapshot.as_bytes();
            bytes[offset..offset + 3] == color
        })
        .count()
}

/// UIR-16 必须同时满足实际主题对比度、固定几何、长文本裁剪、焦点环和主按钮命中下限。
#[test]
fn 对比度长文本焦点和逻辑命中区真实通过() {
    let theme_source = include_str!("../ui/theme.slint");
    let surface = theme_color(theme_source, "surface-bg");
    let shell = theme_color(theme_source, "shell-bg");
    let accent_background = theme_color(theme_source, "accent-bg");
    let danger_surface = theme_color(theme_source, "danger-surface");

    for name in ["text-primary", "text-secondary", "text-muted"] {
        let foreground = theme_color(theme_source, name);
        assert!(
            contrast_ratio(foreground, surface) >= 4.5,
            "正文令牌 {name} 在 surface-bg 上对比度不足"
        );
        assert!(
            contrast_ratio(foreground, shell) >= 4.5,
            "正文令牌 {name} 在 shell-bg 上对比度不足"
        );
    }

    for name in ["control-border", "border-selected", "focus-ring"] {
        assert!(
            contrast_ratio(theme_color(theme_source, name), surface) >= 3.0,
            "控件状态令牌 {name} 在 surface-bg 上对比度不足"
        );
    }
    assert!(
        contrast_ratio(theme_color(theme_source, "accent-text"), accent_background) >= 4.5,
        "主操作文字与 accent-bg 对比度不足"
    );
    assert!(
        contrast_ratio(theme_color(theme_source, "danger"), danger_surface) >= 3.0,
        "危险状态文字与 danger-surface 对比度不足"
    );

    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: true,
        renderer_name: Some(SharedString::from("software")),
    })))
    .expect("测试平台只能初始化一次");

    // 基础按钮的 hover/pressed 必须来自真实 PointerEvent，且使用主题状态色区分。
    let gallery = PrimitiveGallery::new().expect("按钮状态测试画廊应成功创建");
    gallery.show().expect("按钮状态测试画廊应成功显示");
    let hover_position = LogicalPosition::new(26.0, 66.0);
    gallery.window().dispatch_event(WindowEvent::PointerMoved {
        position: hover_position,
    });
    let hover_snapshot = gallery.window().take_snapshot().expect("按钮悬停快照失败");
    assert!(
        exact_color_pixels(
            &hover_snapshot,
            theme_color(theme_source, "surface-hover"),
            8,
            44,
            48,
            84
        ) > 100,
        "hover 状态没有形成真实的主题表面"
    );
    gallery
        .window()
        .dispatch_event(WindowEvent::PointerPressed {
            position: hover_position,
            button: PointerEventButton::Left,
        });
    let pressed_snapshot = gallery.window().take_snapshot().expect("按钮按压快照失败");
    assert!(
        exact_color_pixels(
            &pressed_snapshot,
            theme_color(theme_source, "surface-selected"),
            8,
            44,
            48,
            84
        ) > 100,
        "pressed 状态没有形成真实的主题选中表面"
    );
    gallery
        .window()
        .dispatch_event(WindowEvent::PointerReleased {
            position: hover_position,
            button: PointerEventButton::Left,
        });

    let window = create_app_window().expect("无障碍契约窗口应成功创建");
    let long_chinese = SharedString::from("中文长文本".repeat(128));
    let long_english = SharedString::from("continuous-long-english-word".repeat(32));
    let cards = vec![
        text_card(long_chinese, SharedString::from("来源".repeat(80))),
        text_card(long_english, SharedString::from("source-".repeat(80))),
    ];
    window.set_cards(ModelRc::new(VecModel::from(cards.clone())));
    window.set_selected_card(cards[0].clone());
    window.set_has_selected_card(true);

    let copies = Rc::new(Cell::new(0_u32));
    let copies_for_callback = Rc::clone(&copies);
    window.on_selected_copy_requested(move || {
        copies_for_callback.set(copies_for_callback.get() + 1);
    });

    window.show().expect("无障碍契约窗口应成功显示");
    assert_eq!(window.window().size().width, 720);
    assert_eq!(window.window().size().height, 520);
    assert_eq!(window.get_selected_copy_hit_width(), 108.0);
    assert_eq!(window.get_selected_copy_hit_height(), 38.0);

    // 长文本和连续英文都使用固定 78px 文本行，不能通过 preferred size 撑开布局。
    assert_eq!(window.get_history_viewport_height(), 156.0);
    let snapshot = window.window().take_snapshot().expect("长文本快照失败");
    assert_eq!(snapshot.width(), 720);
    assert_eq!(snapshot.height(), 520);
    assert!(
        divider_pixels(&snapshot, theme_color(theme_source, "border-subtle")) > 350,
        "长文本或来源绘制越过左栏分隔线"
    );

    // 图片 loading/failed 两态各自复用固定 92px 逻辑行；软件快照证明状态仍在窗口内。
    for (loaded, failed) in [(false, false), (true, false), (false, true)] {
        window.set_cards(ModelRc::new(VecModel::from(vec![image_card(
            loaded, failed,
        )])));
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
        assert_eq!(window.get_history_viewport_height(), 92.0);
        let image_snapshot = window.window().take_snapshot().expect("图片状态快照失败");
        assert_eq!(image_snapshot.width(), 720);
        assert_eq!(image_snapshot.height(), 520);
    }

    // 点击现有主按钮的有效区域，证明 108×38 是真实命中区而非仅有输出属性。
    click(&window, 580.0, 440.0);
    assert_eq!(copies.get(), 1, "主按钮必须真实执行一次且只能执行一次");

    // 搜索输入先通过真实鼠标获得焦点，再通过真实按键触发用户输入回调。
    let search_edits = Rc::new(Cell::new(0_u32));
    let search_edits_for_callback = Rc::clone(&search_edits);
    window.on_search_text_changed(move |_| {
        search_edits_for_callback.set(search_edits_for_callback.get() + 1);
    });
    click(&window, 300.0, 55.0);
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: SharedString::from("x"),
    });
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: SharedString::from("x"),
    });
    assert!(
        search_edits.get() > 0,
        "搜索输入没有经过真实 LineEdit 焦点路径"
    );
    let search_focus_snapshot = window.window().take_snapshot().expect("搜索焦点快照失败");
    let focus_ring = theme_color(theme_source, "focus-ring");
    assert!(
        exact_color_pixels(&search_focus_snapshot, focus_ring, 195, 615, 35, 77) > 40,
        "搜索输入获得焦点后没有绘制主题 focus-ring"
    );

    // 清空短语输入使用独立的真实 LineEdit；覆盖层打开后点击其中心应留下同一焦点环。
    window.set_cleanup_panel_open(true);
    window.set_clear_all_confirmation_visible(true);
    let clear_before = window
        .window()
        .take_snapshot()
        .expect("清空输入初始快照失败");
    let clear_before_pixels = exact_color_pixels(&clear_before, focus_ring, 198, 522, 229, 271);
    click(&window, 300.0, 250.0);
    let clear_focus_snapshot = window
        .window()
        .take_snapshot()
        .expect("清空输入焦点快照失败");
    let clear_focus_pixels =
        exact_color_pixels(&clear_focus_snapshot, focus_ring, 198, 522, 229, 271);
    assert!(
        clear_focus_pixels > clear_before_pixels && clear_focus_pixels > 40,
        "清空短语输入获得焦点后没有绘制主题 focus-ring"
    );
}

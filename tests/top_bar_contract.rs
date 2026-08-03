//! 此集成测试验证 UIR-04 顶部工具栏的真实绘制、数量语义和搜索框程序回写边界，
//! 并同步锁定 UIR-05 的 720×520 工具栏坐标。
//!
//! 测试只使用 Slint 软件后端和内存模型，不访问剪贴板、SQLite、窗口定位或真实快捷键；
//! geometry 模式的 20,000 条逻辑历史只通过受限属性进入界面，实际窗口模型保持有界。

use clipboard_board::{create_app_window, ClipboardCard};
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::Cell;
use std::rc::Rc;

/// 构造顶部数量契约所需的最小文本卡片，不携带正文或原图。
fn card(index: usize) -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from(format!("工具栏测试-{index}")),
        source: SharedString::from("top-bar"),
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

/// 统计软件快照中指定区域内的非 shell 像素，证明视觉元素确实绘制而非只声明尺寸。
fn non_shell_pixels(
    snapshot: &slint::SharedPixelBuffer<slint::Rgba8Pixel>,
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
            let pixel = [pixels[offset], pixels[offset + 1], pixels[offset + 2]];
            pixel != [16, 16, 20] && pixel != [9, 9, 11]
        })
        .count()
}

/// 顶部工具栏固定、数量来自活动逻辑路径、程序回写不伪造用户编辑事件。
#[test]
fn 顶部工具栏显示真实品牌搜索和快捷键提示() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: true,
        renderer_name: Some(SharedString::from("software")),
    })))
    .expect("测试平台只能初始化一次");

    let window = create_app_window().expect("测试窗口应成功创建");
    window.set_geometry_mode(true);
    window.set_history_logical_count(20_000);
    window.set_window_cards(ModelRc::new(VecModel::from(vec![card(0)])));

    assert_eq!(window.get_history_active_logical_count(), 20_000);
    assert_eq!(window.get_history_model_length(), 1);
    assert!((1..=100).contains(&window.get_history_model_length()));
    assert!((1..=100).contains(&window.get_window_cards().row_count()));

    let programmatic_edits = Rc::new(Cell::new(0_u32));
    let programmatic_edits_for_callback = Rc::clone(&programmatic_edits);
    window.on_search_text_changed(move |_| {
        programmatic_edits_for_callback.set(programmatic_edits_for_callback.get() + 1);
    });
    window.set_search_text("程序同步".into());
    assert_eq!(programmatic_edits.get(), 0, "程序回写不能伪造 edited 事件");

    window.show().expect("顶部工具栏测试窗口应成功显示");
    let snapshot = window.window().take_snapshot().expect("顶部工具栏快照失败");
    assert_eq!(snapshot.width(), 720);
    assert_eq!(snapshot.height(), 520);

    // 唯一客户区内容内缩为 28px；工具栏内容保持原有全局坐标 y=28..82。
    assert!(
        non_shell_pixels(&snapshot, 28, 188, 28, 82) > 200,
        "品牌图标和 ClipboardBoard 文案没有形成真实绘制"
    );
    assert!(
        non_shell_pixels(&snapshot, 196, 608, 36, 74) > 200,
        "搜索框没有形成真实绘制区域"
    );
    assert!(
        non_shell_pixels(&snapshot, 620, 680, 43, 67) > 80,
        "Alt + V 静态胶囊没有形成真实绘制"
    );

    // 工具栏底部 1px 分隔线位于 shell 内全局 y=81；采样宽度证明它不是普通文本下划线。
    let separator_pixels = non_shell_pixels(&snapshot, 28, 692, 81, 82);
    assert!(
        separator_pixels > 300,
        "工具栏底部分隔线绘制不足：{separator_pixels}"
    );

    // 切回 legacy 后总数和实际模型都必须来自完整 cards，而不是残留 geometry 状态。
    window.set_geometry_mode(false);
    window.set_cards(ModelRc::new(VecModel::from(vec![
        card(0),
        card(1),
        card(2),
    ])));
    assert_eq!(window.get_history_active_logical_count(), 3);
    assert_eq!(window.get_history_model_length(), 3);
}

//! 该契约只验证真实 UI 事件队列中的历史滚动释放行为，不访问剪贴板、数据库或用户文件。

use clipboard_board::app::{bind_app_window, post_ui_event};
use clipboard_board::command::{UiClipboardItem, UiClipboardItemKind, UiEvent, UiSnapshot};
use clipboard_board::create_app_window;
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, Rgba8Pixel, SharedPixelBuffer, SharedString};

/// 初始化带软件渲染器的测试后端；回归必须运行真实 Slint invoke 队列。
fn init_test_backend() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: true,
        renderer_name: Some(SharedString::from("software")),
    })))
    .expect("测试平台只能初始化一次");
}

/// 构造首项已选中的显式几何历史；只有首项选中时，旧的错误对齐才会把视口拉回顶部。
fn item(index: usize) -> UiClipboardItem {
    UiClipboardItem {
        id: index as u64 + 1,
        preview: format!("真实滚动-{index}"),
        source: "滚动事件测试".to_owned(),
        relative_time: "刚刚".to_owned(),
        content_hash: [index as u8; 32],
        copy_count: 0,
        is_pinned: false,
        kind: UiClipboardItemKind::Text,
    }
}

/// 读取软件快照中的单个 RGBA 像素，用来定位实际绘制的主题 thumb。
fn pixel(snapshot: &SharedPixelBuffer<Rgba8Pixel>, x: usize, y: usize) -> [u8; 4] {
    let offset = (y * snapshot.width() as usize + x) * 4;
    let bytes = snapshot.as_bytes();
    [
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]
}

/// 从唯一主题源读取滚动条 thumb 颜色，避免测试复制第二套视觉令牌。
fn theme_color(name: &str) -> [u8; 3] {
    let source = include_str!("../ui/theme.slint");
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

/// 发送一次真实 thumb 拖动；释放后由队列中的 HistoryWindowViewportChanged 驱动 reducer。
fn drag(window: &clipboard_board::AppWindow, start: LogicalPosition, end: LogicalPosition) {
    window
        .window()
        .dispatch_event(WindowEvent::PointerMoved { position: start });
    window.window().dispatch_event(WindowEvent::PointerPressed {
        position: start,
        button: PointerEventButton::Left,
    });
    window
        .window()
        .dispatch_event(WindowEvent::PointerMoved { position: end });
    window
        .window()
        .dispatch_event(WindowEvent::PointerReleased {
            position: end,
            button: PointerEventButton::Left,
        });
}

/// 真实绑定路径中，首项选中也不能让滚动条释放后的负向视口回到顶部。
#[test]
fn 真实视口事件释放后保持滚动位置() {
    init_test_backend();
    let window = create_app_window().expect("滚动释放回归窗口应成功创建");
    bind_app_window(&window);
    post_ui_event(UiEvent::ReplaceSnapshot(UiSnapshot {
        items: (0..8).map(item).collect(),
        selected_index: Some(0),
    }))
    .expect("初始历史快照应能进入 UI 事件队列");

    let weak_window = window.as_weak();
    slint::invoke_from_event_loop(move || {
        let window = weak_window
            .upgrade()
            .expect("初始快照回调执行时窗口必须仍然存在");
        window
            .show()
            .expect("滚动释放回归窗口应成功显示");
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
        let snapshot = window
            .window()
            .take_snapshot()
            .expect("滚动释放回归快照失败");
        let thumb = theme_color("scrollbar-thumb");
        let thumb_start = (200_usize..488)
            .flat_map(|y| (276_usize..292).map(move |x| (x, y)))
            .find(|(x, y)| {
                let [red, green, blue, _] = pixel(&snapshot, *x, *y);
                [red, green, blue] == thumb
            })
            .expect("真实几何历史滚动条 thumb 应有绘制像素");

        drag(
            &window,
            LogicalPosition::new(
                thumb_start.0 as f32,
                thumb_start.1 as f32 + 10.0,
            ),
            LogicalPosition::new(
                thumb_start.0 as f32,
                thumb_start.1 as f32 + 50.0,
            ),
        );
        let immediate_viewport = window.get_history_viewport_y();
        assert!(
            immediate_viewport < 0.0,
            "thumb 释放后应先保留负向 viewport，实际值：{immediate_viewport}"
        );

        // 该回调排在拖动产生的真实 HistoryWindowViewportChanged 之后，模拟用户松手后的迟到帧。
        let weak_window_after_release = window.as_weak();
        slint::invoke_from_event_loop(move || {
            let window = weak_window_after_release
                .upgrade()
                .expect("释放后的回调执行时窗口必须仍然存在");
            let viewport_after_release = window.get_history_viewport_y();
            assert!(
                viewport_after_release < 0.0,
                "迟到视口回调不能把释放后的滚动位置拉回顶部，实际值：{viewport_after_release}"
            );
            slint::quit_event_loop().expect("滚动释放回归事件循环应能退出");
        })
        .expect("释放后断言回调应能进入 UI 事件队列");
    })
    .expect("初始快照确认回调应能进入 UI 事件队列");

    slint::run_event_loop().expect("滚动释放回归事件循环应正常结束");
}

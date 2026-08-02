//! 此集成测试验证右栏选中投影来自完整快照，而不是 bounded WindowCommit 的局部模型。
//!
//! 测试只使用摘要 DTO 和 Slint 测试后端，不访问 SQLite、剪贴板、原图路径或用户文件。

use clipboard_board::app::{bind_app_window, post_ui_event, set_window_commit};
use clipboard_board::command::{
    UiClipboardItem, UiClipboardItemKind, UiEvent, UiSnapshot, WindowCommitBuilder,
    WindowCommitPayload, WindowOffset,
};
use clipboard_board::{create_app_window, ClipboardCard};
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, SharedString};

/// 初始化带软件渲染器的测试后端；同一测试同时覆盖事件循环和真实右栏像素。
fn init_test_backend() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: true,
        renderer_name: Some(SharedString::from("software")),
    })))
    .expect("测试平台只能初始化一次");
}

/// 构造只携带安全摘要的文本条目；其索引同时编码进稳定 ID、哈希和预览以便追踪来源。
fn item(index: usize) -> UiClipboardItem {
    UiClipboardItem {
        id: index as u64 + 1,
        preview: format!("完整快照-{index}"),
        source: format!("来源-{index}"),
        relative_time: "刚刚".to_owned(),
        content_hash: [index as u8; 32],
        copy_count: 1,
        is_pinned: false,
        kind: UiClipboardItemKind::Text,
    }
}

/// 构造只包含首条卡片的窗口提交，故意让它与完整快照中的选中项不在同一局部索引。
fn bounded_first_card_commit() -> clipboard_board::command::WindowCommit {
    let first = item(0);
    let mut builder = WindowCommitBuilder::new(41, 1, 1).expect("测试窗口身份必须非零");
    assert!(builder.set_window(WindowCommitPayload {
        start: 0,
        total_count: 85,
        total_height: 85 * 78,
        visible_height: 500,
        clamped_viewport_y: 0,
        origin_token: None,
        cards: vec![first],
        offsets: vec![WindowOffset {
            absolute_index: 0,
            id: 1,
            content_hash: [0; 32],
            top: 0,
            height: 78,
        }],
    }));
    assert!(builder.ready());
    builder
        .publish_commit_stamp()
        .expect("测试窗口提交应能完成发布")
}

/// 构造右栏图片三态使用的安全卡片；图片尺寸固定为摘要元数据，不参与动态布局。
fn image_preview_card(thumbnail: Image, loaded: bool, failed: bool) -> ClipboardCard {
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
        thumbnail_loaded: loaded,
        thumbnail_failed: failed,
    }
}

/// 构造不透明缩略图，loaded 状态应在右栏 contain 预览表面留下稳定颜色。
fn solid_thumbnail() -> Image {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(4, 4);
    for pixel in buffer.make_mut_bytes().chunks_exact_mut(4) {
        pixel.copy_from_slice(&[84, 132, 196, 255]);
    }
    Image::from_rgba8(buffer)
}

/// 统计右栏图片状态区域的可见像素，证明加载和失败占位并非只更新了属性。
fn light_pixels(
    snapshot: &SharedPixelBuffer<slint::Rgba8Pixel>,
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
            bytes[offset] > 70 && bytes[offset + 1] > 70 && bytes[offset + 2] > 70
        })
        .count()
}

/// 统计与 loaded 缩略图颜色一致的像素，避免把状态文案误判为成功预览。
fn thumbnail_pixels(
    snapshot: &SharedPixelBuffer<slint::Rgba8Pixel>,
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
            bytes[offset] == 84 && bytes[offset + 1] == 132 && bytes[offset + 2] == 196
        })
        .count()
}

/// 选中项位于窗口提交之外时仍显示完整快照投影，清空快照后必须 fail-closed。
#[test]
fn 选中投影不受窗口局部模型限制且能安全失效() {
    init_test_backend();
    let window = create_app_window().expect("测试窗口应成功创建");
    bind_app_window(&window);
    let weak_window = window.as_weak();

    let snapshot_items = (0..85).map(item).collect::<Vec<_>>();
    post_ui_event(UiEvent::ReplaceSnapshot(UiSnapshot {
        items: snapshot_items,
        selected_index: Some(84),
    }))
    .expect("完整快照事件应能进入 UI 队列");

    let weak_window_for_ack = weak_window.clone();
    slint::invoke_from_event_loop(move || {
        let window = weak_window_for_ack
            .upgrade()
            .expect("确认回调执行时窗口应仍然存在");
        assert_eq!(window.get_selected_index(), 84);
        assert!(window.get_has_selected_card());
        let selected_card = window.get_selected_card();
        assert_eq!(selected_card.preview.to_string(), "完整快照-84");
        assert_eq!(selected_card.source.to_string(), "来源-84");
        assert!(selected_card.copy_enabled);

        // WindowCommit 只发布局部首条卡片，不能把右栏投影误解析为 local index 0。
        assert!(set_window_commit(&window, bounded_first_card_commit()));
        assert_eq!(window.get_window_start(), 0);
        assert_eq!(window.get_window_length(), 1);
        assert!(window.get_has_selected_card());
        assert_eq!(
            window.get_selected_card().preview.to_string(),
            "完整快照-84"
        );

        // 用新的搜索结果替换完整快照，旧的第 84 条身份不得继续残留在投影中。
        let weak_window_for_replace = window.as_weak();
        post_ui_event(UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![item(7)],
            selected_index: Some(0),
        }))
        .expect("替换快照事件应能进入 UI 队列");
        slint::invoke_from_event_loop(move || {
            let window = weak_window_for_replace
                .upgrade()
                .expect("替换结果确认回调执行时窗口应仍然存在");
            assert_eq!(window.get_selected_index(), 0);
            assert!(window.get_has_selected_card());
            assert_eq!(window.get_selected_card().preview.to_string(), "完整快照-7");

            let weak_window_for_clear = window.as_weak();
            post_ui_event(UiEvent::ReplaceSnapshot(UiSnapshot::default()))
                .expect("清空快照事件应能进入 UI 队列");
            slint::invoke_from_event_loop(move || {
                let window = weak_window_for_clear
                    .upgrade()
                    .expect("最终确认回调执行时窗口应仍然存在");
                assert_eq!(window.get_selected_index(), -1);
                assert!(!window.get_has_selected_card());
                let empty_card = window.get_selected_card();
                assert!(empty_card.preview.is_empty());
                assert!(empty_card.source.is_empty());
                assert!(!empty_card.is_image);
                assert!(!empty_card.copy_enabled);

                // 三态共用同一固定预览表面；失败和加载不会改变右栏布局。
                window.set_selected_card(image_preview_card(Image::default(), false, false));
                window.set_has_selected_card(true);
                window.show().expect("图片预览测试窗口应成功显示");
                i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
                let loading_snapshot = window
                    .window()
                    .take_snapshot()
                    .expect("图片加载态软件渲染快照失败");
                assert!(
                    light_pixels(&loading_snapshot, 350, 690, 160, 450) > 20,
                    "图片加载态没有形成可见占位"
                );

                window.set_selected_card(image_preview_card(Image::default(), false, true));
                i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
                let failed_snapshot = window
                    .window()
                    .take_snapshot()
                    .expect("图片失败态软件渲染快照失败");
                assert!(
                    light_pixels(&failed_snapshot, 350, 690, 160, 450) > 20,
                    "图片失败态没有形成可见占位"
                );

                window.set_selected_card(image_preview_card(solid_thumbnail(), true, false));
                i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
                let loaded_snapshot = window
                    .window()
                    .take_snapshot()
                    .expect("图片成功态软件渲染快照失败");
                assert!(
                    thumbnail_pixels(&loaded_snapshot, 350, 690, 160, 450) > 100,
                    "图片成功态没有在 contain 预览表面绘制缩略图"
                );
                slint::quit_event_loop().expect("测试事件循环应该允许退出");
            })
            .expect("最终确认回调必须能够进入事件循环");
        })
        .expect("替换结果确认回调必须能够进入事件循环");
    })
    .expect("选中投影确认回调必须能够进入事件循环");

    slint::run_event_loop().expect("测试事件循环应该正常结束");
}

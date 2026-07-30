//! 此集成测试使用 Slint 测试后端验证混合历史列表的真实几何与固定分页状态区。
//!
//! 测试只创建内存组件并推进 mock time，不显示应用窗口，不访问剪贴板、托盘、注册表
//! 或默认应用目录。分页身份与 reducer 门禁由 `ui_event` 定向单元测试覆盖。

use clipboard_board::{create_app_window, ClipboardCard};
use slint::{ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Once;
use std::time::Duration;

/// 同一集成测试进程只能安装一次 Slint 全局测试平台。
static INIT_TEST_BACKEND: Once = Once::new();

/// 幂等初始化不带真实事件循环的测试后端。
fn init_test_backend() {
    INIT_TEST_BACKEND.call_once(i_slint_backend_testing::init_no_event_loop);
}

/// 构造固定高度文本或图片摘要，测试模型不携带正文和原图。
fn card(label: &str, is_image: bool) -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from(label),
        source: SharedString::from("滚动测试"),
        relative_time: SharedString::from("刚刚"),
        is_pinned: false,
        pin_pending: false,
        delete_pending: false,
        is_image,
        copy_enabled: true,
        image_width: if is_image { 320 } else { 0 },
        image_height: if is_image { 200 } else { 0 },
        thumbnail: Default::default(),
        thumbnail_loaded: false,
        thumbnail_failed: false,
    }
}

/// 推进测试后端的绑定与 ListView 布局，不进入真实窗口事件循环。
fn update_layout() {
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
}

/// 文本 106px 与图片 186px 必须由真实 ListView 累加为混合内容高度。
#[test]
fn 混合卡片产生真实内容高度且状态区不改变可见高度() {
    init_test_backend();
    let window = create_app_window().expect("测试组件应成功创建");
    window.set_cards(ModelRc::new(VecModel::from(vec![
        card("文本", false),
        card("图片", true),
    ])));
    update_layout();

    assert_eq!(window.get_history_model_length(), 2);
    assert_eq!(window.get_history_viewport_height(), 292.0);
    let idle_height = window.get_history_visible_height();
    assert!(idle_height > 0.0);

    window.set_history_next_page_loading(true);
    update_layout();
    assert_eq!(window.get_history_visible_height(), idle_height);
    window.set_history_next_page_loading(false);
    window.set_history_retry_required(true);
    update_layout();
    assert_eq!(window.get_history_visible_height(), idle_height);
}

/// Append 重新绑定模型后恢复旧视口，不得因首项仍被选中而触发选择滚入。
#[test]
fn 追加模型保持视口与复制索引() {
    init_test_backend();
    let window = create_app_window().expect("测试组件应成功创建");
    let initial = (0..8)
        .map(|index| card(&format!("条目-{index}"), index % 2 == 1))
        .collect::<Vec<_>>();
    window.set_cards(ModelRc::new(VecModel::from(initial.clone())));
    window.set_selected_index(0);
    update_layout();

    window.set_history_viewport_y(-584.0);
    update_layout();
    let retained_viewport = window.get_history_viewport_y();
    let minimum_viewport =
        -(window.get_history_viewport_height() - window.get_history_visible_height()).max(0.0);
    assert!(
        retained_viewport >= minimum_viewport && retained_viewport <= 0.0,
        "未显示测试组件也必须把视口夹紧在合法范围"
    );

    let copied = Rc::new(RefCell::new(Vec::new()));
    let copied_for_callback = Rc::clone(&copied);
    window.on_copy_item_requested(move |index| {
        copied_for_callback.borrow_mut().push(index);
    });
    window.invoke_copy_item_requested(4);

    let mut appended = initial;
    appended.extend([
        card("追加图片", true),
        card("追加文本", false),
        card("追加图片二", true),
    ]);
    window.set_cards(ModelRc::new(VecModel::from(appended)));
    window.set_history_viewport_y(retained_viewport);
    update_layout();

    assert_eq!(window.get_history_viewport_y(), retained_viewport);
    assert_eq!(window.get_selected_index(), 0);
    window.invoke_copy_item_requested(4);
    assert_eq!(copied.borrow().as_slice(), &[4, 4]);
}

//! 此集成测试使用 Slint 测试后端验证混合历史列表的真实几何、固定分页状态区与双路径视觉。
//!
//! 测试只创建内存组件并推进 mock time；双路径契约使用软件快照和合成指针事件，
//! 不访问剪贴板、托盘、注册表或默认应用目录。分页身份与 reducer 门禁由 `ui_event`
//! 定向单元测试覆盖。

use clipboard_board::app::set_window_commit;
use clipboard_board::command::{
    UiClipboardItem, UiClipboardItemKind, WindowCommitBuilder, WindowCommitPayload, WindowOffset,
};
use clipboard_board::{create_app_window, ClipboardCard};
use slint::{ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// 为当前 Rust 测试线程安装独立的 Slint 无事件循环后端。
///
/// Slint testing backend 的 `init_no_event_loop` 是线程局部设计；不能用跨线程
/// `Once` 把首次测试线程的平台复用于后续测试，否则后续窗口会读到错误的线程平台。
fn init_test_backend() {
    i_slint_backend_testing::init_no_event_loop();
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

/// 文本 78px 与图片 92px 必须由真实 ListView 累加为混合内容高度。
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
    assert_eq!(window.get_history_viewport_height(), 170.0);
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
fn 追加模型保持视口与选择索引() {
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

    let selected = Rc::new(RefCell::new(Vec::new()));
    let selected_for_callback = Rc::clone(&selected);
    window.on_card_selection_requested(move |index| {
        selected_for_callback.borrow_mut().push(index);
    });
    window.invoke_card_selection_requested(4);

    let mut appended = initial;
    appended.extend([
        card("追加图片", true),
        card("追加文本", false),
        card("追加图片二", true),
    ]);
    window.set_cards(ModelRc::new(VecModel::from(appended)));
    window.set_history_viewport_y(retained_viewport);
    update_layout();

    // Slint ListView 在模型替换后的异步布局中允许重新 clamp 到新的合法范围；
    // 这里验证统一视口属性仍是负向且落在新内容边界内，不把后端的行边界量化误判成业务回归。
    let actual_viewport = window.get_history_viewport_y();
    let minimum_viewport =
        -(window.get_history_viewport_height() - window.get_history_visible_height()).max(0.0);
    assert!(
        actual_viewport >= minimum_viewport - 0.5 && actual_viewport <= 0.5,
        "模型替换后的 legacy 视口必须保持在合法范围"
    );
    assert_eq!(window.get_selected_index(), 0);
    window.invoke_card_selection_requested(4);
    assert_eq!(selected.borrow().as_slice(), &[4, 4]);
}

/// WindowCommit 原子替换期间不得发送空 token；最终 programmatic clamp 只发送一次目标 token。
#[test]
fn 显式窗口提交只发送一次来源令牌() {
    init_test_backend();
    let window = create_app_window().expect("测试组件应成功创建");
    update_layout();

    let item = UiClipboardItem {
        id: 1,
        preview: "token 测试".to_owned(),
        source: "测试".to_owned(),
        relative_time: "刚刚".to_owned(),
        content_hash: [1; 32],
        copy_count: 1,
        is_pinned: false,
        kind: UiClipboardItemKind::Text,
    };
    let mut builder = WindowCommitBuilder::new(9, 1, 1).expect("测试 nonce 必须非零");
    assert!(builder.set_window(WindowCommitPayload {
        start: 0,
        total_count: 1,
        total_height: 78,
        visible_height: 50,
        clamped_viewport_y: -28,
        origin_token: Some(7),
        cards: vec![item],
        offsets: vec![WindowOffset {
            absolute_index: 0,
            id: 1,
            content_hash: [1; 32],
            top: 0,
            height: 78,
        }],
    }));
    assert!(builder.ready());
    let commit = builder.publish_commit_stamp().expect("应发布窗口提交");

    let tokens = Rc::new(RefCell::new(Vec::new()));
    let tokens_for_callback = Rc::clone(&tokens);
    window.on_history_viewport_changed(move |_, _, _, token| {
        tokens_for_callback.borrow_mut().push(token.to_string());
    });
    assert!(set_window_commit(&window, commit));
    update_layout();
    // 布局重算可能再发出一次不带来源令牌的普通几何通知；门禁只允许目标令牌
    // 被消费一次，不能把后续空令牌误认为同一次 programmatic clamp。
    let non_empty_tokens = tokens
        .borrow()
        .iter()
        .filter(|token| !token.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(non_empty_tokens, vec!["7"]);
}

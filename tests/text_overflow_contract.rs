//! 此集成测试用真实软件渲染快照锁定长文本不能越过固定卡片边界。
//!
//! 回归输入使用用户实际复制的多行 SQL：既锁定短首行后仍应显示后续行，也扫描两张
//! 卡片之间的透明间隔，避免修复截断时重新引入文字越界。

use clipboard_board::{create_app_window, ClipboardCard};
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

/// 构造用户实际遇到“只显示 /*...”问题的多行 SQL 卡片。
fn long_card() -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from(
            r#"
/*
#4 正向交换；只执行一次。
执行后无论客户端返回成功、失败还是超时，都先执行 #5、#6，禁止重跑本语句。
*/
/* cutover_probe_id=replenishment_group_effect_20260728_probe_forward */
EXCHANGE TABLES
    nlp_semantic_layer.__exchange_cutover_probe_codex_20260728_a
AND nlp_semantic_layer.__exchange_cutover_probe_codex_20260728_b
ON CLUSTER default_cluster;"#,
        ),
        source: SharedString::from("测试来源"),
        relative_time: SharedString::from("刚刚"),
        is_pinned: false,
        pin_pending: false,
        delete_pending: false,
        // 长文本契约不依赖图片和复制行为，但必须填满当前 UI DTO 的安全默认值。
        is_image: false,
        copy_enabled: true,
        image_width: 0,
        image_height: 0,
        thumbnail: Default::default(),
        thumbnail_loaded: false,
        thumbnail_failed: false,
    }
}

/// 构造第二张短卡片，用于确认第一张长文本不会绘制到卡片间隔。
fn short_card() -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from("下一条"),
        source: SharedString::from("测试来源"),
        relative_time: SharedString::from("刚刚"),
        is_pinned: false,
        pin_pending: false,
        delete_pending: false,
        // 间隔采样只关心文本卡片几何，图片字段使用稳定的非图片默认值。
        is_image: false,
        copy_enabled: true,
        image_width: 0,
        image_height: 0,
        thumbnail: Default::default(),
        thumbnail_loaded: false,
        thumbnail_failed: false,
    }
}

/// 短首行之后必须显示后续正文，同时两张卡片间隔内不能出现越界文字。
#[test]
fn 多行长文本显示后续正文且不会绘制到卡片间隔() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: true,
        renderer_name: Some(SharedString::from("software")),
    })))
    .expect("测试平台只能初始化一次");
    let window = create_app_window().expect("测试窗口应成功创建");
    window.set_cards(ModelRc::new(VecModel::from(vec![
        long_card(),
        short_card(),
    ])));
    // 右栏复用同一受限摘要，真实软件渲染必须绘制正文而不是继续显示 UIR-05 占位。
    window.set_selected_card(long_card());
    window.set_has_selected_card(true);
    window.show().expect("测试窗口应成功显示");

    let snapshot = window.window().take_snapshot().expect("软件渲染快照失败");
    assert_eq!(snapshot.width(), 720);
    assert_eq!(snapshot.height(), 520);
    let pixels = snapshot.as_bytes();
    let stride = snapshot.width() as usize * 4;
    // 开头空行之后的 `/*` 和首行正文应在 287～320px 留下足够浅色字形。
    // 旧实现用 elide 只留下 `/*...`，该区域像素过少，因此能精确捕获用户截图。
    let continuation_pixels = (287_usize..321)
        .flat_map(|y| (38_usize..380).map(move |x| (x, y)))
        .filter(|(x, y)| {
            let offset = y * stride + x * 4;
            pixels[offset] > 120 && pixels[offset + 1] > 120 && pixels[offset + 2] > 120
        })
        .count();
    assert!(
        continuation_pixels > 40,
        "短首行后的正文没有显示，后续区域只有 {continuation_pixels} 个浅色像素"
    );

    // 720×520 双栏布局中 264px 左栏的文本外层行高为 78px，首张背景结束后保留 8px 透明间隔；
    // 只扫描左栏内的间隔，避免把右侧预览边界纳入文本越界判定。
    let light_pixels = (321_usize..329)
        .flat_map(|y| (38_usize..292).map(move |x| (x, y)))
        .filter(|(x, y)| {
            let offset = y * stride + x * 4;
            pixels[offset] > 120 && pixels[offset + 1] > 120 && pixels[offset + 2] > 120
        })
        .count();

    assert_eq!(
        light_pixels, 0,
        "长文本在卡片间隔中留下了 {light_pixels} 个浅色像素"
    );

    // 右栏正文位于分隔线右侧，长摘要的多行内容应在固定预览区内真实出现。
    let preview_pixels = (190_usize..480)
        .flat_map(|y| (350_usize..690).map(move |x| (x, y)))
        .filter(|(x, y)| {
            let offset = y * stride + x * 4;
            pixels[offset] > 120 && pixels[offset + 1] > 120 && pixels[offset + 2] > 120
        })
        .count();
    assert!(
        preview_pixels > 80,
        "右栏受限摘要没有形成真实正文像素，仅发现 {preview_pixels} 个浅色像素"
    );
}

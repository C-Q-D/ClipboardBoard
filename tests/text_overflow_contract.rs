//! 此集成测试用真实软件渲染快照锁定长文本不能越过固定卡片边界。
//!
//! 回归输入包含换行和超长英文标识符，复现用户截图中的预览文字穿过卡片底部并与
//! 下一张卡片重叠；测试只扫描两张卡片之间的透明间隔，不依赖具体字体字形。

use clipboard_board::{create_app_window, ClipboardCard};
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

/// 构造能够稳定产生多行换行的长文本卡片。
fn long_card() -> ClipboardCard {
    ClipboardCard {
        preview: SharedString::from(
            "EXCHANGE TABLES\n\
             nlp_semantic_layer_replenishment_group_effect_semantic_10m_20260727_r03\n\
             AND\n\
             nlp_semantic_layer__shadow_replenishment_group_effect_semantic_10m_20260727_r03\n\
             ON CLUSTER default_cluster",
        ),
        source: SharedString::from("测试来源"),
        relative_time: SharedString::from("刚刚"),
        is_pinned: false,
        pin_pending: false,
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
    }
}

/// 两张固定高度卡片之间的 10px 间隔内不能出现长文本的浅色像素。
#[test]
fn 长文本预览不会绘制到卡片间隔() {
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
    window.show().expect("测试窗口应成功显示");

    let snapshot = window.window().take_snapshot().expect("软件渲染快照失败");
    assert_eq!(snapshot.width(), 560);
    assert_eq!(snapshot.height(), 640);
    let pixels = snapshot.as_bytes();
    let stride = snapshot.width() as usize * 4;
    // 当前固定布局中首张卡片底边约为 320px，随后是 10px 透明间隔。
    let light_pixels = (321_usize..329)
        .flat_map(|y| (38_usize..420).map(move |x| (x, y)))
        .filter(|(x, y)| {
            let offset = y * stride + x * 4;
            pixels[offset] > 120 && pixels[offset + 1] > 120 && pixels[offset + 2] > 120
        })
        .count();

    assert_eq!(
        light_pixels, 0,
        "长文本在卡片间隔中留下了 {light_pixels} 个浅色像素"
    );
}

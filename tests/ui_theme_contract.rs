//! 主题契约测试只验证视觉令牌的唯一来源和系统字体决策。
//!
//! 颜色像素和最终响应式布局由后续原子通过真实软件渲染验证；本测试先阻止业务
//! Slint 文件继续散落颜色字面量或偷偷引入自定义字体资源。

/// 主题全局必须包含计划规定的核心颜色、字体和排版令牌。
#[test]
fn 主题令牌集中定义且使用系统字体回退() {
    let theme_source = include_str!("../ui/theme.slint");
    let app_source = include_str!("../ui/app-window.slint");

    for token in [
        "window-bg: #09090B",
        "shell-bg: #101014",
        "surface-bg: #15151A",
        "surface-hover: #1E1D25",
        "surface-selected: #2B2936",
        "border-subtle: #2A2931",
        "border-selected: #625C73",
        "text-primary: #F2F1F5",
        "text-secondary: #AAA7B0",
        "text-muted: #77747E",
        "accent-bg: #D8D3E4",
        "accent-text: #17151B",
        "focus-ring: #9086A6",
        "danger: #E98989",
        "danger-surface: #2A171A",
        "warning: #D9A06F",
    ] {
        assert!(theme_source.contains(token), "主题缺少令牌：{token}");
    }

    assert!(
        theme_source.contains("font-family: \"Microsoft YaHei UI\""),
        "主题必须明确声明 Microsoft YaHei UI 系统字体回退"
    );
    assert!(
        theme_source.contains("font-weight-semibold: 600"),
        "主题必须提供语义化中等标题字重"
    );
    assert!(
        !app_source.contains('#'),
        "业务 Slint 文件不得继续散落颜色字面量，所有颜色必须来自 Theme"
    );
    assert!(
        !app_source.contains("@font-face") && !app_source.contains(".woff"),
        "本轮不得从业务界面引入自定义字体资源"
    );
}

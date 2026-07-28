//! 此集成测试确保核心源码文件不会遗失中文文件级职责说明。

use std::path::Path;

/// 读取项目中的核心源码，并验证首行包含中文职责说明。
fn assert_has_chinese_file_comment(relative_path: &str) {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let content = std::fs::read_to_string(workspace_root.join(relative_path))
        .unwrap_or_else(|error| panic!("无法读取 {relative_path}：{error}"));
    let first_line = content.lines().next().unwrap_or_default();

    assert!(
        first_line.contains('此')
            || first_line.contains('负')
            || first_line.contains('定')
            || first_line.contains('脚')
            || first_line.contains('入')
            || first_line.contains('库'),
        "{relative_path} 缺少中文文件级职责说明"
    );
}

/// 核心源码均应带有中文文件级说明，避免后续原子遗忘注释要求。
#[test]
fn 核心源码包含中文文件级职责说明() {
    for relative_path in [
        "build.rs",
        "src/lib.rs",
        "src/main.rs",
        "ui/app-window.slint",
        "src/command.rs",
        "src/diagnostics.rs",
        "src/domain/mod.rs",
        "src/domain/clipboard_item.rs",
        "src/domain/hash.rs",
        "src/app/mod.rs",
        "src/app/ui_event.rs",
        "tests/ui_event.rs",
        "src/platform/mod.rs",
        "src/platform/windows/mod.rs",
        "src/platform/windows/hotkey.rs",
        "src/platform/windows/system_window.rs",
        "src/platform/windows/tray.rs",
        "src/platform/windows/single_instance.rs",
        "src/platform/windows/window/mod.rs",
        "src/platform/windows/window/lifecycle.rs",
    ] {
        assert_has_chinese_file_comment(relative_path);
    }
}

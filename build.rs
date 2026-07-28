//! 此构建脚本负责编译 Slint 标记文件，使 Rust 代码可以使用生成的界面类型。

/// 编译应用的根 Slint 界面文件；界面语法错误必须在编译期暴露。
fn main() {
    slint_build::compile("ui/app-window.slint")
        .expect("无法编译 ui/app-window.slint，请检查 Slint 界面语法");
}

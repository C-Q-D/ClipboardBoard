//! 此构建脚本负责编译 Slint 界面，并在 Windows 上接入可审计的版本与图标资源。
//!
//! 资源编译器不是所有开发环境都预装；缺失时保留可运行的开发构建，但必须输出明确
//! 警告，不能把“未嵌入图标”伪装成成功。版本和资源 ID 均从受控输入生成，避免运行时
//! 托盘代码与构建脚本各自维护一份容易漂移的常量。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 编译应用的根 Slint 界面并生成平台资源；界面语法错误必须在编译期暴露。
fn main() {
    println!("cargo:rerun-if-changed=ui/app-window.slint");
    println!("cargo:rerun-if-changed=assets/clipboard-board.svg");
    println!("cargo:rerun-if-changed=assets/clipboard-board.ico.hex");
    println!("cargo:rerun-if-changed=assets/clipboard-board-resource-id.txt");

    slint_build::compile("ui/app-window.slint")
        .expect("无法编译 ui/app-window.slint，请检查 Slint 界面语法");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo 未提供 OUT_DIR"));
    let icon_path = out_dir.join("clipboard-board.ico");
    write_icon_file(&icon_path);
    let icon_resource_id = read_icon_resource_id();
    write_generated_resource_constants(&out_dir, icon_resource_id);

    // CARGO_CFG_WINDOWS 描述的是目标平台，而不是运行构建脚本的宿主平台。
    if env::var_os("CARGO_CFG_WINDOWS").is_some() {
        configure_windows_resources(&icon_path, icon_resource_id);
    }
}

/// 将仓库内可审计的十六进制图标源解码为临时 ICO 文件，避免提交二进制构建产物。
fn write_icon_file(path: &Path) {
    let source = fs::read_to_string("assets/clipboard-board.ico.hex")
        .expect("无法读取 assets/clipboard-board.ico.hex");
    let payload: String = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    let bytes = decode_hex(&payload).expect("assets/clipboard-board.ico.hex 不是合法十六进制");
    validate_icon_directory(&bytes);
    fs::write(path, bytes).expect("无法把图标写入 Cargo OUT_DIR");
}

/// 校验 ICO 目录中的四个尺寸，避免资源编译器接收到缺尺寸或偏移损坏的图标。
fn validate_icon_directory(bytes: &[u8]) {
    if bytes.len() < 6 || bytes.get(0..4) != Some(&[0, 0, 1, 0]) {
        panic!("图标源不是有效 ICO 文件头");
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]);
    if count != 4 {
        panic!("图标源必须包含 16、32、48、256 四个目录项");
    }
    let directory_len = 6 + usize::from(count) * 16;
    if bytes.len() < directory_len {
        panic!("图标源目录长度不足");
    }
    // ICO 以 0 表示 256 像素，其他尺寸直接使用单字节宽高。
    let expected_sizes = [16_u8, 32, 48, 0];
    for (index, expected) in expected_sizes.into_iter().enumerate() {
        let entry_offset = 6 + index * 16;
        let width = bytes[entry_offset];
        let height = bytes[entry_offset + 1];
        if width != expected || height != expected {
            panic!("图标源第 {index} 个目录项尺寸不符合约定");
        }
    }
}

/// 解析图标资源 ID，并在生成 Rust 常量前拒绝空值或越界值。
fn read_icon_resource_id() -> u16 {
    let source = fs::read_to_string("assets/clipboard-board-resource-id.txt")
        .expect("无法读取 assets/clipboard-board-resource-id.txt");
    let value = source
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .expect("图标资源 ID 文件缺少数值")
        .parse::<u16>()
        .expect("图标资源 ID 必须是 0~65535 的整数");
    assert!(value > 0, "图标资源 ID 必须为正数");
    value
}

/// 生成给 Windows 托盘模块使用的常量；资源 ID 的唯一源仍是 assets 文本文件。
fn write_generated_resource_constants(out_dir: &Path, icon_resource_id: u16) {
    let generated = format!(
        "/// 由 build.rs 从 assets/clipboard-board-resource-id.txt 生成。\npub(crate) const APP_ICON_RESOURCE_ID: u16 = {icon_resource_id};\n"
    );
    fs::write(out_dir.join("clipboard_board_resources.rs"), generated)
        .expect("无法生成 Windows 资源常量");
}

/// 在资源编译器可用时嵌入版本与主图标；工具不可用时保留开发构建并明确报警。
fn configure_windows_resources(icon_path: &Path, icon_resource_id: u16) {
    let Some(resource_compiler) = locate_resource_compiler() else {
        println!("cargo:warning=Windows 资源编译器不可用，ClipboardBoard 图标和版本资源未嵌入");
        return;
    };
    // 把已解析的 SDK 目录只注入当前构建脚本进程，供 rc.exe 的依赖工具或子进程使用；
    // 资源编译器本身通过下方显式 toolkit_path 调用，避免要求用户永久修改环境变量。
    prepend_resource_compiler_path(&resource_compiler);

    let version = env::var("CARGO_PKG_VERSION").expect("Cargo 未提供 CARGO_PKG_VERSION");
    let version_value = version_resource_value(&version)
        .unwrap_or_else(|error| panic!("CARGO_PKG_VERSION 无法映射到 Windows 版本资源：{error}"));
    let mut resource = winres::WindowsResource::new();
    // 资源编号与托盘 LoadIconW 使用同一个 build.rs 生成值，避免资源漂移。
    let resource_id = icon_resource_id.to_string();
    resource.set_icon_with_id(
        icon_path
            .to_str()
            .expect("图标路径包含无法转换的非 Unicode 字符"),
        &resource_id,
    );
    resource.set("ProductName", "ClipboardBoard");
    resource.set("FileDescription", "轻量 Windows 剪贴板工作台");
    resource.set("OriginalFilename", "clipboard-board.exe");
    resource.set_version_info(winres::VersionInfo::FILEVERSION, version_value);
    resource.set_version_info(winres::VersionInfo::PRODUCTVERSION, version_value);
    // winres 的 MSVC 实现不会依赖 PATH 解析 rc.exe，而是直接拼接 toolkit_path；
    // 显式传入已定位的 bin 架构目录，确保注册表不可用时自动发现仍然真正可编译。
    resource.set_toolkit_path(
        resource_compiler
            .parent()
            .and_then(Path::to_str)
            .expect("资源编译器路径包含无法转换的非 Unicode 字符"),
    );
    resource
        .compile()
        .expect("Windows 资源编译失败，未生成可发布的版本/图标资源");
}

/// 定位可调用的 rc.exe；先使用 PATH，再探测常见 Windows SDK 安装目录。
fn locate_resource_compiler() -> Option<PathBuf> {
    if let Ok(output) = Command::new("where.exe").arg("rc.exe").output() {
        if output.status.success() {
            if let Some(path) = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .find(|path| path.is_file())
            {
                return Some(path);
            }
        }
    }

    let mut sdk_bin_roots = Vec::new();
    for variable in ["WindowsSdkDir", "ProgramFiles(x86)", "ProgramFiles"] {
        let Some(root) = env::var_os(variable).map(PathBuf::from) else {
            continue;
        };
        let root = if variable == "WindowsSdkDir" {
            root.join("bin")
        } else {
            root.join("Windows Kits").join("10").join("bin")
        };
        if root.is_dir() {
            sdk_bin_roots.push(root);
        }
    }

    let mut candidates = Vec::new();
    for root in sdk_bin_roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        let mut versions = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        // 版本目录名称可直接按字符串倒序，优先使用最新 SDK，避免旧资源工具拒绝新格式。
        versions.sort_by(|left, right| right.cmp(left));
        for version in versions {
            candidates.push(version.join("x64").join("rc.exe"));
            candidates.push(version.join("x86").join("rc.exe"));
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// 将 rc.exe 所在目录放到当前构建脚本的 PATH 首位；不会修改用户持久化环境变量。
fn prepend_resource_compiler_path(resource_compiler: &Path) {
    let Some(parent) = resource_compiler.parent() else {
        return;
    };
    let mut path = parent.as_os_str().to_owned();
    if let Some(existing) = env::var_os("PATH") {
        path.push(";");
        path.push(existing);
    }
    env::set_var("PATH", path);
}

/// 将 SemVer 的前三段映射为四段 16 位 Windows 文件版本字段。
fn version_resource_value(version: &str) -> Result<u64, String> {
    let mut fields = version.split('.');
    let major = parse_version_field(fields.next(), "major")?;
    let minor = parse_version_field(fields.next(), "minor")?;
    let patch = parse_version_field(fields.next(), "patch")?;
    if fields.next().is_some() {
        return Err("版本段数超过三段".to_owned());
    }
    Ok((u64::from(major) << 48) | (u64::from(minor) << 32) | (u64::from(patch) << 16))
}

/// 解析单个版本字段并限制在 Windows 资源支持的 16 位范围内。
fn parse_version_field(value: Option<&str>, field: &str) -> Result<u16, String> {
    value
        .ok_or_else(|| format!("缺少 {field} 字段"))?
        .parse::<u16>()
        .map_err(|_| format!("{field} 不是 0~65535 的整数"))
}

/// 解码十六进制文本；返回错误位置，便于资源源文件损坏时快速定位。
fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) {
        return Err("十六进制字符数必须为偶数".to_owned());
    }
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len() / 2);
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        let high = hex_digit(pair[0]).ok_or_else(|| format!("第 {index} 个字节包含非法字符"))?;
        let low = hex_digit(pair[1]).ok_or_else(|| format!("第 {index} 个字节包含非法字符"))?;
        output.push((high << 4) | low);
    }
    Ok(output)
}

/// 解析一个 ASCII 十六进制字符。
fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    //! 构建脚本测试只覆盖资源输入解析，不启动真实资源编译器。

    use super::{decode_hex, version_resource_value};

    #[test]
    fn 版本资源来自三段_semver() {
        assert_eq!(version_resource_value("1.2.3"), Ok(0x0001_0002_0003_0000));
        assert!(version_resource_value("1.2").is_err());
        assert!(version_resource_value("1.2.3.4").is_err());
    }

    #[test]
    fn 十六进制图标输入可解码() {
        assert_eq!(decode_hex("00000100"), Ok(vec![0, 0, 1, 0]));
        assert!(decode_hex("0").is_err());
        assert!(decode_hex("zz").is_err());
    }
}

//! 此模块定义文本规范化和 BLAKE3 内容哈希规则。
//!
//! 规范化只移除 Windows 文本格式常见的尾部 NUL；普通空格、换行、制表符、Unicode
//! 字符和正文中的 NUL 均保持不变。哈希输入带稳定域标签，防止未来图片或其他 payload
//! 误用同一哈希命名空间。

/// 文本哈希的固定域标签；版本变化时必须显式迁移去重语义。
pub const TEXT_HASH_DOMAIN: &[u8] = b"ClipboardBoard/text/v1\0";

/// 移除文本末尾的所有 NUL 字符，不改变其他任何字符或空白。
///
/// `CF_UNICODETEXT` 通常包含一个或多个尾部终止 NUL；保留内部 NUL 是为了不损坏用户
/// 实际复制的内容。返回拥有型字符串，便于 payload 在剪贴板句柄关闭后继续使用。
pub fn normalize_text(text: &str) -> String {
    text.trim_end_matches('\0').to_owned()
}

/// 计算规范化文本的 BLAKE3 哈希。
///
/// 哈希输入严格为 `TEXT_HASH_DOMAIN` 加规范化后的 UTF-8 字节；调用方不得把未规范化
/// 的尾部终止符或其他格式字段拼进同一输入，否则会破坏跨来源去重稳定性。
pub fn hash_text(text: &str) -> [u8; 32] {
    // 仅借用规范化切片计算哈希，避免无尾部 NUL 的大文本发生额外分配。
    hash_normalized_text(text.trim_end_matches('\0'))
}

/// 对已经规范化的文本计算哈希，供领域模型避免重复分配。
pub(crate) fn hash_normalized_text(normalized_text: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(TEXT_HASH_DOMAIN);
    hasher.update(normalized_text.as_bytes());
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    //! 此测试模块锁定文本哈希的规范化边界和 Unicode 稳定性。

    use super::{hash_text, normalize_text, TEXT_HASH_DOMAIN};

    /// 尾部 NUL 只影响传输终止，不应改变正文哈希；内部 NUL 必须保留。
    #[test]
    fn 只去除尾部_nul_并保留内部_nul() {
        assert_eq!(normalize_text("前\0中\0\0"), "前\0中");
        assert_eq!(hash_text("内容\0\0"), hash_text("内容"));
        assert_ne!(hash_text("前\0中"), hash_text("前中"));
    }

    /// 空格、换行和制表符都是用户正文，不能被 trim 或折叠。
    #[test]
    fn 空白字符保持原样并参与哈希() {
        let text = "  第一行\n\t第二行  ";
        assert_eq!(normalize_text(text), text);
        assert_ne!(hash_text("a"), hash_text(" a"));
        assert_ne!(hash_text("a\n"), hash_text("a\t"));
    }

    /// 多语言和域标签必须保持确定性，避免使用平台本地编码。
    #[test]
    fn unicode_哈希稳定且包含文本域标签() {
        let first = hash_text("中文 · Русский · 日本語 · 😀");
        let second = hash_text("中文 · Русский · 日本語 · 😀");
        assert_eq!(first, second);
        assert!(!TEXT_HASH_DOMAIN.is_empty());
        assert_ne!(first, [0; 32]);
    }
}

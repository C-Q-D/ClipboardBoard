//! 此模块定义文本剪贴板的 payload、摘要和列表展示边界。
//!
//! `ClipboardPayload` 持有可重新粘贴的完整规范化正文；`ClipboardItemSummary` 只携带
//! 哈希、大小和受限预览，供列表或跨线程 DTO 使用。摘要绝不会把大文本完整复制到 UI
//! 模型，正文所有权在进入领域层后与系统剪贴板句柄彻底解耦。

use super::hash::hash_normalized_text;

/// 列表摘要允许的最大 Unicode 标量数量；截断在字符边界进行，不产生无效 UTF-8。
pub const TEXT_SUMMARY_MAX_CHARS: usize = 512;

/// 文本记录的轻量列表摘要，不包含可重新粘贴的完整正文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardItemSummary {
    /// BLAKE3 文本域哈希，用于去重和后续持久化键。
    pub content_hash: [u8; 32],
    /// 规范化正文的 UTF-8 字节数，而不是摘要字节数。
    pub byte_count: u64,
    /// 面向列表的受限预览，保留原有空白和换行。
    pub preview: String,
    /// 预览是否因达到字符上限而截断。
    pub is_truncated: bool,
}

/// 当前原子支持的完整剪贴板 payload。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardPayload {
    /// 已规范化、拥有所有权的纯文本正文。
    Text(String),
}

impl ClipboardPayload {
    /// 从外部文本构造 payload，并只移除尾部 NUL 终止符。
    pub fn from_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let normalized = text.trim_end_matches('\0');
        // 常见路径没有尾部终止符时直接复用原字符串，避免大文本无意义的二次分配。
        if normalized.len() == text.len() {
            Self::Text(text)
        } else {
            Self::Text(normalized.to_owned())
        }
    }

    /// 返回文本正文；调用方可借用它计算摘要或交给后续持久化层。
    pub fn as_text(&self) -> &str {
        match self {
            Self::Text(text) => text,
        }
    }

    /// 为当前 payload 生成不含完整正文的列表摘要。
    pub fn summary(&self) -> ClipboardItemSummary {
        let text = self.as_text();
        let mut characters = text.chars();
        let preview: String = characters.by_ref().take(TEXT_SUMMARY_MAX_CHARS).collect();
        let is_truncated = characters.next().is_some();

        ClipboardItemSummary {
            content_hash: hash_normalized_text(text),
            byte_count: text.len() as u64,
            is_truncated,
            preview,
        }
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证 payload 所有权、摘要截断和正文空白边界。

    use super::{ClipboardPayload, TEXT_SUMMARY_MAX_CHARS};
    use crate::domain::hash::hash_text;

    /// 构造后尾部 NUL 被移除，正文仍由 payload 独立拥有。
    #[test]
    fn payload_拥有规范化文本() {
        let source = String::from("  保留空格\n\t内容\0\0");
        let payload = ClipboardPayload::from_text(source);
        assert_eq!(payload.as_text(), "  保留空格\n\t内容");
    }

    /// 摘要达到字符上限后停止，Unicode 不会被按字节切断。
    #[test]
    fn 摘要按_unicode_字符边界截断() {
        let text = "😀".repeat(TEXT_SUMMARY_MAX_CHARS + 3);
        let payload = ClipboardPayload::from_text(text.clone());
        let summary = payload.summary();

        assert_eq!(summary.preview.chars().count(), TEXT_SUMMARY_MAX_CHARS);
        assert!(summary.is_truncated);
        assert_eq!(summary.byte_count, text.len() as u64);
        assert_eq!(summary.content_hash, hash_text(&text));
    }

    /// 短文本摘要必须完整保留空格、换行和制表符。
    #[test]
    fn 短摘要保留全部空白() {
        let text = "  第一行\n\t第二行  ";
        let summary = ClipboardPayload::from_text(text).summary();
        assert_eq!(summary.preview, text);
        assert!(!summary.is_truncated);
    }
}

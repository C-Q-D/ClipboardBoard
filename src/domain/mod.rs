//! 此模块汇总剪贴板领域模型和内容哈希规则。
//!
//! 领域层只表达拥有型数据和纯函数，不依赖 UI、Win32、SQLite 或后台线程；后续捕获、
//! 存储和 UI 原子应复用这里的规范化与摘要契约，避免各层自行解释文本。

pub mod clipboard_item;
pub mod hash;
pub mod image_metadata;

pub use clipboard_item::{ClipboardItemSummary, ClipboardPayload, TEXT_SUMMARY_MAX_CHARS};
pub use hash::{hash_text, normalize_text, TEXT_HASH_DOMAIN};
pub use image_metadata::{
    ImageAssetRelativePath, ImageAssetRootId, ImageMetadata, ImageMetadataError,
    ImageOriginalFormat,
};

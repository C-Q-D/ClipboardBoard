//! 此模块汇总规范图片到耐久原图、缩略图和后台结果的流水线。
//!
//! 当前阶段只导出无文件系统副作用的编码能力；staging 发布与 worker 在后续原子接入。

mod encoding;

pub use encoding::{
    build_thumbnail, encode_original_png, encode_thumbnail_webp, ImageEncodingError,
    ThumbnailPixels, MAX_PERSISTED_PNG_BYTES, MAX_THUMBNAIL_EDGE,
};

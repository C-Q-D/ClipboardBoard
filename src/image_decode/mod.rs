//! 此模块汇总外部图片编码到规范 RGBA8 像素的有界解码入口。
//!
//! 各格式解析器只负责拥有型字节或借用切片，不访问剪贴板、磁盘、SQLite 或 UI。

mod dib;
mod png;

pub use dib::{decode_dib, DibDecodeError, MAX_DIB_ENCODED_BYTES};

pub use png::{
    decode_registered_png, PngDecodeError, MAX_IMAGE_DIMENSION, MAX_IMAGE_RGBA_BYTES,
    MAX_PNG_ENCODED_BYTES,
};

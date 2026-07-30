//! 此模块汇总规范图片到耐久原图、缩略图和后台结果的流水线。
//!
//! 编码与文件发布保持分层；worker 在后续原子接入。

mod encoding;
mod publish;
mod worker;

pub use encoding::{
    build_thumbnail, encode_original_png, encode_thumbnail_webp, ImageEncodingError,
    ThumbnailPixels, MAX_PERSISTED_PNG_BYTES, MAX_THUMBNAIL_EDGE,
};
// PIPE-03 将在下一原子消费这两个 crate 内接缝；当前先保持原子提交可独立编译。
#[allow(unused_imports)]
pub(crate) use publish::{publish_image_assets, PublishedImageAssets};
pub use worker::{
    select_image_input, ImageFinalizeHandle, ImageInput, ImageInputError, ImageInputFormat,
    ImageRootSnapshot, ImageWorker, ImageWorkerError, ImageWorkerResult, ImageWorkerSender,
};

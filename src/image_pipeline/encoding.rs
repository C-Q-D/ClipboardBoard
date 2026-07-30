//! 此模块把规范 RGBA8 编码为耐久 PNG 原图和不放大的 lossless WebP 缩略图。
//!
//! 缩放从借用原图直接写目标 RGBA，不复制整份原始像素；PNG writer 使用独立计数
//! 上限，避免把剪贴板注册 PNG 的 30 MiB 限制错误复用于耐久资产。

use std::{
    fmt,
    io::{self, Write},
};

use image::{
    codecs::{png::PngEncoder, webp::WebPEncoder},
    ExtendedColorType, ImageEncoder,
};

use crate::{
    domain::CanonicalImagePixels,
    image_decode::{MAX_IMAGE_DIMENSION, MAX_IMAGE_RGBA_BYTES},
};

/// 耐久 PNG 原图的独立最大编码字节数：80 MiB。
pub const MAX_PERSISTED_PNG_BYTES: u64 = 80 * 1024 * 1024;
/// 缩略图最长边固定上限。
pub const MAX_THUMBNAIL_EDGE: u32 = 320;
/// lossless WebP 缩略图编码上限。
const MAX_THUMBNAIL_WEBP_BYTES: u64 = 2 * 1024 * 1024;

/// 图片编码失败的稳定分类，不携带像素、路径或外部错误文本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageEncodingError {
    /// 输入尺寸或 RGBA 长度超过规范流水线上限。
    InputTooLarge,
    /// 缩略图尺寸计算溢出或产生不满足约束的结果。
    InvalidThumbnailDimensions,
    /// PNG 或 WebP 编码器拒绝输入或 writer 失败。
    EncodeFailed,
    /// 编码输出超过对应固定上限。
    EncodedOutputTooLarge,
}

impl fmt::Display for ImageEncodingError {
    /// 返回不泄漏图片内容和 writer 细节的中文错误。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLarge => write!(formatter, "规范图片输入超过编码上限"),
            Self::InvalidThumbnailDimensions => write!(formatter, "缩略图目标尺寸无效"),
            Self::EncodeFailed => write!(formatter, "图片编码失败"),
            Self::EncodedOutputTooLarge => write!(formatter, "图片编码输出超过上限"),
        }
    }
}

impl std::error::Error for ImageEncodingError {}

/// 最长边不超过 320px 的拥有型 RGBA8 缩略图。
#[derive(Clone, Eq, PartialEq)]
pub struct ThumbnailPixels {
    /// 非零缩略图宽度。
    width: u32,
    /// 非零缩略图高度。
    height: u32,
    /// 顶向下、行连续的 RGBA8 缩略图字节。
    rgba: Vec<u8>,
}

impl fmt::Debug for ThumbnailPixels {
    /// Debug 只输出尺寸和长度，禁止泄漏图片像素。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThumbnailPixels")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("rgba_len", &self.rgba.len())
            .finish()
    }
}

impl ThumbnailPixels {
    /// 返回缩略图宽度。
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// 返回缩略图高度。
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// 借用缩略图 RGBA8 字节供 WebP 编码和发布后精确验证。
    pub fn as_rgba_bytes(&self) -> &[u8] {
        &self.rgba
    }
}

/// 使用独立 80 MiB 上限把规范像素写为 PNG，并返回实际编码字节数。
pub fn encode_original_png(
    image: &CanonicalImagePixels,
    writer: &mut impl Write,
) -> Result<u64, ImageEncodingError> {
    encode_original_png_with_limit(image, writer, MAX_PERSISTED_PNG_BYTES)
}

/// 从规范像素建立不放大的最长边 320px 缩略图。
pub fn build_thumbnail(
    image: &CanonicalImagePixels,
) -> Result<ThumbnailPixels, ImageEncodingError> {
    validate_input_with_limit(image, MAX_IMAGE_RGBA_BYTES)?;
    let (target_width, target_height) = thumbnail_dimensions(image.width(), image.height())?;
    // 区域平均直接写目标 RGBA8，额外像素缓冲严格不超过 320×320×4。
    let rgba = downscale_area_average(
        image.as_rgba_bytes(),
        image.width(),
        image.height(),
        target_width,
        target_height,
    )?;
    Ok(ThumbnailPixels {
        width: target_width,
        height: target_height,
        rgba,
    })
}

/// 把缩略图写为 lossless WebP，并返回实际编码字节数。
pub fn encode_thumbnail_webp(
    thumbnail: &ThumbnailPixels,
    writer: &mut impl Write,
) -> Result<u64, ImageEncodingError> {
    let mut limited = LimitedWriter::new(writer, MAX_THUMBNAIL_WEBP_BYTES);
    let result = WebPEncoder::new_lossless(&mut limited).write_image(
        thumbnail.as_rgba_bytes(),
        thumbnail.width,
        thumbnail.height,
        ExtendedColorType::Rgba8,
    );
    finish_encoding(result, limited)
}

/// 使用可注入上限编码 PNG，定向测试无需分配真实 80 MiB 文件。
fn encode_original_png_with_limit(
    image: &CanonicalImagePixels,
    writer: &mut impl Write,
    limit: u64,
) -> Result<u64, ImageEncodingError> {
    validate_input_with_limit(image, MAX_IMAGE_RGBA_BYTES)?;
    let mut limited = LimitedWriter::new(writer, limit);
    let result = PngEncoder::new(&mut limited).write_image(
        image.as_rgba_bytes(),
        image.width(),
        image.height(),
        ExtendedColorType::Rgba8,
    );
    finish_encoding(result, limited)
}

/// 收敛编码器与限长 writer 结果，并返回已写字节数。
fn finish_encoding<W: Write>(
    result: image::ImageResult<()>,
    writer: LimitedWriter<'_, W>,
) -> Result<u64, ImageEncodingError> {
    if writer.exceeded {
        return Err(ImageEncodingError::EncodedOutputTooLarge);
    }
    result
        .map(|_| writer.written)
        .map_err(|_| ImageEncodingError::EncodeFailed)
}

/// 再次验证编码入口尺寸与 RGBA 长度上限，防止其他调用方绕过来源解码器。
fn validate_input_with_limit(
    image: &CanonicalImagePixels,
    rgba_limit: usize,
) -> Result<(), ImageEncodingError> {
    validate_dimensions_and_length(
        image.width(),
        image.height(),
        image.as_rgba_bytes().len(),
        rgba_limit,
    )
}

/// 纯数值验证尺寸、实际长度和 RGBA 上限，边界测试无需分配 64 MiB。
fn validate_dimensions_and_length(
    width: u32,
    height: u32,
    actual_length: usize,
    rgba_limit: usize,
) -> Result<(), ImageEncodingError> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ImageEncodingError::InputTooLarge)?;
    if width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || expected > rgba_limit
        || actual_length != expected
    {
        return Err(ImageEncodingError::InputTooLarge);
    }
    Ok(())
}

/// 使用整数区域平均把借用原图直接缩到目标缓冲，不建立行或浮点中间图。
fn downscale_area_average(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> Result<Vec<u8>, ImageEncodingError> {
    let target_length = (target_width as usize)
        .checked_mul(target_height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ImageEncodingError::InvalidThumbnailDimensions)?;
    let mut target = Vec::with_capacity(target_length);
    for target_y in 0..target_height {
        let source_y_start =
            u64::from(target_y) * u64::from(source_height) / u64::from(target_height);
        let source_y_end = (u64::from(target_y + 1) * u64::from(source_height)
            / u64::from(target_height))
        .max(source_y_start + 1);
        for target_x in 0..target_width {
            let source_x_start =
                u64::from(target_x) * u64::from(source_width) / u64::from(target_width);
            let source_x_end = (u64::from(target_x + 1) * u64::from(source_width)
                / u64::from(target_width))
            .max(source_x_start + 1);
            let mut sums = [0_u64; 4];
            let mut sample_count = 0_u64;
            for source_y in source_y_start..source_y_end {
                for source_x in source_x_start..source_x_end {
                    let pixel = (source_y as usize * source_width as usize + source_x as usize) * 4;
                    for channel in 0..4 {
                        sums[channel] += u64::from(source[pixel + channel]);
                    }
                    sample_count += 1;
                }
            }
            for sum in sums {
                target.push(((sum + sample_count / 2) / sample_count) as u8);
            }
        }
    }
    if target.len() != target_length {
        return Err(ImageEncodingError::InvalidThumbnailDimensions);
    }
    Ok(target)
}

/// 按最长边计算不放大的缩略图尺寸，短边向下取整但至少为 1。
fn thumbnail_dimensions(width: u32, height: u32) -> Result<(u32, u32), ImageEncodingError> {
    let longest = width.max(height);
    if longest <= MAX_THUMBNAIL_EDGE {
        return Ok((width, height));
    }
    let (target_width, target_height) = if width >= height {
        let scaled = u64::from(height)
            .checked_mul(u64::from(MAX_THUMBNAIL_EDGE))
            .ok_or(ImageEncodingError::InvalidThumbnailDimensions)?
            / u64::from(width);
        (
            MAX_THUMBNAIL_EDGE,
            u32::try_from(scaled.max(1))
                .map_err(|_| ImageEncodingError::InvalidThumbnailDimensions)?,
        )
    } else {
        let scaled = u64::from(width)
            .checked_mul(u64::from(MAX_THUMBNAIL_EDGE))
            .ok_or(ImageEncodingError::InvalidThumbnailDimensions)?
            / u64::from(height);
        (
            u32::try_from(scaled.max(1))
                .map_err(|_| ImageEncodingError::InvalidThumbnailDimensions)?,
            MAX_THUMBNAIL_EDGE,
        )
    };
    if target_width == 0
        || target_height == 0
        || target_width > width
        || target_height > height
        || target_width > MAX_THUMBNAIL_EDGE
        || target_height > MAX_THUMBNAIL_EDGE
    {
        return Err(ImageEncodingError::InvalidThumbnailDimensions);
    }
    Ok((target_width, target_height))
}

/// 拒绝超过上限的 writer 包装。
struct LimitedWriter<'a, W> {
    /// 调用方提供的实际 writer。
    inner: &'a mut W,
    /// 已成功写出的字节数。
    written: u64,
    /// 固定最大允许字节数。
    limit: u64,
    /// 是否观察到超过上限的写请求。
    exceeded: bool,
}

impl<'a, W> LimitedWriter<'a, W> {
    /// 创建尚未写入的限长 writer。
    fn new(inner: &'a mut W, limit: u64) -> Self {
        Self {
            inner,
            written: 0,
            limit,
            exceeded: false,
        }
    }
}

impl<W: Write> Write for LimitedWriter<'_, W> {
    /// 只有整个切片仍在剩余预算内才写入，避免越界输出。
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let requested = u64::try_from(buffer.len()).map_err(|_| io::Error::other("size"))?;
        let next = self.written.checked_add(requested);
        if next.is_none() || next.is_some_and(|next| next > self.limit) {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "encoded output limit",
            ));
        }
        let written = self.inner.write(buffer)?;
        self.written = self
            .written
            .checked_add(written as u64)
            .ok_or_else(|| io::Error::other("size"))?;
        Ok(written)
    }

    /// 转发 flush；文件级 sync 和关闭由 PIPE-02 负责。
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证 PNG/WebP 往返、缩略图尺寸、脱敏 Debug 和可注入资源上限。

    use super::{
        build_thumbnail, encode_original_png, encode_original_png_with_limit,
        encode_thumbnail_webp, validate_dimensions_and_length, validate_input_with_limit,
        ImageEncodingError, MAX_THUMBNAIL_EDGE,
    };
    use crate::domain::CanonicalImagePixels;
    use crate::image_decode::MAX_IMAGE_RGBA_BYTES;

    /// 构造固定颜色的规范图片，测试尺寸保持较小。
    fn test_image(width: u32, height: u32) -> CanonicalImagePixels {
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for index in 0..width as usize * height as usize {
            rgba.extend_from_slice(&[
                (index % 251) as u8,
                (index % 241) as u8,
                (index % 239) as u8,
                (index % 233) as u8,
            ]);
        }
        CanonicalImagePixels::new(width, height, rgba).expect("构造测试图片失败")
    }

    /// PNG 编码往返必须精确保留尺寸和 RGBA8。
    #[test]
    fn png_round_trip_preserves_pixels() {
        let source = test_image(3, 2);
        let mut encoded = Vec::new();
        let byte_count = encode_original_png(&source, &mut encoded).expect("PNG 编码失败");
        let decoded = image::load_from_memory(&encoded)
            .expect("PNG 解码失败")
            .into_rgba8();
        assert_eq!(byte_count as usize, encoded.len());
        assert_eq!(decoded.dimensions(), (3, 2));
        assert_eq!(decoded.as_raw(), source.as_rgba_bytes());
    }

    /// 横图、竖图、极端短边和小图均满足不放大、最长边 320。
    #[test]
    fn thumbnail_dimensions_are_bounded_and_nonzero() {
        for (input, expected) in [
            ((640, 320), (320, 160)),
            ((320, 640), (160, 320)),
            ((16_384, 1), (320, 1)),
            ((1, 16_384), (1, 320)),
            ((80, 40), (80, 40)),
        ] {
            let thumbnail = build_thumbnail(&test_image(input.0, input.1)).expect("缩略图失败");
            assert_eq!((thumbnail.width(), thumbnail.height()), expected);
            assert!(thumbnail.width() <= input.0 && thumbnail.height() <= input.1);
            assert!(
                thumbnail.width() <= MAX_THUMBNAIL_EDGE && thumbnail.height() <= MAX_THUMBNAIL_EDGE
            );
        }
    }

    /// lossless WebP 往返必须与确定性缩略图逐字节一致。
    #[test]
    fn webp_round_trip_preserves_thumbnail_pixels() {
        let thumbnail = build_thumbnail(&test_image(401, 203)).expect("建立缩略图失败");
        let mut encoded = Vec::new();
        let byte_count = encode_thumbnail_webp(&thumbnail, &mut encoded).expect("WebP 编码失败");
        let decoded = image::load_from_memory(&encoded)
            .expect("WebP 解码失败")
            .into_rgba8();
        assert_eq!(byte_count as usize, encoded.len());
        assert_eq!(
            decoded.dimensions(),
            (thumbnail.width(), thumbnail.height())
        );
        assert_eq!(decoded.as_raw(), thumbnail.as_rgba_bytes());
    }

    /// 可注入小上限必须证明输入和编码输出不会静默越界。
    #[test]
    fn injected_limits_reject_input_and_encoded_output() {
        let source = test_image(4, 4);
        assert_eq!(
            validate_input_with_limit(&source, 63),
            Err(ImageEncodingError::InputTooLarge)
        );
        let mut encoded = Vec::new();
        assert_eq!(
            encode_original_png_with_limit(&source, &mut encoded, 8),
            Err(ImageEncodingError::EncodedOutputTooLarge)
        );
    }

    /// 默认 64 MiB RGBA 边界必须恰好允许，超出则拒绝，且测试不分配大缓冲。
    #[test]
    fn default_rgba_limit_accepts_exact_boundary_only() {
        assert_eq!(
            validate_dimensions_and_length(4096, 4096, MAX_IMAGE_RGBA_BYTES, MAX_IMAGE_RGBA_BYTES,),
            Ok(())
        );
        let oversized_length = 4097_usize * 4096 * 4;
        assert_eq!(
            validate_dimensions_and_length(4097, 4096, oversized_length, MAX_IMAGE_RGBA_BYTES,),
            Err(ImageEncodingError::InputTooLarge)
        );
    }

    /// 缩略图 Debug 只允许结构摘要，不能包含像素序列。
    #[test]
    fn thumbnail_debug_redacts_pixels() {
        let thumbnail = build_thumbnail(&test_image(2, 1)).expect("建立缩略图失败");
        assert_eq!(
            format!("{thumbnail:?}"),
            "ThumbnailPixels { width: 2, height: 1, rgba_len: 8 }"
        );
    }
}

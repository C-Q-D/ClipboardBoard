//! 此模块将有界 PNG 编码解码为顶向下、行连续的规范 RGBA8 像素。
//!
//! PNG 头中的尺寸在像素解码前完成显式 checked arithmetic，避免压缩炸弹先触发
//! 大块 RGBA 分配；外部解码器错误被收敛为不含图片数据的稳定枚举。

use std::{fmt, io::Cursor};

use image::{codecs::png::PngDecoder, DynamicImage, ImageDecoder, Limits};

use crate::domain::{CanonicalImageError, CanonicalImagePixels};

/// 单份 PNG 编码的最大字节数：30 MiB。
pub const MAX_PNG_ENCODED_BYTES: usize = 30 * 1024 * 1024;
/// 单个图片维度的最大值，避免极端长条图片拖累后续处理。
pub const MAX_IMAGE_DIMENSION: u32 = 16_384;
/// 规范 RGBA8 载荷的最大字节数：64 MiB。
pub const MAX_IMAGE_RGBA_BYTES: usize = 64 * 1024 * 1024;

/// PNG 解码失败的稳定分类，不携带外部错误文本或图片字节。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PngDecodeError {
    /// 编码切片为空。
    Empty,
    /// PNG 编码超过固定 30 MiB 上限。
    EncodedTooLarge,
    /// PNG 签名、头、校验、数据流或颜色编码无效。
    DecodeFailed,
    /// 宽度或高度超过固定单维上限。
    DimensionsTooLarge,
    /// `宽 × 高 × 4` 溢出或超过固定 RGBA8 上限。
    DecodedTooLarge,
    /// 解码器结果未满足 ATOM-31 的规范像素不变量。
    Canonical(CanonicalImageError),
}

impl fmt::Display for PngDecodeError {
    /// 返回不包含编码字节和外部解码器文本的中文错误。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(formatter, "PNG 编码为空"),
            Self::EncodedTooLarge => write!(formatter, "PNG 编码超过大小上限"),
            Self::DecodeFailed => write!(formatter, "PNG 编码无法解码"),
            Self::DimensionsTooLarge => write!(formatter, "PNG 图片维度超过上限"),
            Self::DecodedTooLarge => write!(formatter, "PNG 解码像素超过上限"),
            Self::Canonical(_) => write!(formatter, "PNG 解码结果不符合规范像素约束"),
        }
    }
}

impl std::error::Error for PngDecodeError {}

/// 把注册 PNG 编码解码为规范 RGBA8 像素。
///
/// 判定顺序固定为编码长度、IHDR 尺寸、单维限制、RGBA 长度、像素解码和领域复核。
/// 调用方可以传入关闭剪贴板后保存的拥有型字节切片；函数不访问任何系统句柄。
pub fn decode_registered_png(bytes: &[u8]) -> Result<CanonicalImagePixels, PngDecodeError> {
    if bytes.is_empty() {
        return Err(PngDecodeError::Empty);
    }
    if bytes.len() > MAX_PNG_ENCODED_BYTES {
        return Err(PngDecodeError::EncodedTooLarge);
    }

    let (width, height) = read_ihdr_dimensions(bytes)?;
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err(PngDecodeError::DimensionsTooLarge);
    }
    let rgba_length = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(PngDecodeError::DecodedTooLarge)?;
    if rgba_length > MAX_IMAGE_RGBA_BYTES {
        return Err(PngDecodeError::DecodedTooLarge);
    }

    // 必须在构造解码器时传入限制，使压缩元数据和像素头解析同样受内存上限保护。
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_RGBA_BYTES as u64);
    let decoder = PngDecoder::with_limits(Cursor::new(bytes), limits)
        .map_err(|_| PngDecodeError::DecodeFailed)?;
    if decoder.total_bytes() > MAX_IMAGE_RGBA_BYTES as u64 {
        return Err(PngDecodeError::DecodedTooLarge);
    }

    let decoded = DynamicImage::from_decoder(decoder).map_err(|_| PngDecodeError::DecodeFailed)?;
    let rgba = decoded.into_rgba8().into_raw();
    // ATOM-31 再次精确验证解码器返回的尺寸和字节数，避免依赖外部库隐含不变量。
    CanonicalImagePixels::new(width, height, rgba).map_err(PngDecodeError::Canonical)
}

/// 从 PNG 签名后的首个 IHDR 块读取非零宽高，不进行像素解码。
fn read_ihdr_dimensions(bytes: &[u8]) -> Result<(u32, u32), PngDecodeError> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24
        || &bytes[..8] != PNG_SIGNATURE
        || u32::from_be_bytes(bytes[8..12].try_into().expect("固定四字节切片")) != 13
        || &bytes[12..16] != b"IHDR"
    {
        return Err(PngDecodeError::DecodeFailed);
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("固定四字节切片"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("固定四字节切片"));
    if width == 0 || height == 0 {
        return Err(PngDecodeError::DecodeFailed);
    }
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证 PNG 色彩转换、损坏输入和解码前资源边界。

    use image::{codecs::png::PngEncoder, ExtendedColorType, ImageEncoder};

    use super::{
        decode_registered_png, PngDecodeError, MAX_IMAGE_DIMENSION, MAX_PNG_ENCODED_BYTES,
    };

    /// 编码一份小型测试 PNG，生产解码入口仍只接受普通字节切片。
    fn encode_png(width: u32, height: u32, color: ExtendedColorType, pixels: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::new();
        PngEncoder::new(&mut encoded)
            .write_image(pixels, width, height, color)
            .expect("编码测试 PNG 失败");
        encoded
    }

    /// 构造只含签名和 IHDR 尺寸字段的头，用于证明超限在完整解码前被拒绝。
    fn partial_png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13_u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    /// 修改一份有效 PNG 的 IHDR 尺寸并重算 CRC，构造无需分配完整像素的解码头测试。
    fn patch_ihdr_dimensions(mut encoded: Vec<u8>, width: u32, height: u32) -> Vec<u8> {
        encoded[16..20].copy_from_slice(&width.to_be_bytes());
        encoded[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = png_crc32(&encoded[12..29]);
        encoded[29..33].copy_from_slice(&crc.to_be_bytes());
        encoded
    }

    /// 计算 PNG chunk 使用的标准 CRC-32，只供小型测试 fixture 修补头部。
    fn png_crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xffff_ffff_u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = 0_u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    /// 验证 RGBA 与 RGB 都转换为精确的顶向下 RGBA8。
    #[test]
    fn decodes_rgba_and_rgb_pixels() {
        let rgba = encode_png(
            2,
            1,
            ExtendedColorType::Rgba8,
            &[255, 0, 0, 128, 0, 0, 255, 255],
        );
        let rgba = decode_registered_png(&rgba).expect("解码 RGBA PNG 失败");
        assert_eq!(rgba.width(), 2);
        assert_eq!(rgba.height(), 1);
        assert_eq!(rgba.as_rgba_bytes(), &[255, 0, 0, 128, 0, 0, 255, 255]);

        let rgb = encode_png(1, 1, ExtendedColorType::Rgb8, &[1, 2, 3]);
        assert_eq!(
            decode_registered_png(&rgb)
                .expect("解码 RGB PNG 失败")
                .as_rgba_bytes(),
            &[1, 2, 3, 255]
        );
    }

    /// 验证灰度与灰度透明格式转换为稳定 RGBA 通道。
    #[test]
    fn decodes_luma_and_luma_alpha_pixels() {
        let luma = encode_png(1, 1, ExtendedColorType::L8, &[42]);
        assert_eq!(
            decode_registered_png(&luma)
                .expect("解码灰度 PNG 失败")
                .as_rgba_bytes(),
            &[42, 42, 42, 255]
        );
        let luma_alpha = encode_png(1, 1, ExtendedColorType::La8, &[42, 7]);
        assert_eq!(
            decode_registered_png(&luma_alpha)
                .expect("解码灰度透明 PNG 失败")
                .as_rgba_bytes(),
            &[42, 42, 42, 7]
        );
    }

    /// 验证空、截断、损坏和编码字节超限均被稳定拒绝。
    #[test]
    fn rejects_empty_truncated_corrupt_and_encoded_too_large() {
        assert_eq!(decode_registered_png(&[]), Err(PngDecodeError::Empty));
        assert_eq!(
            decode_registered_png(b"\x89PNG"),
            Err(PngDecodeError::DecodeFailed)
        );
        let mut corrupt = encode_png(1, 1, ExtendedColorType::Rgba8, &[1, 2, 3, 4]);
        corrupt[12..16].copy_from_slice(b"BAD!");
        assert_eq!(
            decode_registered_png(&corrupt),
            Err(PngDecodeError::DecodeFailed)
        );
        assert_eq!(
            decode_registered_png(&vec![0; MAX_PNG_ENCODED_BYTES + 1]),
            Err(PngDecodeError::EncodedTooLarge)
        );
    }

    /// 验证单维和 RGBA 总长度在像素解码前按本地稳定错误分类。
    #[test]
    fn rejects_dimension_and_rgba_limits_before_decode() {
        assert_eq!(
            decode_registered_png(&partial_png_header(MAX_IMAGE_DIMENSION + 1, 1)),
            Err(PngDecodeError::DimensionsTooLarge)
        );
        assert_eq!(
            decode_registered_png(&partial_png_header(4097, 4096)),
            Err(PngDecodeError::DecodedTooLarge)
        );
    }

    /// 验证 16 位原生解码缓冲区超过上限时不会先分配完整 DynamicImage。
    #[test]
    fn rejects_large_sixteen_bit_native_buffers_before_pixel_decode() {
        let rgba16 = encode_png(1, 1, ExtendedColorType::Rgba16, &[0; 8]);
        assert_eq!(
            decode_registered_png(&patch_ihdr_dimensions(rgba16, 4096, 4096)),
            Err(PngDecodeError::DecodedTooLarge)
        );

        let rgb16 = encode_png(1, 1, ExtendedColorType::Rgb16, &[0; 6]);
        assert_eq!(
            decode_registered_png(&patch_ihdr_dimensions(rgb16, 4096, 4096)),
            Err(PngDecodeError::DecodedTooLarge)
        );
    }
}

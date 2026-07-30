//! 此模块把受限的 Windows DIB/DIBV5 内存布局解析为规范 RGBA8 像素。
//!
//! 解析器只接受无颜色表、无 ICC profile 的 24/32 位未压缩格式，并在分配输出前完成
//! 头部、位掩码、行跨度、像素偏移和输入范围验证。

use std::fmt;

use crate::domain::{CanonicalImageError, CanonicalImagePixels};

use super::{MAX_IMAGE_DIMENSION, MAX_IMAGE_RGBA_BYTES};

/// 单份 DIB 编码内存的最大字节数：72 MiB。
pub const MAX_DIB_ENCODED_BYTES: usize = 72 * 1024 * 1024;

/// Windows 未压缩 RGB 格式编号。
const BI_RGB: u32 = 0;
/// Windows 以 DWORD 位掩码描述颜色通道的格式编号。
const BI_BITFIELDS: u32 = 3;

/// DIB 解析失败的稳定分类，不携带原始像素或外部错误文本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DibDecodeError {
    /// 输入为空或超过固定 72 MiB 上限。
    EncodedSizeInvalid,
    /// 头部或像素范围被截断。
    Truncated,
    /// 头尺寸不是本版本明确支持的已知结构。
    UnsupportedHeader,
    /// planes、位深、压缩、颜色表或 V5 profile 不在支持范围。
    UnsupportedFormat,
    /// 宽高为零、负宽或高度无法安全取绝对值。
    InvalidDimensions,
    /// 单维超过限制，或规范 RGBA8 长度溢出或超过 64 MiB。
    DecodedTooLarge,
    /// RGB(A) 位掩码为空、非连续或彼此重叠。
    InvalidMasks,
    /// 解码结果未满足规范 RGBA8 的领域不变量。
    Canonical(CanonicalImageError),
}

impl fmt::Display for DibDecodeError {
    /// 返回不包含输入字节、路径和系统错误文本的中文错误。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EncodedSizeInvalid => write!(formatter, "DIB 编码大小无效"),
            Self::Truncated => write!(formatter, "DIB 数据被截断"),
            Self::UnsupportedHeader => write!(formatter, "DIB 头部类型不受支持"),
            Self::UnsupportedFormat => write!(formatter, "DIB 像素格式不受支持"),
            Self::InvalidDimensions => write!(formatter, "DIB 图片尺寸无效"),
            Self::DecodedTooLarge => write!(formatter, "DIB 解码像素超过上限"),
            Self::InvalidMasks => write!(formatter, "DIB 颜色位掩码无效"),
            Self::Canonical(_) => write!(formatter, "DIB 解码结果不符合规范像素约束"),
        }
    }
}

impl std::error::Error for DibDecodeError {}

/// 已验证的 RGB(A) DWORD 位掩码。
#[derive(Clone, Copy, Debug)]
struct ChannelMasks {
    /// 红色通道掩码。
    red: u32,
    /// 绿色通道掩码。
    green: u32,
    /// 蓝色通道掩码。
    blue: u32,
    /// 可选 alpha 通道掩码；零表示输出不透明。
    alpha: u32,
}

/// 把 DIB/DIBV5 内存字节解析为顶向下、行连续的规范 RGBA8。
///
/// 输入必须从 bitmap information header 开始，不包含 `BITMAPFILEHEADER`。函数允许并
/// 忽略像素范围后的尾随字节，但不会信任 `biSizeImage` 来缩小范围检查。
pub fn decode_dib(bytes: &[u8]) -> Result<CanonicalImagePixels, DibDecodeError> {
    if bytes.is_empty() || bytes.len() > MAX_DIB_ENCODED_BYTES {
        return Err(DibDecodeError::EncodedSizeInvalid);
    }

    let header_size =
        usize::try_from(read_u32(bytes, 0)?).map_err(|_| DibDecodeError::UnsupportedHeader)?;
    if !matches!(header_size, 40 | 52 | 56 | 108 | 124) {
        return Err(DibDecodeError::UnsupportedHeader);
    }
    if bytes.len() < header_size {
        return Err(DibDecodeError::Truncated);
    }

    let signed_width = read_i32(bytes, 4)?;
    let signed_height = read_i32(bytes, 8)?;
    if signed_width <= 0 || signed_height == 0 || signed_height == i32::MIN {
        return Err(DibDecodeError::InvalidDimensions);
    }
    let width = u32::try_from(signed_width).map_err(|_| DibDecodeError::InvalidDimensions)?;
    let height = signed_height.unsigned_abs();
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err(DibDecodeError::DecodedTooLarge);
    }

    let planes = read_u16(bytes, 12)?;
    let bit_count = read_u16(bytes, 14)?;
    let compression = read_u32(bytes, 16)?;
    let colors_used = read_u32(bytes, 32)?;
    if planes != 1 || colors_used != 0 {
        return Err(DibDecodeError::UnsupportedFormat);
    }
    let supported = matches!(
        (bit_count, compression),
        (24, BI_RGB) | (32, BI_RGB) | (32, BI_BITFIELDS)
    );
    if !supported {
        return Err(DibDecodeError::UnsupportedFormat);
    }
    if header_size == 124 && (read_u32(bytes, 112)? != 0 || read_u32(bytes, 116)? != 0) {
        return Err(DibDecodeError::UnsupportedFormat);
    }

    let (pixel_offset, masks) = if compression == BI_BITFIELDS {
        let masks = read_and_validate_masks(bytes, header_size)?;
        let offset = if header_size == 40 { 52 } else { header_size };
        (offset, Some(masks))
    } else {
        (header_size, None)
    };

    // 所有输出和输入范围都在 Vec 分配前证明，避免损坏头部触发大分配或越界读取。
    let rgba_length = checked_rgba_length(width, height)?;
    let row_stride = checked_row_stride(width, bit_count)?;
    let required_bytes = row_stride
        .checked_mul(height as usize)
        .ok_or(DibDecodeError::DecodedTooLarge)?;
    let pixel_end = pixel_offset
        .checked_add(required_bytes)
        .ok_or(DibDecodeError::Truncated)?;
    if pixel_end > bytes.len() {
        return Err(DibDecodeError::Truncated);
    }

    let mut rgba = Vec::with_capacity(rgba_length);
    for output_y in 0..height as usize {
        let source_y = if signed_height < 0 {
            output_y
        } else {
            height as usize - 1 - output_y
        };
        let row_start = pixel_offset + source_y * row_stride;
        for x in 0..width as usize {
            if bit_count == 24 {
                let pixel = row_start + x * 3;
                rgba.extend_from_slice(&[bytes[pixel + 2], bytes[pixel + 1], bytes[pixel], 255]);
            } else if let Some(masks) = masks {
                let pixel = row_start + x * 4;
                let value = u32::from_le_bytes(
                    bytes[pixel..pixel + 4]
                        .try_into()
                        .expect("像素范围已验证为完整 DWORD"),
                );
                rgba.extend_from_slice(&[
                    scale_masked_channel(value, masks.red),
                    scale_masked_channel(value, masks.green),
                    scale_masked_channel(value, masks.blue),
                    if masks.alpha == 0 {
                        255
                    } else {
                        scale_masked_channel(value, masks.alpha)
                    },
                ]);
            } else {
                let pixel = row_start + x * 4;
                // BI_RGB 的最高字节按 Windows 保留位处理，不能把传统 DIB 误解为全透明。
                rgba.extend_from_slice(&[bytes[pixel + 2], bytes[pixel + 1], bytes[pixel], 255]);
            }
        }
    }

    CanonicalImagePixels::new(width, height, rgba).map_err(DibDecodeError::Canonical)
}

/// 读取并验证 32 位 BI_BITFIELDS 的内嵌或外置 RGB(A) 掩码。
fn read_and_validate_masks(
    bytes: &[u8],
    header_size: usize,
) -> Result<ChannelMasks, DibDecodeError> {
    let red = read_u32(bytes, 40)?;
    let green = read_u32(bytes, 44)?;
    let blue = read_u32(bytes, 48)?;
    let alpha = if header_size >= 56 {
        read_u32(bytes, 52)?
    } else {
        0
    };
    let masks = ChannelMasks {
        red,
        green,
        blue,
        alpha,
    };

    if !is_contiguous_mask(red)
        || !is_contiguous_mask(green)
        || !is_contiguous_mask(blue)
        || (alpha != 0 && !is_contiguous_mask(alpha))
    {
        return Err(DibDecodeError::InvalidMasks);
    }
    let channels = [red, green, blue, alpha];
    for left in 0..channels.len() {
        if channels[left] == 0 {
            continue;
        }
        for right in left + 1..channels.len() {
            if channels[left] & channels[right] != 0 {
                return Err(DibDecodeError::InvalidMasks);
            }
        }
    }
    Ok(masks)
}

/// 判断非零掩码在去除低位零后是否由连续的一段 1 构成。
fn is_contiguous_mask(mask: u32) -> bool {
    if mask == 0 {
        return false;
    }
    let shifted = mask >> mask.trailing_zeros();
    shifted & shifted.wrapping_add(1) == 0
}

/// 把连续掩码覆盖的整数通道四舍五入缩放到 0 至 255。
fn scale_masked_channel(value: u32, mask: u32) -> u8 {
    let shift = mask.trailing_zeros();
    let maximum = mask >> shift;
    let channel = (value & mask) >> shift;
    ((u64::from(channel) * 255 + u64::from(maximum) / 2) / u64::from(maximum)) as u8
}

/// checked 计算规范 RGBA8 输出长度并应用 64 MiB 上限。
fn checked_rgba_length(width: u32, height: u32) -> Result<usize, DibDecodeError> {
    let length = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(DibDecodeError::DecodedTooLarge)?;
    if length > MAX_IMAGE_RGBA_BYTES {
        return Err(DibDecodeError::DecodedTooLarge);
    }
    Ok(length)
}

/// checked 计算 DWORD 对齐的 DIB 行跨度。
fn checked_row_stride(width: u32, bit_count: u16) -> Result<usize, DibDecodeError> {
    let bits = u64::from(width)
        .checked_mul(u64::from(bit_count))
        .ok_or(DibDecodeError::DecodedTooLarge)?;
    let stride = bits
        .checked_add(31)
        .map(|aligned| aligned / 32 * 4)
        .ok_or(DibDecodeError::DecodedTooLarge)?;
    usize::try_from(stride).map_err(|_| DibDecodeError::DecodedTooLarge)
}

/// 从小端 DIB 字节读取 WORD。
fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, DibDecodeError> {
    let end = offset.checked_add(2).ok_or(DibDecodeError::Truncated)?;
    let encoded = bytes.get(offset..end).ok_or(DibDecodeError::Truncated)?;
    Ok(u16::from_le_bytes(
        encoded.try_into().expect("固定两字节切片"),
    ))
}

/// 从小端 DIB 字节读取 DWORD。
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, DibDecodeError> {
    let end = offset.checked_add(4).ok_or(DibDecodeError::Truncated)?;
    let encoded = bytes.get(offset..end).ok_or(DibDecodeError::Truncated)?;
    Ok(u32::from_le_bytes(
        encoded.try_into().expect("固定四字节切片"),
    ))
}

/// 从小端 DIB 字节读取有符号 LONG。
fn read_i32(bytes: &[u8], offset: usize) -> Result<i32, DibDecodeError> {
    Ok(read_u32(bytes, offset)? as i32)
}

#[cfg(test)]
mod tests {
    //! 此测试模块覆盖 DIB 颜色、方向、行填充、位掩码和分配前拒绝边界。

    use super::{
        decode_dib, DibDecodeError, BI_BITFIELDS, BI_RGB, MAX_DIB_ENCODED_BYTES,
        MAX_IMAGE_DIMENSION,
    };

    /// 构造指定已知头尺寸的基础 DIB，并填入解析所需公共字段。
    fn header(
        header_size: usize,
        width: i32,
        height: i32,
        bit_count: u16,
        compression: u32,
    ) -> Vec<u8> {
        let mut bytes = vec![0; header_size];
        write_u32(&mut bytes, 0, header_size as u32);
        write_i32(&mut bytes, 4, width);
        write_i32(&mut bytes, 8, height);
        write_u16(&mut bytes, 12, 1);
        write_u16(&mut bytes, 14, bit_count);
        write_u32(&mut bytes, 16, compression);
        bytes
    }

    /// 向测试 DIB 的指定偏移写入小端 WORD。
    fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    /// 向测试 DIB 的指定偏移写入小端 DWORD。
    fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// 向测试 DIB 的指定偏移写入小端 LONG。
    fn write_i32(bytes: &mut [u8], offset: usize, value: i32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// 24 位 bottom-up 输入必须跳过填充并翻转为顶向下输出。
    #[test]
    fn decodes_bottom_up_bgr24_with_row_padding() {
        let mut dib = header(40, 2, 2, 24, BI_RGB);
        // 文件第一行是底部：蓝、白；每行 6 个像素字节加 2 个填充字节。
        dib.extend_from_slice(&[255, 0, 0, 255, 255, 255, 9, 9]);
        // 文件第二行是顶部：红、绿。
        dib.extend_from_slice(&[0, 0, 255, 0, 255, 0, 8, 8]);

        let decoded = decode_dib(&dib).expect("解码 24 位 DIB 失败");

        assert_eq!(decoded.width(), 2);
        assert_eq!(decoded.height(), 2);
        assert_eq!(
            decoded.as_rgba_bytes(),
            &[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,]
        );
    }

    /// 32 位 top-down BI_RGB 保持行顺序并忽略保留高字节。
    #[test]
    fn decodes_top_down_bgr32_as_opaque() {
        let mut dib = header(40, 2, -1, 32, BI_RGB);
        dib.extend_from_slice(&[3, 2, 1, 0, 6, 5, 4, 7]);

        assert_eq!(
            decode_dib(&dib).unwrap().as_rgba_bytes(),
            &[1, 2, 3, 255, 4, 5, 6, 255]
        );
    }

    /// 外置十位 RGB 掩码和内嵌 alpha 掩码都必须正确缩放。
    #[test]
    fn decodes_external_and_embedded_bitfields() {
        let mut external = header(40, 1, 1, 32, BI_BITFIELDS);
        external.extend_from_slice(&0x0000_03ff_u32.to_le_bytes());
        external.extend_from_slice(&0x000f_fc00_u32.to_le_bytes());
        external.extend_from_slice(&0x3ff0_0000_u32.to_le_bytes());
        external.extend_from_slice(&0x200f_fc00_u32.to_le_bytes());
        assert_eq!(
            decode_dib(&external).unwrap().as_rgba_bytes(),
            &[0, 255, 128, 255]
        );

        let mut embedded = header(56, 1, -1, 32, BI_BITFIELDS);
        write_u32(&mut embedded, 40, 0x00ff_0000);
        write_u32(&mut embedded, 44, 0x0000_ff00);
        write_u32(&mut embedded, 48, 0x0000_00ff);
        write_u32(&mut embedded, 52, 0xff00_0000);
        embedded.extend_from_slice(&[3, 2, 1, 4]);
        assert_eq!(
            decode_dib(&embedded).unwrap().as_rgba_bytes(),
            &[1, 2, 3, 4]
        );
    }

    /// 未知头、颜色表、V5 profile 和不支持像素格式均明确拒绝。
    #[test]
    fn rejects_unsupported_headers_tables_profiles_and_formats() {
        let mut unknown = header(40, 1, 1, 24, BI_RGB);
        write_u32(&mut unknown, 0, 64);
        assert_eq!(decode_dib(&unknown), Err(DibDecodeError::UnsupportedHeader));

        let mut table = header(40, 1, 1, 24, BI_RGB);
        write_u32(&mut table, 32, 1);
        assert_eq!(decode_dib(&table), Err(DibDecodeError::UnsupportedFormat));

        let mut profile = header(124, 1, 1, 32, BI_RGB);
        write_u32(&mut profile, 112, 124);
        assert_eq!(decode_dib(&profile), Err(DibDecodeError::UnsupportedFormat));
        let mut profile_size = header(124, 1, 1, 32, BI_RGB);
        write_u32(&mut profile_size, 116, 32);
        assert_eq!(
            decode_dib(&profile_size),
            Err(DibDecodeError::UnsupportedFormat)
        );

        for (bits, compression) in [(16, BI_RGB), (24, BI_BITFIELDS), (32, 1)] {
            let invalid = header(40, 1, 1, bits, compression);
            assert_eq!(decode_dib(&invalid), Err(DibDecodeError::UnsupportedFormat));
        }
    }

    /// 无效尺寸和规范输出超限必须在像素分配前拒绝。
    #[test]
    fn rejects_invalid_and_oversized_dimensions() {
        for (width, height) in [(0, 1), (-1, 1), (1, 0), (1, i32::MIN)] {
            assert_eq!(
                decode_dib(&header(40, width, height, 24, BI_RGB)),
                Err(DibDecodeError::InvalidDimensions)
            );
        }
        assert_eq!(
            decode_dib(&header(40, (MAX_IMAGE_DIMENSION + 1) as i32, 1, 24, BI_RGB,)),
            Err(DibDecodeError::DecodedTooLarge)
        );
        assert_eq!(
            decode_dib(&header(40, 4097, 4096, 32, BI_RGB)),
            Err(DibDecodeError::DecodedTooLarge)
        );
    }

    /// 截断输入、无效 planes 和编码总长超限必须稳定失败。
    #[test]
    fn rejects_truncation_planes_and_encoded_limit() {
        assert_eq!(decode_dib(&[]), Err(DibDecodeError::EncodedSizeInvalid));
        assert_eq!(decode_dib(&[40, 0, 0, 0]), Err(DibDecodeError::Truncated));

        let mut truncated = header(40, 1, 1, 24, BI_RGB);
        truncated.extend_from_slice(&[1, 2, 3]);
        assert_eq!(decode_dib(&truncated), Err(DibDecodeError::Truncated));

        let mut planes = header(40, 1, 1, 24, BI_RGB);
        write_u16(&mut planes, 12, 2);
        assert_eq!(decode_dib(&planes), Err(DibDecodeError::UnsupportedFormat));
        assert_eq!(
            decode_dib(&vec![0; MAX_DIB_ENCODED_BYTES + 1]),
            Err(DibDecodeError::EncodedSizeInvalid)
        );
    }

    /// 空、非连续和重叠位掩码均不得进入像素读取。
    #[test]
    fn rejects_invalid_bitfield_masks() {
        let invalid_masks: [[u32; 3]; 3] = [
            [0, 0x0000_ff00, 0x0000_00ff],
            [0x00f5_0000, 0x0000_ff00, 0x0000_00ff],
            [0x00ff_0000, 0x00ff_0000, 0x0000_00ff],
        ];
        for masks in invalid_masks {
            let mut dib = header(40, 1, 1, 32, BI_BITFIELDS);
            for mask in masks {
                dib.extend_from_slice(&mask.to_le_bytes());
            }
            assert_eq!(decode_dib(&dib), Err(DibDecodeError::InvalidMasks));
        }

        let mut overlapping_alpha = header(56, 1, 1, 32, BI_BITFIELDS);
        write_u32(&mut overlapping_alpha, 40, 0x00ff_0000);
        write_u32(&mut overlapping_alpha, 44, 0x0000_ff00);
        write_u32(&mut overlapping_alpha, 48, 0x0000_00ff);
        write_u32(&mut overlapping_alpha, 52, 0x00f0_0000);
        assert_eq!(
            decode_dib(&overlapping_alpha),
            Err(DibDecodeError::InvalidMasks)
        );
    }

    /// `biSizeImage` 不参与范围证明，合法像素和尾随字节仍可解码。
    #[test]
    fn ignores_size_image_and_trailing_bytes() {
        let mut dib = header(40, 1, 1, 24, BI_RGB);
        write_u32(&mut dib, 20, u32::MAX);
        dib.extend_from_slice(&[9, 8, 7, 0, 1, 2, 3]);

        assert_eq!(decode_dib(&dib).unwrap().as_rgba_bytes(), &[7, 8, 9, 255]);
    }
}

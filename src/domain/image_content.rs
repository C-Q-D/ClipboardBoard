//! 此模块定义调用方已规范化的 RGBA8 图片像素载荷和稳定内容哈希。
//!
//! 本模块只验证尺寸与字节长度；通道转换、行方向、stride、预乘 alpha 和色彩空间
//! 规范化由 PNG、DIB 等来源解码边界负责。

use std::{fmt, num::NonZeroU32};

/// 规范图片哈希的固定域标签；版本变化时必须显式迁移图片去重语义。
pub const CANONICAL_IMAGE_HASH_DOMAIN: &[u8] = b"ClipboardBoard/canonical-image/v1\0";

/// 规范图片像素构造失败的稳定原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalImageError {
    /// 宽度或高度为零，无法表示有效图片。
    ZeroDimension,
    /// `宽 × 高 × 4` 超出当前平台可寻址长度。
    PixelLengthOverflow,
    /// RGBA 字节长度与尺寸要求不完全相等。
    PixelLengthMismatch,
}

impl fmt::Display for CanonicalImageError {
    /// 返回不包含图片字节的中文错误描述。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDimension => write!(formatter, "规范图片宽高必须为正数"),
            Self::PixelLengthOverflow => write!(formatter, "规范图片像素长度溢出"),
            Self::PixelLengthMismatch => write!(formatter, "规范图片像素长度与尺寸不匹配"),
        }
    }
}

impl std::error::Error for CanonicalImageError {}

/// 顶向下、行连续、straight RGBA8 的拥有型像素载荷。
///
/// 构造函数只能机械验证尺寸和字节长度。调用方必须先保证输入的通道顺序、行方向、
/// alpha 表示和颜色字节已经符合该语义；后续来源解码器不得把未规范化字节直接传入。
#[derive(Clone, Eq, PartialEq)]
pub struct CanonicalImagePixels {
    /// 非零图片宽度。
    width: NonZeroU32,
    /// 非零图片高度。
    height: NonZeroU32,
    /// 顶向下、行连续的 RGBA8 字节，不提供可变访问以保持构造不变量。
    pixels: Box<[u8]>,
}

impl fmt::Debug for CanonicalImagePixels {
    /// 只输出尺寸与像素长度，禁止把剪贴板图片字节带入日志或断言消息。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CanonicalImagePixels")
            .field("width", &self.width())
            .field("height", &self.height())
            .field("pixel_len", &self.pixels.len())
            .finish()
    }
}

impl CanonicalImagePixels {
    /// 验证非零尺寸、乘法边界和精确 RGBA8 长度后取得像素所有权。
    ///
    /// 调用方声明 `pixels` 已经是顶向下、行连续、straight RGBA8；本方法不会尝试
    /// 从任意字节判断或修复通道语义。
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self, CanonicalImageError> {
        let width = NonZeroU32::new(width).ok_or(CanonicalImageError::ZeroDimension)?;
        let height = NonZeroU32::new(height).ok_or(CanonicalImageError::ZeroDimension)?;
        let expected_length = usize::try_from(width.get())
            .ok()
            .and_then(|width| {
                usize::try_from(height.get())
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixel_count| pixel_count.checked_mul(4))
            .ok_or(CanonicalImageError::PixelLengthOverflow)?;
        if pixels.len() != expected_length {
            return Err(CanonicalImageError::PixelLengthMismatch);
        }

        Ok(Self {
            width,
            height,
            pixels: pixels.into_boxed_slice(),
        })
    }

    /// 返回非零图片宽度。
    pub const fn width(&self) -> u32 {
        self.width.get()
    }

    /// 返回非零图片高度。
    pub const fn height(&self) -> u32 {
        self.height.get()
    }

    /// 返回只读规范 RGBA8 字节视图。
    pub fn as_rgba_bytes(&self) -> &[u8] {
        &self.pixels
    }

    /// 按固定域标签、宽高小端字节和全部 RGBA8 字节计算 BLAKE3 内容哈希。
    pub fn content_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(CANONICAL_IMAGE_HASH_DOMAIN);
        hasher.update(&self.width().to_le_bytes());
        hasher.update(&self.height().to_le_bytes());
        hasher.update(&self.pixels);
        *hasher.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块锁定规范像素构造边界和图片哈希字节协议。

    use super::{CanonicalImageError, CanonicalImagePixels, CANONICAL_IMAGE_HASH_DOMAIN};

    /// 验证精确长度载荷保留尺寸、只读字节和稳定哈希。
    #[test]
    fn valid_pixels_preserve_read_only_contract() {
        let pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let image = CanonicalImagePixels::new(2, 1, pixels.clone()).expect("构造规范图片失败");

        assert_eq!(image.width(), 2);
        assert_eq!(image.height(), 1);
        assert_eq!(image.as_rgba_bytes(), pixels);
        assert_eq!(image.content_hash(), image.content_hash());
        assert!(!CANONICAL_IMAGE_HASH_DOMAIN.is_empty());
    }

    /// 验证 Debug 只输出结构摘要，不能泄漏完整 RGBA 字节。
    #[test]
    fn debug_output_redacts_pixel_bytes() {
        let image =
            CanonicalImagePixels::new(1, 1, vec![12, 34, 56, 78]).expect("构造 Debug 测试图片失败");

        assert_eq!(
            format!("{image:?}"),
            "CanonicalImagePixels { width: 1, height: 1, pixel_len: 4 }"
        );
    }

    /// 验证零尺寸、乘法溢出和长度差一字节均返回精确错误。
    #[test]
    fn invalid_dimensions_and_lengths_are_rejected() {
        assert_eq!(
            CanonicalImagePixels::new(0, 1, Vec::new()),
            Err(CanonicalImageError::ZeroDimension)
        );
        assert_eq!(
            CanonicalImagePixels::new(1, 0, Vec::new()),
            Err(CanonicalImageError::ZeroDimension)
        );
        assert_eq!(
            CanonicalImagePixels::new(u32::MAX, u32::MAX, Vec::new()),
            Err(CanonicalImageError::PixelLengthOverflow)
        );
        assert_eq!(
            CanonicalImagePixels::new(1, 1, vec![0; 3]),
            Err(CanonicalImageError::PixelLengthMismatch)
        );
        assert_eq!(
            CanonicalImagePixels::new(1, 1, vec![0; 5]),
            Err(CanonicalImageError::PixelLengthMismatch)
        );
    }

    /// 固定摘要锁定域标签结尾 NUL、字段顺序、宽高小端编码和 RGBA 字节。
    #[test]
    fn one_pixel_hash_matches_golden_digest() {
        let image = CanonicalImagePixels::new(1, 1, vec![0x12, 0x34, 0x56, 0x78])
            .expect("构造 golden 图片失败");

        assert_eq!(
            image.content_hash(),
            [
                32, 39, 37, 115, 229, 5, 211, 159, 155, 158, 241, 4, 251, 23, 143, 34, 248, 8, 182,
                95, 139, 72, 146, 153, 4, 167, 14, 237, 127, 8, 152, 211,
            ]
        );
    }

    /// 验证典型像素、尺寸和不同内容行顺序变化产生不同摘要。
    #[test]
    fn pixel_dimensions_and_distinct_row_order_affect_hash() {
        let original = CanonicalImagePixels::new(1, 2, vec![255, 0, 0, 255, 0, 0, 255, 255])
            .expect("构造原图片失败");
        let changed_pixel = CanonicalImagePixels::new(1, 2, vec![254, 0, 0, 255, 0, 0, 255, 255])
            .expect("构造像素变化图片失败");
        let changed_dimensions = CanonicalImagePixels::new(2, 1, original.as_rgba_bytes().to_vec())
            .expect("构造尺寸变化图片失败");
        let swapped_rows = CanonicalImagePixels::new(1, 2, vec![0, 0, 255, 255, 255, 0, 0, 255])
            .expect("构造换行图片失败");

        assert_ne!(original.content_hash(), changed_pixel.content_hash());
        assert_ne!(original.content_hash(), changed_dimensions.content_hash());
        assert_ne!(original.content_hash(), swapped_rows.content_hash());
    }
}

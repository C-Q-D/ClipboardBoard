//! 此模块负责从受限历史资产读取耐久 PNG，并编码可写入 `CF_DIBV5` 的内存载荷。
//!
//! 文件系统读取、PNG 解码和像素身份复核都在调用线程完成；本模块不访问 SQLite、
//! Windows 剪贴板或 UI，也不会在错误和 Debug 输出中泄漏图片字节或完整路径。

use std::{fmt, io::Read, path::Path};

#[cfg(windows)]
use std::fs::OpenOptions;
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
#[cfg(not(windows))]
use std::{
    fs::{self, File},
    io::Take,
};

#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
};

use crate::{
    image_decode::{
        decode_registered_png, MAX_DIB_ENCODED_BYTES, MAX_IMAGE_RGBA_BYTES, MAX_PNG_ENCODED_BYTES,
    },
    storage::HistoryImageSummary,
};

#[cfg(windows)]
use crate::image_storage::{windows_path_eq, HeldDirectory};

/// `BITMAPV5HEADER` 的固定字节数。
const BITMAP_V5_HEADER_SIZE: usize = 124;
/// `BI_BITFIELDS` 表示像素通道由头部位掩码描述。
const BI_BITFIELDS: u32 = 3;
/// `LCS_sRGB` 的 Win32 四字符颜色空间标识。
const LCS_SRGB: u32 = 0x7352_4742;
/// `LCS_GM_IMAGES` 表示图片显示用途。
const LCS_GM_IMAGES: u32 = 4;

/// 图片复制准备失败的稳定原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageCopyError {
    /// 资产根或固定子树解析后越过受管边界。
    InvalidAssetPath,
    /// 原图不存在、不是普通文件或读取失败。
    AssetUnavailable,
    /// 原图编码字节超过当前固定读取上限。
    AssetTooLarge,
    /// 磁盘文件大小与持久化元数据不一致。
    AssetSizeMismatch,
    /// 耐久文件不是当前支持的有效 PNG。
    InvalidPng,
    /// PNG 宽高或规范像素哈希与历史身份不一致。
    IdentityMismatch,
    /// DIBV5 长度、尺寸或字段转换发生溢出。
    DibEncodingOverflow,
}

impl fmt::Display for ImageCopyError {
    /// 返回不包含图片路径、文件内容或外部库文本的中文错误。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAssetPath => write!(formatter, "图片原图路径不在受管目录内"),
            Self::AssetUnavailable => write!(formatter, "图片原图不可用"),
            Self::AssetTooLarge => write!(formatter, "图片原图超过读取上限"),
            Self::AssetSizeMismatch => write!(formatter, "图片原图大小与历史记录不一致"),
            Self::InvalidPng => write!(formatter, "图片原图不是有效 PNG"),
            Self::IdentityMismatch => write!(formatter, "图片原图与历史记录身份不一致"),
            Self::DibEncodingOverflow => write!(formatter, "图片剪贴板载荷长度溢出"),
        }
    }
}

impl std::error::Error for ImageCopyError {}

/// 已复核身份、可交给 Windows writer 的拥有型 DIBV5 载荷。
#[derive(Eq, PartialEq)]
pub struct PreparedImageClipboard {
    /// 规范像素内容哈希，用于绑定自身写回预期。
    content_hash: [u8; 32],
    /// 从 `BITMAPV5HEADER` 开始、不含 `BITMAPFILEHEADER` 的完整 DIBV5 字节。
    dib_v5: Box<[u8]>,
}

impl fmt::Debug for PreparedImageClipboard {
    /// 只输出哈希存在性和载荷长度，禁止把图片字节写入诊断。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedImageClipboard")
            .field("content_hash", &"<redacted>")
            .field("dib_v5_len", &self.dib_v5.len())
            .finish()
    }
}

impl PreparedImageClipboard {
    /// 返回规范像素哈希，供剪贴板自身写回 expectation 使用。
    pub const fn content_hash(&self) -> &[u8; 32] {
        &self.content_hash
    }

    /// 返回完整 DIBV5 内存字节，调用方不得添加位图文件头。
    pub fn dib_v5_bytes(&self) -> &[u8] {
        &self.dib_v5
    }
}

/// 读取图片历史绑定的耐久 PNG，复核身份后编码为顶向下 BGRA DIBV5。
pub fn prepare_image_clipboard(
    image: &HistoryImageSummary,
) -> Result<PreparedImageClipboard, ImageCopyError> {
    let original_root = image.canonical_root.join("original");
    let asset_path = original_root.join(image.metadata.image_path().as_path());
    let encoded = read_bounded_regular_file(&image.canonical_root, &original_root, &asset_path)?;
    let recorded_size = usize::try_from(image.metadata.content_size())
        .map_err(|_| ImageCopyError::AssetSizeMismatch)?;
    if encoded.len() != recorded_size {
        return Err(ImageCopyError::AssetSizeMismatch);
    }

    let pixels = decode_registered_png(&encoded).map_err(|_| ImageCopyError::InvalidPng)?;
    if pixels.width() != image.metadata.width().get()
        || pixels.height() != image.metadata.height().get()
        || pixels.content_hash() != *image.metadata.content_hash()
    {
        return Err(ImageCopyError::IdentityMismatch);
    }

    let dib_v5 = encode_dib_v5(pixels.width(), pixels.height(), pixels.as_rgba_bytes())?;
    Ok(PreparedImageClipboard {
        content_hash: pixels.content_hash(),
        dib_v5: dib_v5.into_boxed_slice(),
    })
}

/// 解析根、固定 original 子树和最终文件，拒绝任一级符号链接或解析后的边界逃逸。
#[cfg(not(windows))]
fn validate_resolved_path(
    root: &Path,
    original_root: &Path,
    asset_path: &Path,
) -> Result<(), ImageCopyError> {
    if !root.is_absolute() {
        return Err(ImageCopyError::InvalidAssetPath);
    }
    let resolved_root = fs::canonicalize(root).map_err(|_| ImageCopyError::AssetUnavailable)?;
    let resolved_original =
        fs::canonicalize(original_root).map_err(|_| ImageCopyError::AssetUnavailable)?;
    let resolved_asset =
        fs::canonicalize(asset_path).map_err(|_| ImageCopyError::AssetUnavailable)?;
    let shard = asset_path
        .parent()
        .ok_or(ImageCopyError::InvalidAssetPath)?;
    if !resolved_original.starts_with(&resolved_root)
        || !resolved_asset.starts_with(&resolved_original)
        || !is_plain_directory(root)?
        || !is_plain_directory(original_root)?
        || !is_plain_directory(shard)?
        || fs::symlink_metadata(asset_path)
            .map_err(|_| ImageCopyError::AssetUnavailable)?
            .file_type()
            .is_symlink()
    {
        return Err(ImageCopyError::InvalidAssetPath);
    }
    Ok(())
}

/// 要求固定路径层级是非链接目录，避免受管根内通过可替换链接改写解析目标。
#[cfg(not(windows))]
fn is_plain_directory(path: &Path) -> Result<bool, ImageCopyError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| ImageCopyError::AssetUnavailable)?;
    Ok(metadata.file_type().is_dir() && !metadata.file_type().is_symlink())
}

/// 只读取普通文件，并在元数据检查后再次用 `Take` 限制竞态增长。
#[cfg(not(windows))]
fn read_bounded_regular_file(
    root: &Path,
    original_root: &Path,
    path: &Path,
) -> Result<Vec<u8>, ImageCopyError> {
    validate_resolved_path(root, original_root, path)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| ImageCopyError::AssetUnavailable)?;
    if !metadata.file_type().is_file() {
        return Err(ImageCopyError::AssetUnavailable);
    }
    if metadata.len() > MAX_PNG_ENCODED_BYTES as u64 {
        return Err(ImageCopyError::AssetTooLarge);
    }

    let file = File::open(path).map_err(|_| ImageCopyError::AssetUnavailable)?;
    let read_limit = u64::try_from(MAX_PNG_ENCODED_BYTES + 1).expect("30 MiB 上限可表示为 u64");
    let mut reader: Take<File> = file.take(read_limit);
    let mut encoded = Vec::with_capacity(metadata.len() as usize);
    reader
        .read_to_end(&mut encoded)
        .map_err(|_| ImageCopyError::AssetUnavailable)?;
    if encoded.len() > MAX_PNG_ENCODED_BYTES {
        return Err(ImageCopyError::AssetTooLarge);
    }
    Ok(encoded)
}

/// Windows 下用不共享删除的目录句柄固定整条受管路径，并从同一 no-follow 文件句柄读取。
#[cfg(windows)]
fn read_bounded_regular_file(
    root: &Path,
    original_root: &Path,
    path: &Path,
) -> Result<Vec<u8>, ImageCopyError> {
    if !root.is_absolute() {
        return Err(ImageCopyError::InvalidAssetPath);
    }
    let shard = path.parent().ok_or(ImageCopyError::InvalidAssetPath)?;
    let root_guard = HeldDirectory::open(root, "固定图片资产根")
        .map_err(|_| ImageCopyError::InvalidAssetPath)?;
    let original_guard = HeldDirectory::open(original_root, "固定原图目录")
        .map_err(|_| ImageCopyError::InvalidAssetPath)?;
    let shard_guard = HeldDirectory::open(shard, "固定原图分片目录")
        .map_err(|_| ImageCopyError::InvalidAssetPath)?;
    if original_guard
        .canonical_path
        .parent()
        .is_none_or(|parent| !windows_path_eq(parent, &root_guard.canonical_path))
        || shard_guard
            .canonical_path
            .parent()
            .is_none_or(|parent| !windows_path_eq(parent, &original_guard.canonical_path))
    {
        return Err(ImageCopyError::InvalidAssetPath);
    }

    // 目录 capability 在整个读取期间保持存活；最终文件同样禁止 DELETE 共享并拒绝
    // reparse point，因此路径成员不能在验证与读取间被重定向。
    let mut file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| ImageCopyError::AssetUnavailable)?;
    let metadata = file
        .metadata()
        .map_err(|_| ImageCopyError::AssetUnavailable)?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_file() {
        return Err(ImageCopyError::InvalidAssetPath);
    }
    if metadata.len() > MAX_PNG_ENCODED_BYTES as u64 {
        return Err(ImageCopyError::AssetTooLarge);
    }

    let read_limit = u64::try_from(MAX_PNG_ENCODED_BYTES + 1).expect("30 MiB 上限可表示为 u64");
    let mut encoded = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(read_limit)
        .read_to_end(&mut encoded)
        .map_err(|_| ImageCopyError::AssetUnavailable)?;
    if encoded.len() > MAX_PNG_ENCODED_BYTES {
        return Err(ImageCopyError::AssetTooLarge);
    }
    Ok(encoded)
}

/// 把顶向下 straight RGBA8 像素编码为 124 字节头加紧密 BGRA 像素的 DIBV5。
fn encode_dib_v5(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, ImageCopyError> {
    let pixel_len = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ImageCopyError::DibEncodingOverflow)?;
    if pixel_len == 0 || pixel_len > MAX_IMAGE_RGBA_BYTES || rgba.len() != pixel_len {
        return Err(ImageCopyError::DibEncodingOverflow);
    }
    let total_len = BITMAP_V5_HEADER_SIZE
        .checked_add(pixel_len)
        .filter(|length| *length <= MAX_DIB_ENCODED_BYTES)
        .ok_or(ImageCopyError::DibEncodingOverflow)?;
    let signed_width = i32::try_from(width).map_err(|_| ImageCopyError::DibEncodingOverflow)?;
    let signed_height = i32::try_from(height)
        .ok()
        .and_then(i32::checked_neg)
        .ok_or(ImageCopyError::DibEncodingOverflow)?;
    let image_size = u32::try_from(pixel_len).map_err(|_| ImageCopyError::DibEncodingOverflow)?;

    let mut dib = vec![0_u8; total_len];
    write_u32(&mut dib, 0, BITMAP_V5_HEADER_SIZE as u32);
    write_i32(&mut dib, 4, signed_width);
    write_i32(&mut dib, 8, signed_height);
    write_u16(&mut dib, 12, 1);
    write_u16(&mut dib, 14, 32);
    write_u32(&mut dib, 16, BI_BITFIELDS);
    write_u32(&mut dib, 20, image_size);
    write_u32(&mut dib, 40, 0x00ff_0000);
    write_u32(&mut dib, 44, 0x0000_ff00);
    write_u32(&mut dib, 48, 0x0000_00ff);
    write_u32(&mut dib, 52, 0xff00_0000);
    write_u32(&mut dib, 56, LCS_SRGB);
    write_u32(&mut dib, 108, LCS_GM_IMAGES);

    for (source, target) in rgba
        .chunks_exact(4)
        .zip(dib[BITMAP_V5_HEADER_SIZE..].chunks_exact_mut(4))
    {
        target.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
    }
    Ok(dib)
}

/// 在已经完成长度证明的缓冲区写入小端 WORD。
fn write_u16(target: &mut [u8], offset: usize, value: u16) {
    target[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

/// 在已经完成长度证明的缓冲区写入小端 DWORD。
fn write_u32(target: &mut [u8], offset: usize, value: u32) {
    target[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// 在已经完成长度证明的缓冲区写入小端 LONG。
fn write_i32(target: &mut [u8], offset: usize, value: i32) {
    target[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    //! 此测试模块覆盖原图读取边界、身份复核、透明像素和 DIBV5 往返。

    use std::{
        fs,
        io::Write,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use image::{codecs::png::PngEncoder, ExtendedColorType, ImageEncoder};

    use crate::{
        domain::{CanonicalImagePixels, ImageAssetRootId, ImageMetadata},
        image_decode::{decode_dib, MAX_PNG_ENCODED_BYTES},
        storage::HistoryImageSummary,
    };

    use super::{prepare_image_clipboard, ImageCopyError};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    /// 创建当前测试独占的临时资产根。
    fn temporary_root() -> PathBuf {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "clipboard-board-image-copy-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("创建图片复制测试根失败");
        root
    }

    /// 编码一份 RGBA PNG，供文件身份和 DIBV5 往返测试使用。
    fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::new();
        PngEncoder::new(&mut encoded)
            .write_image(rgba, width, height, ExtendedColorType::Rgba8)
            .expect("编码图片复制测试 PNG 失败");
        encoded
    }

    /// 创建受限图片摘要并按真实布局写入原图。
    fn write_asset(root: &Path, width: u32, height: u32, rgba: &[u8]) -> HistoryImageSummary {
        let pixels =
            CanonicalImagePixels::new(width, height, rgba.to_vec()).expect("构造测试像素失败");
        let hash = pixels.content_hash();
        let hash_hex = hash
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let encoded = encode_png(width, height, rgba);
        let relative = format!("{}/{hash_hex}.png", &hash_hex[..2]);
        let thumbnail = format!("{}/{hash_hex}.webp", &hash_hex[..2]);
        let absolute = root.join("original").join(&relative);
        fs::create_dir_all(absolute.parent().expect("测试原图必须存在父目录"))
            .expect("创建测试原图分片目录失败");
        fs::write(&absolute, &encoded).expect("写入测试原图失败");

        HistoryImageSummary {
            metadata: ImageMetadata::new(
                hash,
                ImageAssetRootId::new([9; 32]),
                relative,
                thumbnail,
                width,
                height,
                encoded.len() as u64,
            )
            .expect("构造测试图片元数据失败"),
            canonical_root: fs::canonicalize(root).expect("规范化测试根失败"),
        }
    }

    /// 多行透明像素经 PNG 读取和 DIBV5 编码后必须逐字节保持规范 RGBA 身份。
    #[test]
    fn valid_png_roundtrips_through_dib_v5() {
        let root = temporary_root();
        let rgba = [255, 0, 0, 0, 0, 255, 0, 64, 0, 0, 255, 128, 12, 34, 56, 255];
        let image = write_asset(&root, 2, 2, &rgba);

        let prepared = prepare_image_clipboard(&image).expect("准备 DIBV5 失败");
        let decoded = decode_dib(prepared.dib_v5_bytes()).expect("回读 DIBV5 失败");
        assert_eq!(decoded.width(), 2);
        assert_eq!(decoded.height(), 2);
        assert_eq!(decoded.as_rgba_bytes(), rgba);
        assert_eq!(prepared.content_hash(), image.metadata.content_hash());
        assert_eq!(
            i32::from_le_bytes(prepared.dib_v5_bytes()[8..12].try_into().unwrap()),
            -2
        );
        fs::remove_dir_all(root).expect("清理 DIBV5 往返测试根失败");
    }

    /// 单像素图片同样必须生成无文件头、从 124 字节 V5 头开始的载荷。
    #[test]
    fn one_pixel_payload_starts_with_bitmap_v5_header() {
        let root = temporary_root();
        let image = write_asset(&root, 1, 1, &[1, 2, 3, 4]);
        let prepared = prepare_image_clipboard(&image).expect("准备单像素 DIBV5 失败");

        assert_eq!(prepared.dib_v5_bytes().len(), 128);
        assert_eq!(
            u32::from_le_bytes(prepared.dib_v5_bytes()[0..4].try_into().unwrap()),
            124
        );
        assert_eq!(&prepared.dib_v5_bytes()[124..], &[3, 2, 1, 4]);
        assert!(!format!("{prepared:?}").contains("[1, 2, 3, 4]"));
        fs::remove_dir_all(root).expect("清理单像素测试根失败");
    }

    /// 文件缺失和目录冒充原图都必须稳定拒绝。
    #[test]
    fn missing_or_non_regular_asset_is_rejected() {
        let root = temporary_root();
        let image = write_asset(&root, 1, 1, &[1, 2, 3, 255]);
        let path = root
            .join("original")
            .join(image.metadata.image_path().as_path());
        fs::remove_file(&path).expect("删除测试原图失败");
        assert_eq!(
            prepare_image_clipboard(&image),
            Err(ImageCopyError::AssetUnavailable)
        );
        fs::create_dir(&path).expect("创建冒充原图目录失败");
        assert_eq!(
            prepare_image_clipboard(&image),
            Err(ImageCopyError::AssetUnavailable)
        );
        fs::remove_dir_all(root).expect("清理不可用文件测试根失败");
    }

    /// 公开 DTO 被错误构造为相对根时必须在接触任一文件句柄前拒绝。
    #[test]
    fn relative_asset_root_is_rejected() {
        let root = temporary_root();
        let mut image = write_asset(&root, 1, 1, &[1, 2, 3, 255]);
        image.canonical_root = PathBuf::from("relative-root");

        assert_eq!(
            prepare_image_clipboard(&image),
            Err(ImageCopyError::InvalidAssetPath)
        );
        fs::remove_dir_all(root).expect("清理相对根测试目录失败");
    }

    /// 稀疏文件超过 PNG 上限时必须在读取前拒绝。
    #[test]
    fn oversized_asset_is_rejected_before_decode() {
        let root = temporary_root();
        let image = write_asset(&root, 1, 1, &[1, 2, 3, 255]);
        let path = root
            .join("original")
            .join(image.metadata.image_path().as_path());
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("打开超限测试原图失败")
            .set_len(MAX_PNG_ENCODED_BYTES as u64 + 1)
            .expect("扩展超限测试原图失败");

        assert_eq!(
            prepare_image_clipboard(&image),
            Err(ImageCopyError::AssetTooLarge)
        );
        fs::remove_dir_all(root).expect("清理超限测试根失败");
    }

    /// 文件字节数变化必须在解码前按持久化大小身份拒绝。
    #[test]
    fn recorded_size_mismatch_is_rejected() {
        let root = temporary_root();
        let image = write_asset(&root, 1, 1, &[1, 2, 3, 255]);
        let path = root
            .join("original")
            .join(image.metadata.image_path().as_path());
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("打开大小错配原图失败")
            .write_all(b"x")
            .expect("追加大小错配字节失败");

        assert_eq!(
            prepare_image_clipboard(&image),
            Err(ImageCopyError::AssetSizeMismatch)
        );
        fs::remove_dir_all(root).expect("清理大小错配测试根失败");
    }

    /// 相同大小的损坏编码必须被 PNG 解码边界拒绝。
    #[test]
    fn corrupt_png_is_rejected_without_external_error_text() {
        let root = temporary_root();
        let image = write_asset(&root, 1, 1, &[1, 2, 3, 255]);
        let path = root
            .join("original")
            .join(image.metadata.image_path().as_path());
        let length = fs::metadata(&path).expect("读取原图长度失败").len() as usize;
        fs::write(path, vec![0; length]).expect("写入损坏 PNG 失败");

        assert_eq!(
            prepare_image_clipboard(&image),
            Err(ImageCopyError::InvalidPng)
        );
        assert!(!ImageCopyError::InvalidPng.to_string().contains('\\'));
        fs::remove_dir_all(root).expect("清理损坏 PNG 测试根失败");
    }

    /// 替换成同尺寸但不同像素的有效 PNG 必须由规范哈希复核拒绝。
    #[test]
    fn canonical_hash_mismatch_is_rejected() {
        let root = temporary_root();
        let mut image = write_asset(&root, 1, 1, &[1, 2, 3, 255]);
        let path = root
            .join("original")
            .join(image.metadata.image_path().as_path());
        let replacement = encode_png(1, 1, &[4, 5, 6, 255]);
        fs::write(&path, &replacement).expect("替换哈希错配 PNG 失败");
        // 测试编码器对两个单像素 PNG 通常生成相同长度；若依赖版本变化，则同步更新元数据
        // 大小，仅让本测试聚焦规范像素哈希边界。
        if replacement.len() as i64 != image.metadata.content_size() {
            let hash = *image.metadata.content_hash();
            let image_path = image.metadata.image_path().as_path().to_owned();
            let thumbnail_path = image.metadata.thumbnail_path().as_path().to_owned();
            image.metadata = ImageMetadata::new(
                hash,
                image.metadata.root_id(),
                image_path,
                thumbnail_path,
                1,
                1,
                replacement.len() as u64,
            )
            .expect("重建哈希错配测试元数据失败");
        }

        assert_eq!(
            prepare_image_clipboard(&image),
            Err(ImageCopyError::IdentityMismatch)
        );
        fs::remove_dir_all(root).expect("清理哈希错配测试根失败");
    }
}

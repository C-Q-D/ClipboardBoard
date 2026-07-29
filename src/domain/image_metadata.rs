//! 此模块定义图片持久化元数据、稳定资产根身份和根内相对路径校验。
//!
//! 领域对象只接受与内容哈希一致的固定两组件路径，不依赖 SQLite、文件系统或图片解码。

use std::{
    fmt,
    num::NonZeroU32,
    path::{Component, Path, PathBuf},
};

/// 图片资产根的稳定 32 字节身份；目录移动后该身份保持不变。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ImageAssetRootId([u8; 32]);

impl ImageAssetRootId {
    /// 从已经验证或持久化的 32 字节值构造根身份。
    pub const fn new(value: [u8; 32]) -> Self {
        Self(value)
    }

    /// 返回根身份的只读字节，供后续 SQLite 和 marker 编码使用。
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// 固定图片子树内的两组件相对路径，例如 `ab/<64hex>.png`。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ImageAssetRelativePath(PathBuf);

impl ImageAssetRelativePath {
    /// 返回经过校验的根内相对路径；调用方仍须由目录布局添加固定子树。
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// 原图编码格式；当前持久化契约只允许 PNG。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageOriginalFormat {
    /// 耐久原图使用 PNG 编码。
    Png,
}

impl ImageOriginalFormat {
    /// 返回 SQLite 使用的稳定小写格式名。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
        }
    }
}

/// 图片元数据构造失败的稳定原因；错误不包含图片字节。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageMetadataError {
    /// 路径不是恰好两个普通组件，或包含绝对根、盘符、`.`、`..`。
    InvalidPathShape,
    /// 路径含非 UTF-8、反斜杠或其他不属于持久化格式的字符。
    InvalidPathEncoding,
    /// 分片、文件名、大小写或扩展名与预期内容哈希不一致。
    PathHashMismatch,
    /// 宽度或高度为零。
    ZeroDimension,
    /// 原图字节数为零或超过 SQLite `INTEGER` 正数范围。
    InvalidContentSize,
}

impl fmt::Display for ImageMetadataError {
    /// 返回不泄漏本地路径和图片内容的中文错误描述。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPathShape => write!(formatter, "图片资产路径结构无效"),
            Self::InvalidPathEncoding => write!(formatter, "图片资产路径编码无效"),
            Self::PathHashMismatch => write!(formatter, "图片资产路径与内容哈希不一致"),
            Self::ZeroDimension => write!(formatter, "图片宽高必须为正数"),
            Self::InvalidContentSize => write!(formatter, "图片原图字节数超出持久化范围"),
        }
    }
}

impl std::error::Error for ImageMetadataError {}

/// 可写入 v2 图片行的完整领域元数据；所有字段在构造时一次性建立不变量。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageMetadata {
    /// 规范图片像素内容哈希；同时决定两个相对资产路径。
    content_hash: [u8; 32],
    /// 当前图片文件所属的稳定资产根。
    root_id: ImageAssetRootId,
    /// `original` 固定子树内的 PNG 相对路径。
    image_path: ImageAssetRelativePath,
    /// `thumbnail` 固定子树内的 WebP 相对路径。
    thumbnail_path: ImageAssetRelativePath,
    /// 非零图片宽度。
    width: NonZeroU32,
    /// 非零图片高度。
    height: NonZeroU32,
    /// 当前固定的耐久原图格式。
    format: ImageOriginalFormat,
    /// 已发布原图 PNG 的实际文件字节数。
    content_size: i64,
}

impl ImageMetadata {
    /// 校验路径、尺寸和大小后构造完整图片元数据。
    ///
    /// `image_path` 与 `thumbnail_path` 必须分别是
    /// `<哈希前两位>/<完整小写哈希>.png|webp`，且与 `content_hash` 完全一致。
    pub fn new(
        content_hash: [u8; 32],
        root_id: ImageAssetRootId,
        image_path: impl Into<PathBuf>,
        thumbnail_path: impl Into<PathBuf>,
        width: u32,
        height: u32,
        content_size: u64,
    ) -> Result<Self, ImageMetadataError> {
        let width = NonZeroU32::new(width).ok_or(ImageMetadataError::ZeroDimension)?;
        let height = NonZeroU32::new(height).ok_or(ImageMetadataError::ZeroDimension)?;
        let content_size =
            i64::try_from(content_size).map_err(|_| ImageMetadataError::InvalidContentSize)?;
        if content_size == 0 {
            return Err(ImageMetadataError::InvalidContentSize);
        }

        let hash_hex = content_hash_hex(&content_hash);
        let image_path = validate_relative_path(image_path.into(), &hash_hex, "png")?;
        let thumbnail_path =
            validate_relative_path(thumbnail_path.into(), &hash_hex, "webp")?;

        Ok(Self {
            content_hash,
            root_id,
            image_path,
            thumbnail_path,
            width,
            height,
            format: ImageOriginalFormat::Png,
            content_size,
        })
    }

    /// 返回规范内容哈希。
    pub const fn content_hash(&self) -> &[u8; 32] {
        &self.content_hash
    }

    /// 返回稳定资产根身份。
    pub const fn root_id(&self) -> ImageAssetRootId {
        self.root_id
    }

    /// 返回原图固定子树内的相对路径。
    pub fn image_path(&self) -> &ImageAssetRelativePath {
        &self.image_path
    }

    /// 返回缩略图固定子树内的相对路径。
    pub fn thumbnail_path(&self) -> &ImageAssetRelativePath {
        &self.thumbnail_path
    }

    /// 返回非零图片宽度。
    pub const fn width(&self) -> NonZeroU32 {
        self.width
    }

    /// 返回非零图片高度。
    pub const fn height(&self) -> NonZeroU32 {
        self.height
    }

    /// 返回固定原图格式。
    pub const fn format(&self) -> ImageOriginalFormat {
        self.format
    }

    /// 返回可直接写入 SQLite INTEGER 的原图字节数。
    pub const fn content_size(&self) -> i64 {
        self.content_size
    }
}

/// 把固定 32 字节哈希编码为小写 64 位十六进制，不受区域设置影响。
pub(crate) fn content_hash_hex(content_hash: &[u8; 32]) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(64);
    for byte in content_hash {
        // 写入 String 不会失败；显式忽略 fmt::Result 可避免引入不可能出现的业务错误。
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

/// 验证固定子树内的路径形状和哈希绑定，拒绝任何平台路径逃逸组件。
fn validate_relative_path(
    path: PathBuf,
    hash_hex: &str,
    extension: &str,
) -> Result<ImageAssetRelativePath, ImageMetadataError> {
    let portable = path
        .to_str()
        .ok_or(ImageMetadataError::InvalidPathEncoding)?;
    // 数据库契约固定使用正斜杠和两个非空组件；先检查原始文本，避免 `Path::components`
    // 折叠 `.` 或重复分隔符后把非规范输入误判为合法。
    let portable_components = portable.split('/').collect::<Vec<_>>();
    if portable.contains('\\')
        || portable.contains(':')
        || portable_components.len() != 2
        || portable_components
            .iter()
            .any(|component| component.is_empty() || *component == "." || *component == "..")
    {
        return Err(ImageMetadataError::InvalidPathShape);
    }

    let components = path.components().collect::<Vec<_>>();
    let [Component::Normal(shard), Component::Normal(file_name)] = components.as_slice() else {
        return Err(ImageMetadataError::InvalidPathShape);
    };
    let shard = shard
        .to_str()
        .ok_or(ImageMetadataError::InvalidPathEncoding)?;
    let file_name = file_name
        .to_str()
        .ok_or(ImageMetadataError::InvalidPathEncoding)?;
    let expected_file_name = format!("{hash_hex}.{extension}");
    if shard != &hash_hex[..2] || file_name != expected_file_name {
        return Err(ImageMetadataError::PathHashMismatch);
    }
    Ok(ImageAssetRelativePath(path))
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证图片元数据路径身份、Windows 逃逸形式和数值边界。

    use std::path::PathBuf;

    use super::{
        content_hash_hex, ImageAssetRootId, ImageMetadata, ImageMetadataError,
        ImageOriginalFormat,
    };

    /// 为指定字节生成合法原图和缩略图相对路径。
    fn valid_paths(value: u8) -> (String, String) {
        let hex = content_hash_hex(&[value; 32]);
        (
            format!("{}/{hex}.png", &hex[..2]),
            format!("{}/{hex}.webp", &hex[..2]),
        )
    }

    /// 验证合法元数据保留稳定身份、非零尺寸和固定格式。
    #[test]
    fn valid_metadata_preserves_contract_fields() {
        let (image_path, thumbnail_path) = valid_paths(0xab);
        let metadata = ImageMetadata::new(
            [0xab; 32],
            ImageAssetRootId::new([0xcd; 32]),
            image_path,
            thumbnail_path,
            1920,
            1080,
            4096,
        )
        .expect("构造合法图片元数据失败");

        assert_eq!(metadata.content_hash(), &[0xab; 32]);
        assert_eq!(metadata.root_id().as_bytes(), &[0xcd; 32]);
        assert_eq!(metadata.width().get(), 1920);
        assert_eq!(metadata.height().get(), 1080);
        assert_eq!(metadata.format(), ImageOriginalFormat::Png);
        assert_eq!(metadata.format().as_str(), "png");
        assert_eq!(metadata.content_size(), 4096);
    }

    /// 验证两个资产路径都必须绑定同一内容哈希，大小写也不得放宽。
    #[test]
    fn mismatched_or_uppercase_hash_path_is_rejected() {
        let (image_path, thumbnail_path) = valid_paths(0x11);
        assert_eq!(
            ImageMetadata::new(
                [0x22; 32],
                ImageAssetRootId::new([1; 32]),
                image_path,
                thumbnail_path,
                1,
                1,
                1,
            ),
            Err(ImageMetadataError::PathHashMismatch)
        );

        let (image_path, thumbnail_path) = valid_paths(0xaa);
        assert_eq!(
            ImageMetadata::new(
                [0xaa; 32],
                ImageAssetRootId::new([1; 32]),
                image_path.to_ascii_uppercase(),
                thumbnail_path,
                1,
                1,
                1,
            ),
            Err(ImageMetadataError::PathHashMismatch)
        );
    }

    /// 验证绝对、父级、当前目录、额外组件和反斜杠路径均不能进入领域对象。
    #[test]
    fn escaping_or_noncanonical_paths_are_rejected() {
        let (image_path, thumbnail_path) = valid_paths(0x33);
        for invalid in [
            PathBuf::from(format!("../{image_path}")),
            PathBuf::from(format!("./{image_path}")),
            PathBuf::from(format!("extra/{image_path}")),
            PathBuf::from(format!("C:/{image_path}")),
            PathBuf::from(format!(r"33\{}", &image_path[3..])),
        ] {
            assert!(
                ImageMetadata::new(
                    [0x33; 32],
                    ImageAssetRootId::new([2; 32]),
                    invalid,
                    thumbnail_path.clone(),
                    1,
                    1,
                    1,
                )
                .is_err()
            );
        }
    }

    /// 验证零尺寸、零大小和超过 SQLite 正数范围的大小均被拒绝。
    #[test]
    fn numeric_boundaries_are_enforced() {
        let (image_path, thumbnail_path) = valid_paths(0x44);
        for (width, height, size, expected) in [
            (0, 1, 1, ImageMetadataError::ZeroDimension),
            (1, 0, 1, ImageMetadataError::ZeroDimension),
            (1, 1, 0, ImageMetadataError::InvalidContentSize),
            (
                1,
                1,
                i64::MAX as u64 + 1,
                ImageMetadataError::InvalidContentSize,
            ),
        ] {
            assert_eq!(
                ImageMetadata::new(
                    [0x44; 32],
                    ImageAssetRootId::new([3; 32]),
                    image_path.clone(),
                    thumbnail_path.clone(),
                    width,
                    height,
                    size,
                ),
                Err(expected)
            );
        }
    }
}

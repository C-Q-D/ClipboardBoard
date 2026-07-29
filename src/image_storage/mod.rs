//! 此模块定义图片资产根、固定子目录和哈希分片路径的纯布局契约。
//!
//! 当前模块只构造和校验路径，不访问文件系统；目录认领与 Windows 安全检查由后续实现。

use std::{
    ffi::OsString,
    fmt,
    path::{Component, Path, PathBuf},
};

use crate::domain::{image_metadata::content_hash_hex, ImageAssetRootId};

mod prepare;
#[cfg(windows)]
mod windows_guard;

pub use prepare::{
    prepare_image_storage, ImageStorageFallback, ImageStoragePrepareError,
    ImageStoragePrepareErrorKind, PreparedImageStorage,
};

/// 自定义图片根外部的受管恢复基目录名称。
pub const CUSTOM_RECOVERY_DIRECTORY_NAME: &str = ".clipboardboard-recovery";

/// 用户选择的图片资产根偏好；设置持久化将在后续原子接入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImageStoragePreference {
    /// 使用 `%LOCALAPPDATA%\ClipboardBoard\images`。
    Default,
    /// 使用用户指定的绝对专用子目录。
    Custom(PathBuf),
}

/// 当前布局使用的根类型，与 SQLite `image_asset_roots.root_kind` 对齐。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageStorageRootKind {
    /// LOCALAPPDATA 下的应用默认根。
    Default,
    /// 用户选择的专用绝对目录。
    Custom,
}

impl ImageStorageRootKind {
    /// 返回 SQLite 使用的稳定小写根类型。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Custom => "custom",
        }
    }
}

/// 图片路径布局构造失败的稳定原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageStoragePathError {
    /// 默认路径缺少 LOCALAPPDATA。
    MissingLocalAppData,
    /// 自定义路径不是绝对路径。
    CustomRootMustBeAbsolute,
    /// 自定义路径是文件系统根、盘符根、UNC share 根或包含非规范组件。
    CustomRootMustBeDedicatedDirectory,
}

impl fmt::Display for ImageStoragePathError {
    /// 返回不包含用户完整路径的中文错误描述。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingLocalAppData => write!(formatter, "缺少 LOCALAPPDATA 环境变量"),
            Self::CustomRootMustBeAbsolute => write!(formatter, "自定义图片目录必须是绝对路径"),
            Self::CustomRootMustBeDedicatedDirectory => {
                write!(formatter, "自定义图片目录必须是专用子目录")
            }
        }
    }
}

impl std::error::Error for ImageStoragePathError {}

/// 同一内容哈希对应的原图与缩略图相对/绝对路径集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageAssetPaths {
    /// `original` 子树内的两组件相对路径。
    pub image_relative: PathBuf,
    /// `thumbnail` 子树内的两组件相对路径。
    pub thumbnail_relative: PathBuf,
    /// 原图的完整预期路径；目录可能尚未创建。
    pub image_absolute: PathBuf,
    /// 缩略图的完整预期路径；目录可能尚未创建。
    pub thumbnail_absolute: PathBuf,
}

/// 一个图片资产根的完整纯路径布局。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageStorageLayout {
    /// 当前根类型。
    root_kind: ImageStorageRootKind,
    /// 图片资产根；包含 original、thumbnail 和 staging。
    asset_root: PathBuf,
    /// 耐久 PNG 原图固定子目录。
    original_directory: PathBuf,
    /// 可重建 WebP 缩略图固定子目录。
    thumbnail_directory: PathBuf,
    /// 图片发布前的临时写入目录。
    staging_directory: PathBuf,
    /// 位于资产根之外的同卷恢复基目录。
    recovery_base_directory: PathBuf,
}

impl ImageStorageLayout {
    /// 根据默认或自定义偏好构造布局，不创建任何目录。
    pub fn from_preference(
        preference: ImageStoragePreference,
    ) -> Result<Self, ImageStoragePathError> {
        layout_from_local_app_data(std::env::var_os("LOCALAPPDATA"), preference)
    }

    /// 使用显式 LOCALAPPDATA 构造布局，供目录准备流程和隔离测试复用。
    pub(crate) fn from_preference_with_local_app_data(
        local_app_data: Option<OsString>,
        preference: ImageStoragePreference,
    ) -> Result<Self, ImageStoragePathError> {
        layout_from_local_app_data(local_app_data, preference)
    }

    /// 返回根类型。
    pub const fn root_kind(&self) -> ImageStorageRootKind {
        self.root_kind
    }

    /// 返回图片资产根。
    pub fn asset_root(&self) -> &Path {
        &self.asset_root
    }

    /// 返回原图固定子目录。
    pub fn original_directory(&self) -> &Path {
        &self.original_directory
    }

    /// 返回缩略图固定子目录。
    pub fn thumbnail_directory(&self) -> &Path {
        &self.thumbnail_directory
    }

    /// 返回 staging 固定子目录。
    pub fn staging_directory(&self) -> &Path {
        &self.staging_directory
    }

    /// 返回资产根外部的恢复基目录。
    pub fn recovery_base_directory(&self) -> &Path {
        &self.recovery_base_directory
    }

    /// 为指定根身份生成独立恢复隔离目录；当前方法只构造路径。
    pub fn recovery_directory(&self, root_id: ImageAssetRootId) -> PathBuf {
        self.recovery_base_directory
            .join(content_hash_hex(root_id.as_bytes()))
    }

    /// 从内容哈希生成固定分片下的原图与缩略图路径。
    pub fn asset_paths(&self, content_hash: &[u8; 32]) -> ImageAssetPaths {
        let hex = content_hash_hex(content_hash);
        let shard = &hex[..2];
        let image_file_name = format!("{hex}.png");
        let thumbnail_file_name = format!("{hex}.webp");
        // 持久化相对路径固定使用 `/`，不能让 Windows PathBuf::join 产生反斜杠。
        let image_relative = PathBuf::from(format!("{shard}/{image_file_name}"));
        let thumbnail_relative = PathBuf::from(format!("{shard}/{thumbnail_file_name}"));

        ImageAssetPaths {
            image_absolute: self
                .original_directory
                .join(shard)
                .join(image_file_name),
            thumbnail_absolute: self
                .thumbnail_directory
                .join(shard)
                .join(thumbnail_file_name),
            image_relative,
            thumbnail_relative,
        }
    }
}

/// 用可注入环境值构造布局，避免测试并发修改进程环境变量。
fn layout_from_local_app_data(
    local_app_data: Option<OsString>,
    preference: ImageStoragePreference,
) -> Result<ImageStorageLayout, ImageStoragePathError> {
    let (root_kind, asset_root, recovery_base_directory) = match preference {
        ImageStoragePreference::Default => {
            let local_app_data = local_app_data
                .filter(|value| !value.is_empty())
                .ok_or(ImageStoragePathError::MissingLocalAppData)?;
            let application_root = PathBuf::from(local_app_data).join("ClipboardBoard");
            (
                ImageStorageRootKind::Default,
                application_root.join("images"),
                application_root.join("recovery"),
            )
        }
        ImageStoragePreference::Custom(asset_root) => {
            validate_custom_root(&asset_root)?;
            let parent = asset_root
                .parent()
                .ok_or(ImageStoragePathError::CustomRootMustBeDedicatedDirectory)?;
            let recovery_base_directory = parent.join(CUSTOM_RECOVERY_DIRECTORY_NAME);
            (
                ImageStorageRootKind::Custom,
                asset_root,
                recovery_base_directory,
            )
        }
    };

    Ok(ImageStorageLayout {
        original_directory: asset_root.join("original"),
        thumbnail_directory: asset_root.join("thumbnail"),
        staging_directory: asset_root.join("staging"),
        recovery_base_directory,
        asset_root,
        root_kind,
    })
}

/// 自定义根必须是规范绝对子目录，不能把整个卷或 share 交给未来清理逻辑。
fn validate_custom_root(path: &Path) -> Result<(), ImageStoragePathError> {
    if !path.is_absolute() {
        return Err(ImageStoragePathError::CustomRootMustBeAbsolute);
    }
    let raw_path = path.to_string_lossy();
    let has_noncanonical_component = raw_path
        .split(['\\', '/'])
        .any(|component| component == "." || component == "..");
    let uses_reserved_recovery_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(CUSTOM_RECOVERY_DIRECTORY_NAME));
    if path.file_name().is_none()
        || has_noncanonical_component
        || uses_reserved_recovery_name
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(ImageStoragePathError::CustomRootMustBeDedicatedDirectory);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证默认/自定义根、外部恢复目录和稳定哈希分片。

    use std::{ffi::OsString, path::PathBuf};

    use crate::domain::{ImageAssetRootId, ImageMetadata};

    use super::{
        layout_from_local_app_data, ImageStoragePathError, ImageStoragePreference,
        ImageStorageRootKind, CUSTOM_RECOVERY_DIRECTORY_NAME,
    };

    /// 验证默认布局严格位于 LOCALAPPDATA 的应用目录。
    #[test]
    fn default_layout_uses_local_app_data() {
        let base = PathBuf::from(r"C:\Users\Tester\AppData\Local");
        let layout = layout_from_local_app_data(
            Some(OsString::from(&base)),
            ImageStoragePreference::Default,
        )
        .expect("构造默认布局失败");

        assert_eq!(layout.root_kind(), ImageStorageRootKind::Default);
        assert_eq!(
            layout.asset_root(),
            base.join("ClipboardBoard").join("images")
        );
        assert_eq!(
            layout.recovery_base_directory(),
            base.join("ClipboardBoard").join("recovery")
        );
        assert!(!layout
            .recovery_base_directory()
            .starts_with(layout.asset_root()));
    }

    /// 验证自定义恢复基目录位于资产根同父目录，而不是资产根内部。
    #[test]
    fn custom_layout_uses_external_sibling_recovery() {
        let root = PathBuf::from(r"D:\ClipboardAssets");
        let layout = layout_from_local_app_data(
            None,
            ImageStoragePreference::Custom(root.clone()),
        )
        .expect("构造自定义布局失败");

        assert_eq!(layout.root_kind().as_str(), "custom");
        assert_eq!(layout.asset_root(), root);
        assert_eq!(
            layout.recovery_base_directory(),
            PathBuf::from(r"D:\").join(CUSTOM_RECOVERY_DIRECTORY_NAME)
        );
        assert!(!layout
            .recovery_base_directory()
            .starts_with(layout.asset_root()));
    }

    /// 验证哈希稳定生成小写前缀分片、PNG 原图和 WebP 缩略图。
    #[test]
    fn content_hash_generates_stable_asset_paths() {
        let layout = layout_from_local_app_data(
            None,
            ImageStoragePreference::Custom(PathBuf::from(r"D:\ClipboardAssets")),
        )
        .expect("构造分片布局失败");
        let paths = layout.asset_paths(&[0xab; 32]);
        let hex = "ab".repeat(32);

        assert_eq!(
            paths.image_relative,
            PathBuf::from(format!("ab/{hex}.png"))
        );
        assert_eq!(
            paths.thumbnail_relative,
            PathBuf::from(format!("ab/{hex}.webp"))
        );
        assert_eq!(
            paths.image_relative.to_str().expect("原图相对路径非 UTF-8"),
            format!("ab/{hex}.png")
        );
        assert_eq!(
            paths
                .thumbnail_relative
                .to_str()
                .expect("缩略图相对路径非 UTF-8"),
            format!("ab/{hex}.webp")
        );
        assert_eq!(
            paths.image_absolute,
            layout.original_directory().join(&paths.image_relative)
        );
        assert_eq!(
            paths.thumbnail_absolute,
            layout
                .thumbnail_directory()
                .join(&paths.thumbnail_relative)
        );
        assert_eq!(
            layout.recovery_directory(ImageAssetRootId::new([0xcd; 32])),
            layout.recovery_base_directory().join("cd".repeat(32))
        );

        ImageMetadata::new(
            [0xab; 32],
            ImageAssetRootId::new([0xcd; 32]),
            paths.image_relative,
            paths.thumbnail_relative,
            10,
            20,
            30,
        )
        .expect("布局生成的相对路径必须能进入领域元数据");
    }

    /// 验证缺失默认环境、相对路径和盘符根均被拒绝。
    #[test]
    fn invalid_roots_are_rejected() {
        assert_eq!(
            layout_from_local_app_data(None, ImageStoragePreference::Default),
            Err(ImageStoragePathError::MissingLocalAppData)
        );
        assert_eq!(
            layout_from_local_app_data(
                None,
                ImageStoragePreference::Custom(PathBuf::from("relative"))
            ),
            Err(ImageStoragePathError::CustomRootMustBeAbsolute)
        );
        assert_eq!(
            layout_from_local_app_data(
                None,
                ImageStoragePreference::Custom(PathBuf::from(r"D:\"))
            ),
            Err(ImageStoragePathError::CustomRootMustBeDedicatedDirectory)
        );
        for invalid in [
            PathBuf::from(r"D:\Work\.clipboardboard-recovery"),
            PathBuf::from(r"D:\Work\.CLIPBOARDBOARD-RECOVERY"),
            PathBuf::from(r"D:\Assets\.\images"),
            PathBuf::from(r"D:\Assets\..\images"),
        ] {
            assert_eq!(
                layout_from_local_app_data(
                    None,
                    ImageStoragePreference::Custom(invalid)
                ),
                Err(ImageStoragePathError::CustomRootMustBeDedicatedDirectory)
            );
        }
    }
}

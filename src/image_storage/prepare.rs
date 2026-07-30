//! 此模块负责创建图片存储目录、识别受管目录并在自定义路径失败时回退默认目录。
//!
//! 当前阶段只建立后续图片写入所需的最小目录能力，不包含图片编码、设置持久化和目录迁移。

use std::{
    ffi::OsString,
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use crate::domain::{image_metadata::content_hash_hex, ImageAssetRootId};

use super::{
    windows_guard::{PublishShardGuard, WindowsStorageGuard},
    ImageAssetPaths, ImageStorageLayout, ImageStoragePathError, ImageStoragePreference,
};

/// 资产根身份文件；用于避免把含用户文件的目录误当成应用缓存目录。
const OWNER_FILE_NAME: &str = ".clipboardboard.owner";
/// 根身份文件协议版本。
const OWNER_VERSION: &str = "clipboardboard-image-root-v1";

/// 图片目录准备错误的稳定分类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageStoragePrepareErrorKind {
    /// 路径布局本身无效。
    InvalidLayout,
    /// 文件系统权限拒绝。
    PermissionDenied,
    /// 磁盘或设备空间不足。
    StorageFull,
    /// 已有目录包含非本应用管理的内容。
    UnknownDirectoryContents,
    /// 根身份文件格式无效。
    InvalidOwnerMarker,
    /// 目录是重解析点。
    ReparsePoint,
    /// 句柄最终路径不满足固定目录边界。
    UnsafePath,
    /// 恢复基目录与资产根跨卷。
    CrossVolumeRecovery,
    /// 其他文件系统错误。
    Io,
}

/// 不包含用户内容或完整本地路径的目录准备错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageStoragePrepareError {
    /// 供 UI 和回退策略稳定判断的错误分类。
    kind: ImageStoragePrepareErrorKind,
    /// 不含路径的失败操作描述。
    operation: &'static str,
}

impl ImageStoragePrepareError {
    /// 构造稳定错误。
    pub(super) const fn new(kind: ImageStoragePrepareErrorKind, operation: &'static str) -> Self {
        Self { kind, operation }
    }

    /// 将 IO 错误收敛为可展示、可测试的类别。
    pub(super) fn from_io(operation: &'static str, error: io::Error) -> Self {
        let kind = match error.kind() {
            io::ErrorKind::PermissionDenied => Self::permission_denied_kind(),
            io::ErrorKind::StorageFull => ImageStoragePrepareErrorKind::StorageFull,
            _ => ImageStoragePrepareErrorKind::Io,
        };
        Self::new(kind, operation)
    }

    /// 单独封装权限分类，避免调用方依赖平台错误码。
    const fn permission_denied_kind() -> ImageStoragePrepareErrorKind {
        ImageStoragePrepareErrorKind::PermissionDenied
    }

    /// 返回稳定错误分类。
    pub const fn kind(&self) -> ImageStoragePrepareErrorKind {
        self.kind
    }

    /// 返回不含用户路径的操作描述。
    pub const fn operation(&self) -> &'static str {
        self.operation
    }
}

impl fmt::Display for ImageStoragePrepareError {
    /// 返回简短中文错误。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}：{:?}", self.operation, self.kind)
    }
}

impl std::error::Error for ImageStoragePrepareError {}

impl From<ImageStoragePathError> for ImageStoragePrepareError {
    /// 将路径布局错误转换为准备错误。
    fn from(_: ImageStoragePathError) -> Self {
        Self::new(
            ImageStoragePrepareErrorKind::InvalidLayout,
            "构造图片存储路径",
        )
    }
}

/// 自定义目录失败后的回退信息。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageStorageFallback {
    /// 用户原本请求的自定义目录。
    requested_path: PathBuf,
    /// 自定义目录失败的稳定原因。
    reason: ImageStoragePrepareError,
}

impl ImageStorageFallback {
    /// 返回用户请求路径，供设置界面解释当前实际生效位置。
    pub fn requested_path(&self) -> &Path {
        &self.requested_path
    }

    /// 返回自定义目录失败原因。
    pub const fn reason(&self) -> &ImageStoragePrepareError {
        &self.reason
    }
}

/// 已准备图片存储 capability；不可克隆，生命周期内固定受管目录身份。
#[derive(Debug)]
pub struct PreparedImageStorage {
    /// 实际生效的路径布局。
    layout: ImageStorageLayout,
    /// 首次创建后稳定不变的资产根身份。
    root_id: ImageAssetRootId,
    /// 当前由 Windows 句柄确认的规范资产根路径。
    canonical_root: PathBuf,
    /// 自定义失败时记录原请求与原因。
    fallback: Option<ImageStorageFallback>,
    /// 持有受管目录句柄，避免运行期间被替换。
    _guard: WindowsStorageGuard,
}

/// 已创建且由 Windows 句柄固定身份的单次图片发布目标。
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct PreparedAssetPublish {
    /// 哈希绑定的原图、缩略图相对与绝对路径。
    pub paths: ImageAssetPaths,
    /// 固定 staging 目录；发布方只能在此创建独占临时文件。
    pub staging_directory: PathBuf,
    /// capability 存活期间禁止两个分片目录被替换。
    _shard_guard: PublishShardGuard,
}

impl PreparedImageStorage {
    /// 返回实际生效布局。
    pub const fn layout(&self) -> &ImageStorageLayout {
        &self.layout
    }

    /// 返回稳定资产根身份。
    pub const fn root_id(&self) -> ImageAssetRootId {
        self.root_id
    }

    /// 返回当前规范资产根路径。
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    /// 返回自定义目录失败后的回退详情。
    pub const fn fallback(&self) -> Option<&ImageStorageFallback> {
        self.fallback.as_ref()
    }

    /// 为一个内容哈希创建并固定发布目录，不向流水线暴露顶层目录句柄。
    #[allow(dead_code)]
    pub(crate) fn prepare_asset_publish(
        &self,
        content_hash: &[u8; 32],
    ) -> Result<PreparedAssetPublish, ImageStoragePrepareError> {
        let paths = self.layout.asset_paths(content_hash);
        let original_shard = paths.image_absolute.parent().ok_or_else(|| {
            ImageStoragePrepareError::new(
                ImageStoragePrepareErrorKind::UnsafePath,
                "原图目标缺少分片目录",
            )
        })?;
        let thumbnail_shard = paths.thumbnail_absolute.parent().ok_or_else(|| {
            ImageStoragePrepareError::new(
                ImageStoragePrepareErrorKind::UnsafePath,
                "缩略图目标缺少分片目录",
            )
        })?;
        create_directory(original_shard, "创建原图分片目录")?;
        create_directory(thumbnail_shard, "创建缩略图分片目录")?;
        let shard_guard = self
            ._guard
            .hold_publish_shards(original_shard, thumbnail_shard)?;
        Ok(PreparedAssetPublish {
            paths,
            staging_directory: self.layout.staging_directory().to_path_buf(),
            _shard_guard: shard_guard,
        })
    }
}

/// 按偏好准备图片目录；自定义目录失败时回退默认目录并保留原因。
pub fn prepare_image_storage(
    preference: ImageStoragePreference,
) -> Result<PreparedImageStorage, ImageStoragePrepareError> {
    prepare_with_local_app_data(std::env::var_os("LOCALAPPDATA"), preference)
}

/// 使用显式 LOCALAPPDATA 隔离测试，避免修改进程环境。
fn prepare_with_local_app_data(
    local_app_data: Option<OsString>,
    preference: ImageStoragePreference,
) -> Result<PreparedImageStorage, ImageStoragePrepareError> {
    match preference {
        ImageStoragePreference::Default => {
            let layout = ImageStorageLayout::from_preference_with_local_app_data(
                local_app_data,
                ImageStoragePreference::Default,
            )?;
            prepare_layout(layout, None)
        }
        ImageStoragePreference::Custom(requested_path) => {
            let custom_result = ImageStorageLayout::from_preference_with_local_app_data(
                local_app_data.clone(),
                ImageStoragePreference::Custom(requested_path.clone()),
            )
            .map_err(ImageStoragePrepareError::from)
            .and_then(|layout| prepare_layout(layout, None));
            match custom_result {
                Ok(prepared) => Ok(prepared),
                Err(reason) => {
                    let default_layout = ImageStorageLayout::from_preference_with_local_app_data(
                        local_app_data,
                        ImageStoragePreference::Default,
                    )?;
                    prepare_layout(
                        default_layout,
                        Some(ImageStorageFallback {
                            requested_path,
                            reason,
                        }),
                    )
                }
            }
        }
    }
}

/// 创建并校验单个图片目录布局。
fn prepare_layout(
    layout: ImageStorageLayout,
    fallback: Option<ImageStorageFallback>,
) -> Result<PreparedImageStorage, ImageStoragePrepareError> {
    create_directory(layout.asset_root(), "创建图片资产根")?;
    reject_unknown_root_contents(layout.asset_root())?;
    let root_id = read_or_create_root_id(layout.asset_root())?;

    for (path, operation) in [
        (layout.original_directory(), "创建原图目录"),
        (layout.thumbnail_directory(), "创建缩略图目录"),
        (layout.staging_directory(), "创建临时发布目录"),
        (layout.recovery_base_directory(), "创建恢复基目录"),
    ] {
        create_directory(path, operation)?;
    }

    let guard = WindowsStorageGuard::open(
        layout.asset_root(),
        layout.original_directory(),
        layout.thumbnail_directory(),
        layout.staging_directory(),
        layout.recovery_base_directory(),
    )?;
    let canonical_root = guard.asset_root.canonical_path.clone();
    Ok(PreparedImageStorage {
        layout,
        root_id,
        canonical_root,
        fallback,
        _guard: guard,
    })
}

/// 创建缺失目录；已存在普通文件或其他错误必须显式失败。
fn create_directory(path: &Path, operation: &'static str) -> Result<(), ImageStoragePrepareError> {
    fs::create_dir_all(path)
        .map_err(|error| ImageStoragePrepareError::from_io(operation, error))?;
    if !path.is_dir() {
        return Err(ImageStoragePrepareError::new(
            ImageStoragePrepareErrorKind::Io,
            operation,
        ));
    }
    Ok(())
}

/// 已有根只允许身份文件和三个固定子目录，避免误用用户的普通资料目录。
fn reject_unknown_root_contents(root: &Path) -> Result<(), ImageStoragePrepareError> {
    let has_owner = root.join(OWNER_FILE_NAME).is_file();
    for entry in fs::read_dir(root)
        .map_err(|error| ImageStoragePrepareError::from_io("枚举图片资产根", error))?
    {
        let entry =
            entry.map_err(|error| ImageStoragePrepareError::from_io("读取资产根条目", error))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // 尚无身份文件时只接受完全空目录，不能因用户目录恰好叫 original 而误认领。
        if !has_owner {
            return Err(ImageStoragePrepareError::new(
                ImageStoragePrepareErrorKind::UnknownDirectoryContents,
                "未认领图片资产根不是空目录",
            ));
        }
        let is_owner = name == OWNER_FILE_NAME && entry.path().is_file();
        let is_managed_directory = ["original", "thumbnail", "staging"]
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
            && entry.path().is_dir();
        if !is_owner && !is_managed_directory {
            return Err(ImageStoragePrepareError::new(
                ImageStoragePrepareErrorKind::UnknownDirectoryContents,
                "图片资产根包含未知内容",
            ));
        }
    }
    Ok(())
}

/// 读取已有根 ID；空目录首次准备时根据规范路径生成并写入。
fn read_or_create_root_id(root: &Path) -> Result<ImageAssetRootId, ImageStoragePrepareError> {
    let owner_path = root.join(OWNER_FILE_NAME);
    if owner_path.exists() {
        let contents = fs::read_to_string(owner_path)
            .map_err(|error| ImageStoragePrepareError::from_io("读取图片根身份", error))?;
        return parse_owner(&contents);
    }

    let canonical = fs::canonicalize(root)
        .map_err(|error| ImageStoragePrepareError::from_io("规范化图片资产根", error))?;
    let root_id = derive_root_id(&canonical);
    let contents = format!(
        "{OWNER_VERSION}\nid={}\n",
        content_hash_hex(root_id.as_bytes())
    );
    let mut owner = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(owner_path)
        .map_err(|error| ImageStoragePrepareError::from_io("创建图片根身份", error))?;
    owner
        .write_all(contents.as_bytes())
        .and_then(|()| owner.sync_all())
        .map_err(|error| ImageStoragePrepareError::from_io("持久化图片根身份", error))?;
    Ok(root_id)
}

/// 首次创建以规范路径做域分隔 BLAKE3；移动后直接沿用身份文件。
fn derive_root_id(canonical_path: &Path) -> ImageAssetRootId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"ClipboardBoard/image-storage-root/v1\0");
    hasher.update(
        canonical_path
            .as_os_str()
            .to_string_lossy()
            .to_lowercase()
            .as_bytes(),
    );
    ImageAssetRootId::new(*hasher.finalize().as_bytes())
}

/// 严格解析根身份文件。
fn parse_owner(contents: &str) -> Result<ImageAssetRootId, ImageStoragePrepareError> {
    let mut lines = contents.lines();
    let valid_version = lines.next() == Some(OWNER_VERSION);
    let encoded = lines.next().and_then(|line| line.strip_prefix("id="));
    if !valid_version || lines.next().is_some() {
        return Err(ImageStoragePrepareError::new(
            ImageStoragePrepareErrorKind::InvalidOwnerMarker,
            "图片根身份格式无效",
        ));
    }
    let encoded = encoded.ok_or_else(|| {
        ImageStoragePrepareError::new(
            ImageStoragePrepareErrorKind::InvalidOwnerMarker,
            "图片根身份缺少 ID",
        )
    })?;
    if encoded.len() != 64
        || encoded
            .bytes()
            .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte))
    {
        return Err(ImageStoragePrepareError::new(
            ImageStoragePrepareErrorKind::InvalidOwnerMarker,
            "图片根身份 ID 无效",
        ));
    }
    let mut decoded = [0_u8; 32];
    for (index, output) in decoded.iter_mut().enumerate() {
        *output = u8::from_str_radix(&encoded[index * 2..index * 2 + 2], 16).map_err(|_| {
            ImageStoragePrepareError::new(
                ImageStoragePrepareErrorKind::InvalidOwnerMarker,
                "图片根身份 ID 无效",
            )
        })?;
    }
    Ok(ImageAssetRootId::new(decoded))
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证幂等创建、自定义失败回退和移动后的稳定根身份。

    use std::{
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{prepare_with_local_app_data, ImageStoragePrepareErrorKind};
    use crate::image_storage::{ImageStoragePreference, ImageStorageRootKind};

    /// 为单个测试建立唯一临时目录。
    fn test_base(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时间早于 UNIX_EPOCH")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "clipboard-board-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    /// 仅清理带测试固定前缀的目录。
    fn cleanup(path: &Path) {
        assert!(path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("clipboard-board-")));
        let _ = fs::remove_dir_all(path);
    }

    /// 验证默认目录可重复准备，并创建固定子目录。
    #[test]
    fn default_prepare_is_idempotent() {
        let base = test_base("default");
        fs::create_dir_all(&base).expect("创建测试基目录失败");
        let first = prepare_with_local_app_data(
            Some(OsString::from(&base)),
            ImageStoragePreference::Default,
        )
        .expect("首次准备默认目录失败");
        let root_id = first.root_id();
        assert_eq!(first.layout().root_kind(), ImageStorageRootKind::Default);
        drop(first);

        let second = prepare_with_local_app_data(
            Some(OsString::from(&base)),
            ImageStoragePreference::Default,
        )
        .expect("重复准备默认目录失败");
        assert_eq!(second.root_id(), root_id);
        assert!(second.layout().original_directory().is_dir());
        assert!(second.layout().thumbnail_directory().is_dir());
        assert!(second.layout().staging_directory().is_dir());
        drop(second);
        cleanup(&base);
    }

    /// 验证自定义目录含未知文件时保留该文件，并回退默认目录。
    #[test]
    fn unknown_custom_directory_falls_back_without_deleting_content() {
        let base = test_base("fallback");
        let requested = base.join("requested");
        let local = base.join("local");
        fs::create_dir_all(&requested).expect("创建自定义目录失败");
        fs::create_dir_all(&local).expect("创建默认基目录失败");
        fs::write(requested.join("user.txt"), b"keep").expect("写入未知文件失败");

        let prepared = prepare_with_local_app_data(
            Some(OsString::from(&local)),
            ImageStoragePreference::Custom(requested.clone()),
        )
        .expect("回退默认目录失败");
        let fallback = prepared.fallback().expect("缺少回退详情");
        assert_eq!(fallback.requested_path(), requested);
        assert_eq!(
            fallback.reason().kind(),
            ImageStoragePrepareErrorKind::UnknownDirectoryContents
        );
        assert_eq!(
            fs::read(requested.join("user.txt")).expect("未知文件被删除"),
            b"keep"
        );
        drop(prepared);
        cleanup(&base);
    }

    /// 验证未认领根即使只含同名 original 目录，也不会被误认为应用资产。
    #[test]
    fn unowned_original_directory_is_not_claimed() {
        let base = test_base("unowned");
        let requested = base.join("requested");
        let original = requested.join("original");
        let local = base.join("local");
        fs::create_dir_all(&original).expect("创建用户 original 目录失败");
        fs::create_dir_all(&local).expect("创建默认基目录失败");
        fs::write(original.join("photo.jpg"), b"user-image").expect("写入用户图片失败");

        let prepared = prepare_with_local_app_data(
            Some(OsString::from(&local)),
            ImageStoragePreference::Custom(requested.clone()),
        )
        .expect("回退默认目录失败");
        assert_eq!(
            prepared.fallback().expect("缺少回退详情").reason().kind(),
            ImageStoragePrepareErrorKind::UnknownDirectoryContents
        );
        assert_eq!(
            fs::read(original.join("photo.jpg")).expect("用户图片被改动"),
            b"user-image"
        );
        drop(prepared);
        cleanup(&base);
    }

    /// 验证移动同卷受管目录后沿用根 ID，并返回新规范路径。
    #[test]
    fn moved_root_preserves_id_and_updates_path() {
        let base = test_base("move");
        fs::create_dir_all(&base).expect("创建测试基目录失败");
        let first_path = base.join("first");
        let first =
            prepare_with_local_app_data(None, ImageStoragePreference::Custom(first_path.clone()))
                .expect("首次准备自定义目录失败");
        let root_id = first.root_id();
        drop(first);

        let second_path = base.join("second");
        fs::rename(first_path, &second_path).expect("移动受管目录失败");
        let second =
            prepare_with_local_app_data(None, ImageStoragePreference::Custom(second_path.clone()))
                .expect("移动后重新准备失败");
        assert_eq!(second.root_id(), root_id);
        assert!(second.canonical_root().ends_with("second"));
        assert!(second_path.is_dir());
        drop(second);
        cleanup(&base);
    }
}

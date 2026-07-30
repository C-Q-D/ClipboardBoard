//! 此模块把规范图片经独占 staging 文件耐久发布，并在复用或返回前验证两个资产。
//!
//! 两个最终文件无法跨文件原子提交，因此流程固定先同步两份 staging，再依次发布；
//! 可观察失败只回滚本次创建的文件，崩溃遗留由后续启动恢复原子处理。

use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{BufReader, Write},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use image::{ImageFormat, ImageReader, Limits};
use windows_sys::Win32::{
    Foundation::{GENERIC_READ, HANDLE},
    Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, DELETE, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_DISPOSITION_INFO, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    },
};

use crate::{
    domain::{CanonicalImagePixels, ImageMetadata, ImageMetadataError},
    image_decode::{MAX_IMAGE_DIMENSION, MAX_IMAGE_RGBA_BYTES},
    image_storage::{ImageStoragePrepareError, PreparedAssetPublish, PreparedImageStorage},
};

use super::{
    build_thumbnail, encode_original_png, encode_thumbnail_webp, ImageEncodingError,
    ThumbnailPixels, MAX_PERSISTED_PNG_BYTES, MAX_THUMBNAIL_EDGE,
};

/// 独占 staging 文件名冲突时的有限重试次数。
const STAGING_CREATE_ATTEMPTS: usize = 32;
/// WebP 缩略图文件的固定最大字节数。
const MAX_THUMBNAIL_WEBP_BYTES: u64 = 2 * 1024 * 1024;
/// 进程内单调发布 token，结合实例 nonce 与 PID 避免旧 staging 名称冲突。
static NEXT_STAGING_TOKEN: AtomicU64 = AtomicU64::new(1);
/// 每次进程启动只计算一次的实例 nonce。
static PROCESS_INSTANCE_NONCE: OnceLock<u128> = OnceLock::new();

/// 图片发布失败的稳定分类；错误不包含路径、像素或底层系统文本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImagePublishError {
    /// 图片存储 capability 无法建立安全发布目标。
    StorageUnavailable,
    /// 原图或缩略图编码失败。
    Encoding(ImageEncodingError),
    /// 无法独占创建 staging 文件。
    StagingUnavailable,
    /// staging 文件无法刷新到磁盘。
    SyncFailed,
    /// 已存在资产只有原图或只有缩略图。
    ExistingAssetIncomplete,
    /// 已存在资产内容、格式、尺寸或哈希不符合契约。
    ExistingAssetInvalid,
    /// staging 文件无法发布到最终位置。
    PublishFailed,
    /// 新发布资产未通过完整回读验证。
    VerificationFailed,
    /// 失败后无法完整删除本次创建的资产。
    RollbackIncomplete,
    /// 已验证文件无法构造领域元数据。
    Metadata(ImageMetadataError),
}

impl fmt::Display for ImagePublishError {
    /// 返回不泄漏本地路径和图片内容的稳定中文错误。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StorageUnavailable => write!(formatter, "图片存储目标不可安全使用"),
            Self::Encoding(error) => write!(formatter, "图片编码失败：{error}"),
            Self::StagingUnavailable => write!(formatter, "无法创建图片临时文件"),
            Self::SyncFailed => write!(formatter, "图片临时文件无法耐久同步"),
            Self::ExistingAssetIncomplete => write!(formatter, "已有图片资产不完整"),
            Self::ExistingAssetInvalid => write!(formatter, "已有图片资产校验失败"),
            Self::PublishFailed => write!(formatter, "图片资产发布失败"),
            Self::VerificationFailed => write!(formatter, "新发布图片资产校验失败"),
            Self::RollbackIncomplete => write!(formatter, "图片发布回滚不完整"),
            Self::Metadata(error) => write!(formatter, "图片元数据无效：{error}"),
        }
    }
}

impl std::error::Error for ImagePublishError {}

impl From<ImageEncodingError> for ImagePublishError {
    /// 保留稳定编码错误分类。
    fn from(value: ImageEncodingError) -> Self {
        Self::Encoding(value)
    }
}

impl From<ImageMetadataError> for ImagePublishError {
    /// 保留领域元数据错误分类。
    fn from(value: ImageMetadataError) -> Self {
        Self::Metadata(value)
    }
}

impl From<ImageStoragePrepareError> for ImagePublishError {
    /// 存储层细节统一折叠为 capability 不可用，避免路径泄漏。
    fn from(_: ImageStoragePrepareError) -> Self {
        Self::StorageUnavailable
    }
}

/// 已发布且完整回读验证的图片资产；提交数据库前保留本次文件所有权。
pub(crate) struct PublishedImageAssets {
    /// 可直接写入持久化记录的领域元数据。
    metadata: ImageMetadata,
    /// 持续固定两个分片目录身份，直至明确 commit 或 rollback。
    target: PreparedAssetPublish,
    /// 持续固定两个普通文件身份，防止 finalize 前被替换。
    verified_files: VerifiedAssetFiles,
    /// 本次调用是否创建原图。
    created_original: bool,
    /// 本次调用是否创建缩略图。
    created_thumbnail: bool,
}

impl fmt::Debug for PublishedImageAssets {
    /// Debug 只暴露哈希、尺寸和所有权，不输出本地绝对路径。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublishedImageAssets")
            .field("content_hash", &self.metadata.content_hash())
            .field("width", &self.metadata.width())
            .field("height", &self.metadata.height())
            .field("created_original", &self.created_original)
            .field("created_thumbnail", &self.created_thumbnail)
            .finish()
    }
}

impl PublishedImageAssets {
    /// 返回经过回读验证的持久化元数据。
    pub const fn metadata(&self) -> &ImageMetadata {
        &self.metadata
    }

    /// 数据库提交失败时，仅删除本次调用创建的最终资产。
    pub(crate) fn rollback_created(self) -> Result<(), ImagePublishError> {
        let Self {
            target,
            verified_files,
            created_original,
            created_thumbnail,
            ..
        } = self;
        // 按仍持有的文件身份句柄标记删除，避免关闭句柄到按路径删除之间的替换竞态。
        let result = verified_files.delete_created(created_original, created_thumbnail);
        drop(target);
        result
    }

    /// 数据库提交成功后消费所有权，明确禁止随后回滚已引用资产。
    pub(crate) fn commit(self) -> ImageMetadata {
        self.metadata
    }
}

/// 把一份拥有型规范图片发布为完整原图/缩略图资产对。
pub(crate) fn publish_image_assets(
    storage: &PreparedImageStorage,
    image: CanonicalImagePixels,
) -> Result<PublishedImageAssets, ImagePublishError> {
    publish_image_assets_with_fault(storage, image, PublishFault::default())
}

/// 内部实现允许定向测试在固定阶段注入错误，不改变生产文件系统接口。
fn publish_image_assets_with_fault(
    storage: &PreparedImageStorage,
    image: CanonicalImagePixels,
    fault: PublishFault,
) -> Result<PublishedImageAssets, ImagePublishError> {
    let content_hash = image.content_hash();
    let width = image.width();
    let height = image.height();
    let thumbnail = build_thumbnail(&image)?;
    let target = storage.prepare_asset_publish(&content_hash)?;
    let image_exists = target
        .paths
        .image_absolute
        .try_exists()
        .map_err(|_| ImagePublishError::StorageUnavailable)?;
    let thumbnail_exists = target
        .paths
        .thumbnail_absolute
        .try_exists()
        .map_err(|_| ImagePublishError::StorageUnavailable)?;

    if image_exists != thumbnail_exists {
        return Err(ImagePublishError::ExistingAssetIncomplete);
    }
    if image_exists {
        drop(image);
        let verified_files = verify_asset_pair(
            &target.paths.image_absolute,
            &target.paths.thumbnail_absolute,
            content_hash,
            width,
            height,
            &thumbnail,
        )
        .map_err(|_| ImagePublishError::ExistingAssetInvalid)?;
        return build_result(
            storage,
            target,
            content_hash,
            width,
            height,
            verified_files,
            false,
            false,
        );
    }

    let (mut image_stage, mut thumbnail_stage) =
        create_staging_pair(&target.staging_directory, &content_hash)?;
    encode_original_png(&image, image_stage.writer())?;
    if fault.second_encoding {
        return Err(ImagePublishError::Encoding(
            ImageEncodingError::EncodeFailed,
        ));
    }
    encode_thumbnail_webp(&thumbnail, thumbnail_stage.writer())?;
    sync_staging(image_stage.writer())?;
    sync_staging(thumbnail_stage.writer())?;
    drop(image);
    image_stage.close();
    thumbnail_stage.close();

    if fs::rename(&image_stage.path, &target.paths.image_absolute).is_err() {
        return finish_concurrent_publish(
            storage,
            target,
            content_hash,
            width,
            height,
            &thumbnail,
            false,
        );
    }
    image_stage.published = true;
    if fault.second_publish
        || fs::rename(&thumbnail_stage.path, &target.paths.thumbnail_absolute).is_err()
    {
        if target.paths.thumbnail_absolute.exists() {
            return finish_concurrent_publish(
                storage,
                target,
                content_hash,
                width,
                height,
                &thumbnail,
                true,
            );
        }
        let rollback = if fault.rollback {
            Err(std::io::Error::other("测试注入回滚失败"))
        } else {
            fs::remove_file(&target.paths.image_absolute)
        };
        return match rollback {
            Ok(()) => Err(ImagePublishError::PublishFailed),
            Err(_) => Err(ImagePublishError::RollbackIncomplete),
        };
    }
    thumbnail_stage.published = true;

    let verified_files = match verify_asset_pair(
        &target.paths.image_absolute,
        &target.paths.thumbnail_absolute,
        content_hash,
        width,
        height,
        &thumbnail,
    ) {
        Ok(verified_files) => verified_files,
        Err(()) => {
            rollback_paths(
                &target.paths.image_absolute,
                true,
                &target.paths.thumbnail_absolute,
                true,
            )?;
            return Err(ImagePublishError::VerificationFailed);
        }
    };
    build_result(
        storage,
        target,
        content_hash,
        width,
        height,
        verified_files,
        true,
        true,
    )
}

/// 并发调用已抢先发布目标时，重新验证完整资产对并安全复用。
fn finish_concurrent_publish(
    storage: &PreparedImageStorage,
    target: PreparedAssetPublish,
    content_hash: [u8; 32],
    width: u32,
    height: u32,
    thumbnail: &ThumbnailPixels,
    created_original: bool,
) -> Result<PublishedImageAssets, ImagePublishError> {
    let image_exists = target.paths.image_absolute.exists();
    let thumbnail_exists = target.paths.thumbnail_absolute.exists();
    if image_exists != thumbnail_exists {
        if created_original {
            fs::remove_file(&target.paths.image_absolute)
                .map_err(|_| ImagePublishError::RollbackIncomplete)?;
            return Err(ImagePublishError::PublishFailed);
        }
        return Err(ImagePublishError::ExistingAssetIncomplete);
    }
    if !image_exists {
        return Err(ImagePublishError::PublishFailed);
    }
    let verified_files = match verify_asset_pair(
        &target.paths.image_absolute,
        &target.paths.thumbnail_absolute,
        content_hash,
        width,
        height,
        thumbnail,
    ) {
        Ok(verified_files) => verified_files,
        Err(()) => {
            if created_original {
                fs::remove_file(&target.paths.image_absolute)
                    .map_err(|_| ImagePublishError::RollbackIncomplete)?;
            }
            return Err(ImagePublishError::ExistingAssetInvalid);
        }
    };
    build_result(
        storage,
        target,
        content_hash,
        width,
        height,
        verified_files,
        // 一旦并发方补齐完整资产对，本调用放弃删除所有权，避免破坏对方可能提交的引用。
        false,
        false,
    )
}

/// 把已验证路径和文件句柄封装为领域元数据与 finalize 所有权。
#[allow(clippy::too_many_arguments)]
fn build_result(
    storage: &PreparedImageStorage,
    target: PreparedAssetPublish,
    content_hash: [u8; 32],
    width: u32,
    height: u32,
    verified_files: VerifiedAssetFiles,
    created_original: bool,
    created_thumbnail: bool,
) -> Result<PublishedImageAssets, ImagePublishError> {
    let metadata = ImageMetadata::new(
        content_hash,
        storage.root_id(),
        target.paths.image_relative.clone(),
        target.paths.thumbnail_relative.clone(),
        width,
        height,
        verified_files.image_size,
    );
    let metadata = match metadata {
        Ok(metadata) => metadata,
        Err(error) => {
            verified_files.delete_created(created_original, created_thumbnail)?;
            return Err(ImagePublishError::Metadata(error));
        }
    };
    Ok(PublishedImageAssets {
        metadata,
        target,
        verified_files,
        created_original,
        created_thumbnail,
    })
}

/// 仅供定向测试的固定阶段故障开关，生产入口始终使用全 false。
#[derive(Clone, Copy, Debug, Default)]
struct PublishFault {
    /// 第一份编码成功后模拟第二份编码失败。
    second_encoding: bool,
    /// 原图发布后模拟第二份 rename 失败。
    second_publish: bool,
    /// 第二份发布失败后模拟原图删除失败。
    rollback: bool,
}

/// staging 文件的 RAII 清理状态；未发布临时文件会在所有错误路径自动删除。
struct StagingFile {
    /// 独占创建的临时文件路径。
    path: PathBuf,
    /// 编码期间持有的文件；发布前显式关闭。
    file: Option<File>,
    /// 成功 rename 后不再删除原 staging 路径。
    published: bool,
}

impl StagingFile {
    /// 返回可写文件引用；文件关闭后调用属于流水线内部错误。
    fn writer(&mut self) -> &mut File {
        self.file.as_mut().expect("staging 文件已提前关闭")
    }

    /// 发布前关闭文件句柄，确保 Windows rename 不被自身句柄阻止。
    fn close(&mut self) {
        self.file.take();
    }
}

impl Drop for StagingFile {
    /// 任意提前返回都尽力清理本次独占创建且尚未发布的临时文件。
    fn drop(&mut self) {
        self.file.take();
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// 使用实例 nonce、PID 与单调 token 独占创建两份 staging 文件。
fn create_staging_pair(
    directory: &Path,
    content_hash: &[u8; 32],
) -> Result<(StagingFile, StagingFile), ImagePublishError> {
    create_staging_pair_with_counter(directory, content_hash, &NEXT_STAGING_TOKEN)
}

/// 使用显式 token 源创建 staging，便于无全局竞态地验证碰撞重试协议。
fn create_staging_pair_with_counter(
    directory: &Path,
    content_hash: &[u8; 32],
    counter: &AtomicU64,
) -> Result<(StagingFile, StagingFile), ImagePublishError> {
    for _ in 0..STAGING_CREATE_ATTEMPTS {
        let token = counter.fetch_add(1, Ordering::Relaxed);
        let (image_path, thumbnail_path) = staging_paths(directory, content_hash, token);
        let image_file = match create_new_file(&image_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(ImagePublishError::StagingUnavailable),
        };
        let image_stage = StagingFile {
            path: image_path,
            file: Some(image_file),
            published: false,
        };
        match create_new_file(&thumbnail_path) {
            Ok(file) => {
                return Ok((
                    image_stage,
                    StagingFile {
                        path: thumbnail_path,
                        file: Some(file),
                        published: false,
                    },
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(ImagePublishError::StagingUnavailable),
        }
    }
    Err(ImagePublishError::StagingUnavailable)
}

/// 根据固定实例 nonce、PID、token 与哈希前缀构造两份 staging 路径。
fn staging_paths(directory: &Path, content_hash: &[u8; 32], token: u64) -> (PathBuf, PathBuf) {
    let hash_prefix = format!("{:02x}{:02x}", content_hash[0], content_hash[1]);
    let stem = format!(
        "{:032x}-{}-{token}-{hash_prefix}",
        process_instance_nonce(),
        process::id()
    );
    (
        directory.join(format!("{stem}.png.tmp")),
        directory.join(format!("{stem}.webp.tmp")),
    )
}

/// 以 `create_new` 获得当前调用唯一拥有的 staging 文件。
fn create_new_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create_new(true).write(true).open(path)
}

/// 同步单个 staging 文件的用户态缓冲与文件内容。
fn sync_staging(file: &mut File) -> Result<(), ImagePublishError> {
    file.flush().map_err(|_| ImagePublishError::SyncFailed)?;
    file.sync_all().map_err(|_| ImagePublishError::SyncFailed)
}

/// 返回当前进程实例 nonce；时间异常时仍由 PID 与单调 token 保证进程内唯一。
fn process_instance_nonce() -> u128 {
    *PROCESS_INSTANCE_NONCE.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
            ^ u128::from(process::id())
    })
}

/// 回读验证 PNG 哈希/尺寸与 lossless WebP，并返回持续固定身份的文件句柄。
fn verify_asset_pair(
    image_path: &Path,
    thumbnail_path: &Path,
    expected_hash: [u8; 32],
    expected_width: u32,
    expected_height: u32,
    expected_thumbnail: &ThumbnailPixels,
) -> Result<VerifiedAssetFiles, ()> {
    let image_file = open_regular_file(image_path, MAX_PERSISTED_PNG_BYTES)?;
    let image_size = image_file.metadata().map_err(|_| ())?.len();
    let image = decode_limited(
        image_file.try_clone().map_err(|_| ())?,
        ImageFormat::Png,
        MAX_IMAGE_DIMENSION,
        MAX_IMAGE_DIMENSION,
        MAX_IMAGE_RGBA_BYTES as u64,
    )?;
    let canonical =
        CanonicalImagePixels::new(image.width(), image.height(), image.into_rgba8().into_raw())
            .map_err(|_| ())?;
    if canonical.width() != expected_width
        || canonical.height() != expected_height
        || canonical.content_hash() != expected_hash
    {
        return Err(());
    }

    let thumbnail_file = open_regular_file(thumbnail_path, MAX_THUMBNAIL_WEBP_BYTES)?;
    let thumbnail = decode_limited(
        thumbnail_file.try_clone().map_err(|_| ())?,
        ImageFormat::WebP,
        MAX_THUMBNAIL_EDGE,
        MAX_THUMBNAIL_EDGE,
        (MAX_THUMBNAIL_EDGE as u64) * (MAX_THUMBNAIL_EDGE as u64) * 4,
    )?
    .into_rgba8();
    if thumbnail.width() != expected_thumbnail.width()
        || thumbnail.height() != expected_thumbnail.height()
        || thumbnail.as_raw() != expected_thumbnail.as_rgba_bytes()
    {
        return Err(());
    }
    Ok(VerifiedAssetFiles {
        image_size,
        _image: image_file,
        _thumbnail: thumbnail_file,
    })
}

/// 在解码前设置严格宽高与分配上限，避免持久化文件绕过来源边界。
fn decode_limited(
    file: File,
    format: ImageFormat,
    max_width: u32,
    max_height: u32,
    max_alloc: u64,
) -> Result<image::DynamicImage, ()> {
    let mut reader = ImageReader::with_format(BufReader::new(file), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(max_width);
    limits.max_image_height = Some(max_height);
    limits.max_alloc = Some(max_alloc);
    reader.limits(limits);
    reader.decode().map_err(|_| ())
}

/// 打开非重解析普通文件并固定其身份；句柄不共享 DELETE。
fn open_regular_file(path: &Path, maximum: u64) -> Result<File, ()> {
    let file = OpenOptions::new()
        .access_mode(GENERIC_READ | DELETE)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| ())?;
    let metadata = file.metadata().map_err(|_| ())?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !metadata.is_file()
        || !valid_file_size_for_policy(metadata.len(), maximum)
    {
        return Err(());
    }
    Ok(file)
}

/// 纯数值验证非空与独立持久化大小上限，测试无需创建大型真实文件。
fn valid_file_size_for_policy(size: u64, maximum: u64) -> bool {
    size > 0 && size <= maximum
}

/// finalize 前持续持有的两个已验证文件身份与原图大小。
struct VerifiedAssetFiles {
    /// 已发布原图实际文件大小。
    image_size: u64,
    /// 原图身份句柄。
    _image: File,
    /// 缩略图身份句柄。
    _thumbnail: File,
}

impl VerifiedAssetFiles {
    /// 按验证时固定的文件身份标记删除，只处理本次调用实际创建的成员。
    fn delete_created(
        self,
        remove_image: bool,
        remove_thumbnail: bool,
    ) -> Result<(), ImagePublishError> {
        let thumbnail_result = if remove_thumbnail {
            mark_file_for_deletion(&self._thumbnail)
        } else {
            Ok(())
        };
        let image_result = if remove_image {
            mark_file_for_deletion(&self._image)
        } else {
            Ok(())
        };
        if thumbnail_result.is_err() || image_result.is_err() {
            Err(ImagePublishError::RollbackIncomplete)
        } else {
            Ok(())
        }
    }
}

/// 使用带 DELETE 权限的既有句柄标记文件删除，目标身份不会在调用间改变。
fn mark_file_for_deletion(file: &File) -> Result<(), ()> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: 句柄来自仍存活且带 DELETE 权限的 File；输入结构体和长度完全匹配 API。
    let result = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfo,
            (&raw const disposition).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if result == 0 {
        Err(())
    } else {
        Ok(())
    }
}

/// 删除本次调用创建的最终文件；任意删除失败必须升级为显式回滚不完整。
fn rollback_paths(
    image_path: &Path,
    remove_image: bool,
    thumbnail_path: &Path,
    remove_thumbnail: bool,
) -> Result<(), ImagePublishError> {
    let thumbnail_result = if remove_thumbnail {
        fs::remove_file(thumbnail_path)
    } else {
        Ok(())
    };
    let image_result = if remove_image {
        fs::remove_file(image_path)
    } else {
        Ok(())
    };
    if thumbnail_result.is_err() || image_result.is_err() {
        Err(ImagePublishError::RollbackIncomplete)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块定向覆盖耐久发布、重复复用、不完整资产拒绝与缩略图不放大。

    use std::{
        fs::{self, OpenOptions},
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use crate::{
        domain::CanonicalImagePixels,
        image_decode::MAX_PNG_ENCODED_BYTES,
        image_storage::{prepare_image_storage, ImageStoragePreference},
    };

    use super::{
        create_staging_pair_with_counter, publish_image_assets, publish_image_assets_with_fault,
        staging_paths, valid_file_size_for_policy, ImageEncodingError, ImagePublishError,
        PublishFault, MAX_PERSISTED_PNG_BYTES,
    };

    /// 测试目录序号，避免并行用例共享图片根。
    static TEST_DIRECTORY_ID: AtomicU64 = AtomicU64::new(1);

    /// 建立当前用例独占的自定义图片根。
    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "clipboardboard-pipe02-{label}-{}-{}",
            std::process::id(),
            TEST_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// 删除测试图片根及其外部恢复目录。
    fn cleanup(root: &Path) {
        let recovery = root
            .parent()
            .expect("测试根应有父目录")
            .join(".clipboardboard-recovery");
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(recovery);
    }

    /// 构造带不同像素的 2×1 规范图片。
    fn sample() -> CanonicalImagePixels {
        CanonicalImagePixels::new(2, 1, vec![10, 20, 30, 255, 40, 50, 60, 255])
            .expect("构造测试图片失败")
    }

    /// 成功发布后两个文件存在，元数据尺寸与哈希完整。
    #[test]
    fn publishes_and_verifies_complete_asset_pair() {
        let root = test_root("success");
        let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
            .expect("准备测试存储失败");
        let expected_hash = sample().content_hash();
        let published = publish_image_assets(&storage, sample()).expect("发布图片失败");

        assert_eq!(published.metadata().content_hash(), &expected_hash);
        assert_eq!(published.metadata().width().get(), 2);
        assert_eq!(published.metadata().height().get(), 1);
        assert!(storage
            .layout()
            .asset_paths(&expected_hash)
            .image_absolute
            .is_file());
        assert_eq!(
            fs::read_dir(storage.layout().staging_directory())
                .expect("读取 staging 失败")
                .count(),
            0,
            "成功发布后不应遗留 staging 文件"
        );
        drop(published);
        drop(storage);
        cleanup(&root);
    }

    /// 相同图片再次发布必须验证并复用现有资产，而不是覆盖文件。
    #[test]
    fn duplicate_publish_reuses_verified_pair() {
        let root = test_root("reuse");
        let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
            .expect("准备测试存储失败");
        let first = publish_image_assets(&storage, sample()).expect("首次发布失败");
        let size = first.metadata().content_size();
        let _metadata = first.commit();
        let second = publish_image_assets(&storage, sample()).expect("重复发布失败");
        assert_eq!(second.metadata().content_size(), size);
        assert!(format!("{second:?}").contains("created_original: false"));
        drop(second);
        drop(storage);
        cleanup(&root);
    }

    /// 只有一侧资产存在时必须拒绝，不能把半成品当作可复用记录。
    #[test]
    fn rejects_one_sided_existing_asset() {
        let root = test_root("one-sided");
        let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
            .expect("准备测试存储失败");
        let hash = sample().content_hash();
        let paths = storage.layout().asset_paths(&hash);
        fs::create_dir(paths.image_absolute.parent().expect("原图应有父目录"))
            .expect("创建分片目录失败");
        fs::write(&paths.image_absolute, b"not-an-image").expect("写入单侧文件失败");

        assert_eq!(
            publish_image_assets(&storage, sample()).expect_err("单侧资产应被拒绝"),
            ImagePublishError::ExistingAssetIncomplete
        );
        drop(storage);
        cleanup(&root);
    }

    /// 已存在的完整路径对若内容损坏，必须校验失败且不得被当前调用删除。
    #[test]
    fn rejects_damaged_existing_pair_without_deleting_it() {
        let root = test_root("damaged");
        let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
            .expect("准备测试存储失败");
        let hash = sample().content_hash();
        let paths = storage.layout().asset_paths(&hash);
        fs::create_dir(paths.image_absolute.parent().expect("原图应有父目录"))
            .expect("创建原图分片失败");
        fs::create_dir(paths.thumbnail_absolute.parent().expect("缩略图应有父目录"))
            .expect("创建缩略图分片失败");
        fs::write(&paths.image_absolute, b"broken-png").expect("写入损坏原图失败");
        fs::write(&paths.thumbnail_absolute, b"broken-webp").expect("写入损坏缩略图失败");

        assert_eq!(
            publish_image_assets(&storage, sample()).expect_err("损坏资产应被拒绝"),
            ImagePublishError::ExistingAssetInvalid
        );
        assert!(paths.image_absolute.exists());
        assert!(paths.thumbnail_absolute.exists());
        drop(storage);
        cleanup(&root);
    }

    /// 小图缩略图不得放大，回读验证应保持原始 2×1 尺寸。
    #[test]
    fn low_resolution_thumbnail_is_not_upscaled() {
        let root = test_root("no-upscale");
        let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
            .expect("准备测试存储失败");
        let published = publish_image_assets(&storage, sample()).expect("发布小图失败");
        let paths = storage
            .layout()
            .asset_paths(published.metadata().content_hash());
        let thumbnail = image::open(paths.thumbnail_absolute)
            .expect("读取缩略图失败")
            .into_rgba8();
        assert_eq!(thumbnail.dimensions(), (2, 1));
        drop(published);
        drop(storage);
        cleanup(&root);
    }

    /// 第二份编码失败时只能清理 staging，不能产生任何正式文件。
    #[test]
    fn second_encoding_failure_cleans_staging_without_publish() {
        let root = test_root("second-encoding");
        let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
            .expect("准备测试存储失败");
        let paths = storage.layout().asset_paths(&sample().content_hash());
        let error = publish_image_assets_with_fault(
            &storage,
            sample(),
            PublishFault {
                second_encoding: true,
                ..PublishFault::default()
            },
        )
        .expect_err("第二编码故障应失败");
        assert_eq!(
            error,
            ImagePublishError::Encoding(ImageEncodingError::EncodeFailed)
        );
        assert!(!paths.image_absolute.exists());
        assert!(!paths.thumbnail_absolute.exists());
        assert_eq!(
            fs::read_dir(storage.layout().staging_directory())
                .expect("读取 staging 失败")
                .count(),
            0
        );
        drop(storage);
        cleanup(&root);
    }

    /// 第二份发布失败时删除本次原图；删除失败必须升级为独立错误。
    #[test]
    fn second_publish_failure_rolls_back_or_reports_incomplete() {
        for (rollback_failure, expected) in [
            (false, ImagePublishError::PublishFailed),
            (true, ImagePublishError::RollbackIncomplete),
        ] {
            let root = test_root("second-publish");
            let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
                .expect("准备测试存储失败");
            let paths = storage.layout().asset_paths(&sample().content_hash());
            let error = publish_image_assets_with_fault(
                &storage,
                sample(),
                PublishFault {
                    second_publish: true,
                    rollback: rollback_failure,
                    ..PublishFault::default()
                },
            )
            .expect_err("第二发布故障应失败");
            assert_eq!(error, expected);
            assert_eq!(paths.image_absolute.exists(), rollback_failure);
            assert!(!paths.thumbnail_absolute.exists());
            assert_eq!(
                fs::read_dir(storage.layout().staging_directory())
                    .expect("读取 staging 失败")
                    .count(),
                0
            );
            drop(storage);
            cleanup(&root);
        }
    }

    /// finalize 前目录与文件 capability 必须阻止分片被替换，commit 后才释放。
    #[test]
    fn published_result_holds_shard_identity_until_commit() {
        let root = test_root("capability");
        let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
            .expect("准备测试存储失败");
        let published = publish_image_assets(&storage, sample()).expect("发布图片失败");
        let paths = storage
            .layout()
            .asset_paths(published.metadata().content_hash());
        let shard = paths.image_absolute.parent().expect("原图应有分片目录");
        let moved = storage.layout().original_directory().join("moved-shard");
        assert!(fs::rename(shard, &moved).is_err());
        assert!(
            OpenOptions::new()
                .write(true)
                .open(&paths.image_absolute)
                .is_err(),
            "finalize 前不得允许原地改写已验证内容"
        );
        let _metadata = published.commit();
        OpenOptions::new()
            .write(true)
            .open(&paths.image_absolute)
            .expect("commit 后应释放文件写入锁");
        fs::rename(shard, &moved).expect("commit 后应释放分片身份锁");
        drop(storage);
        cleanup(&root);
    }

    /// 超过 80 MiB 的稀疏 PNG 即使路径与哈希匹配也必须在解码前拒绝。
    #[test]
    fn persisted_png_size_policy_rejects_oversized_existing_file() {
        let root = test_root("persisted-limit");
        let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
            .expect("准备测试存储失败");
        let published = publish_image_assets(&storage, sample()).expect("发布图片失败");
        let paths = storage
            .layout()
            .asset_paths(published.metadata().content_hash());
        let _metadata = published.commit();
        OpenOptions::new()
            .write(true)
            .open(&paths.image_absolute)
            .expect("打开原图失败")
            .set_len(MAX_PERSISTED_PNG_BYTES + 1)
            .expect("设置稀疏文件大小失败");
        assert_eq!(
            publish_image_assets(&storage, sample()).expect_err("超限原图应被拒绝"),
            ImagePublishError::ExistingAssetInvalid
        );
        drop(storage);
        cleanup(&root);
    }

    /// 耐久 PNG policy 独立于注册 PNG 的 30 MiB 限制，并用小阈值验证两侧边界。
    #[test]
    fn persisted_policy_is_independent_from_registered_png_limit() {
        assert!(MAX_PERSISTED_PNG_BYTES > MAX_PNG_ENCODED_BYTES as u64);
        assert!(valid_file_size_for_policy(31, 80));
        assert!(valid_file_size_for_policy(80, 80));
        assert!(!valid_file_size_for_policy(81, 80));
    }

    /// 旧进程残留占用当前 token 时必须推进 token，且不得覆盖或清理旧文件。
    #[test]
    fn staging_collision_advances_token_without_touching_stale_file() {
        let root = test_root("staging-collision");
        let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
            .expect("准备测试存储失败");
        let hash = sample().content_hash();
        let counter = AtomicU64::new(700);
        let (stale_image, _stale_thumbnail) =
            staging_paths(storage.layout().staging_directory(), &hash, 700);
        fs::write(&stale_image, b"stale").expect("写入旧 staging 失败");

        let (image_stage, thumbnail_stage) =
            create_staging_pair_with_counter(storage.layout().staging_directory(), &hash, &counter)
                .expect("碰撞后应推进 token");
        assert_eq!(
            fs::read(&stale_image).expect("读取旧 staging 失败"),
            b"stale"
        );
        assert_ne!(image_stage.path, stale_image);
        drop(image_stage);
        drop(thumbnail_stage);
        assert!(stale_image.exists());
        drop(storage);
        cleanup(&root);
    }

    /// 指向根外资产的文件符号链接必须按 reparse 拒绝，不能跟随复用。
    #[test]
    fn rejects_reparse_asset_files() {
        use std::os::windows::fs::symlink_file;

        let source_root = test_root("reparse-source");
        let source_storage =
            prepare_image_storage(ImageStoragePreference::Custom(source_root.clone()))
                .expect("准备来源存储失败");
        let source = publish_image_assets(&source_storage, sample()).expect("发布来源图片失败");
        let source_paths = source_storage
            .layout()
            .asset_paths(source.metadata().content_hash());
        let _metadata = source.commit();

        let target_root = test_root("reparse-target");
        let target_storage =
            prepare_image_storage(ImageStoragePreference::Custom(target_root.clone()))
                .expect("准备目标存储失败");
        let target_paths = target_storage
            .layout()
            .asset_paths(&sample().content_hash());
        fs::create_dir(
            target_paths
                .image_absolute
                .parent()
                .expect("原图应有父目录"),
        )
        .expect("创建原图分片失败");
        fs::create_dir(
            target_paths
                .thumbnail_absolute
                .parent()
                .expect("缩略图应有父目录"),
        )
        .expect("创建缩略图分片失败");
        symlink_file(&source_paths.image_absolute, &target_paths.image_absolute)
            .expect("创建原图符号链接失败");
        symlink_file(
            &source_paths.thumbnail_absolute,
            &target_paths.thumbnail_absolute,
        )
        .expect("创建缩略图符号链接失败");
        assert_eq!(
            publish_image_assets(&target_storage, sample()).expect_err("重解析资产应被拒绝"),
            ImagePublishError::ExistingAssetInvalid
        );
        drop(target_storage);
        drop(source_storage);
        cleanup(&target_root);
        cleanup(&source_root);
    }
}

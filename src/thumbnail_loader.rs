//! 此模块在独立线程按需读取 WebP 缩略图并返回有界 RGBA8 像素。
//!
//! 加载器不创建 Slint Image，也不访问 UI 状态；请求只携带稳定卡片身份和受限绝对路径。

use std::{
    fmt, fs, io,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
        Arc,
    },
    thread::{self, JoinHandle},
};

use crate::image_pipeline::MAX_THUMBNAIL_EDGE;
use image::ImageDecoder;

/// 缩略图请求队列容量；快速滚动时拒绝多余请求，避免积累不可见图片。
const THUMBNAIL_QUEUE_CAPACITY: usize = 16;
/// 单个 WebP 文件读取上限；正常 320px 缩略图远低于该值。
const MAX_THUMBNAIL_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// 一次缩略图加载请求的稳定身份。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThumbnailLoadRequest {
    /// 面板打开代次，隐藏再打开后旧结果必须丢弃。
    pub panel_generation: u64,
    /// 历史记录 ID。
    pub id: u64,
    /// 与 ID 共同验证的内容哈希。
    pub content_hash: [u8; 32],
    /// 由已验证根和领域相对路径构造的 WebP 绝对路径。
    pub path: PathBuf,
}

/// 后台线程返回的有界 RGBA8 像素。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThumbnailPixels {
    /// 像素宽度，不超过固定缩略图边长。
    pub width: u32,
    /// 像素高度，不超过固定缩略图边长。
    pub height: u32,
    /// 行优先、非预乘 RGBA8 字节。
    pub rgba: Vec<u8>,
}

/// 缩略图加载的有限失败类别；不携带完整路径或底层错误文本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThumbnailLoadFailure {
    /// 路径不是绝对 WebP 文件定位。
    InvalidPath,
    /// 文件不存在、不可读或超过编码上限。
    Unavailable,
    /// WebP 解码失败或像素尺寸/长度不满足约束。
    InvalidImage,
}

/// 缩略图后台结果；始终回显请求身份。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThumbnailLoadResult {
    /// 面板打开代次。
    pub panel_generation: u64,
    /// 历史记录 ID。
    pub id: u64,
    /// 内容哈希。
    pub content_hash: [u8; 32],
    /// 成功像素或有限失败。
    pub outcome: Result<ThumbnailPixels, ThumbnailLoadFailure>,
}

/// 非阻塞提交失败。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThumbnailSubmitError {
    /// 有界队列已满；调用方等待下一次视口变化再尝试。
    Full,
    /// 加载器已经关闭。
    Closed,
}

enum ThumbnailCommand {
    /// 加载一个当前可见缩略图。
    Load(ThumbnailLoadRequest),
    /// 排在已接受请求之后的停止标记。
    Stop,
}

/// 可克隆的非阻塞请求端。
#[derive(Clone)]
pub struct ThumbnailLoaderSender {
    sender: SyncSender<ThumbnailCommand>,
    closed: Arc<AtomicBool>,
}

impl ThumbnailLoaderSender {
    /// 非阻塞提交可见缩略图请求。
    pub fn try_submit(&self, request: ThumbnailLoadRequest) -> Result<(), ThumbnailSubmitError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ThumbnailSubmitError::Closed);
        }
        self.sender
            .try_send(ThumbnailCommand::Load(request))
            .map_err(|error| match error {
                TrySendError::Full(_) => ThumbnailSubmitError::Full,
                TrySendError::Disconnected(_) => ThumbnailSubmitError::Closed,
            })
    }
}

/// 缩略图线程生命周期所有者。
pub struct ThumbnailLoader {
    sender: ThumbnailLoaderSender,
    worker: Option<JoinHandle<()>>,
}

impl ThumbnailLoader {
    /// 启动单一缩略图读取线程；emit 返回 false 后线程停止投递但继续排空已接受请求。
    pub fn start<F>(mut emit: F) -> io::Result<Self>
    where
        F: FnMut(ThumbnailLoadResult) -> bool + Send + 'static,
    {
        let (sender, receiver) = mpsc::sync_channel(THUMBNAIL_QUEUE_CAPACITY);
        let closed = Arc::new(AtomicBool::new(false));
        let worker_closed = Arc::clone(&closed);
        let worker = thread::Builder::new()
            .name("clipboard-board-thumbnail-loader".to_owned())
            .spawn(move || worker_loop(receiver, &worker_closed, &mut emit))?;
        Ok(Self {
            sender: ThumbnailLoaderSender { sender, closed },
            worker: Some(worker),
        })
    }

    /// 返回可绑定到 UI 线程的请求端。
    pub fn sender(&self) -> ThumbnailLoaderSender {
        self.sender.clone()
    }

    /// 拒绝新请求、排空已接受请求并回收线程。
    pub fn stop(mut self) -> Result<(), ThumbnailLoaderError> {
        self.sender.closed.store(true, Ordering::Release);
        let _ = self.sender.sender.send(ThumbnailCommand::Stop);
        self.worker
            .take()
            .expect("缩略图 worker 句柄只消费一次")
            .join()
            .map_err(|_| ThumbnailLoaderError::ThreadPanicked)
    }
}

impl Drop for ThumbnailLoader {
    /// 异常路径仍尽力停止并回收线程。
    fn drop(&mut self) {
        self.sender.closed.store(true, Ordering::Release);
        let _ = self.sender.sender.send(ThumbnailCommand::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 加载器生命周期错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThumbnailLoaderError {
    /// 后台线程发生 panic。
    ThreadPanicked,
}

impl fmt::Display for ThumbnailLoaderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("缩略图加载线程异常退出")
    }
}

impl std::error::Error for ThumbnailLoaderError {}

fn worker_loop<F>(receiver: Receiver<ThumbnailCommand>, closed: &AtomicBool, emit: &mut F)
where
    F: FnMut(ThumbnailLoadResult) -> bool,
{
    let mut delivery_open = true;
    while let Ok(command) = receiver.recv() {
        match command {
            ThumbnailCommand::Load(request) => {
                let result = load_thumbnail(request);
                if delivery_open && !emit(result) {
                    delivery_open = false;
                    closed.store(true, Ordering::Release);
                }
            }
            ThumbnailCommand::Stop => break,
        }
    }
}

/// 读取并解码一张受限 WebP；所有失败都压缩为不泄漏路径的固定类别。
fn load_thumbnail(request: ThumbnailLoadRequest) -> ThumbnailLoadResult {
    let outcome = load_thumbnail_pixels(&request.path);
    ThumbnailLoadResult {
        panel_generation: request.panel_generation,
        id: request.id,
        content_hash: request.content_hash,
        outcome,
    }
}

fn load_thumbnail_pixels(path: &std::path::Path) -> Result<ThumbnailPixels, ThumbnailLoadFailure> {
    if !path.is_absolute()
        || !path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("webp"))
    {
        return Err(ThumbnailLoadFailure::InvalidPath);
    }
    let metadata = fs::metadata(path).map_err(|_| ThumbnailLoadFailure::Unavailable)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_THUMBNAIL_FILE_BYTES {
        return Err(ThumbnailLoadFailure::Unavailable);
    }
    let reader = image::ImageReader::open(path)
        .map_err(|_| ThumbnailLoadFailure::Unavailable)?
        .with_guessed_format()
        .map_err(|_| ThumbnailLoadFailure::InvalidImage)?;
    if reader.format() != Some(image::ImageFormat::WebP) {
        return Err(ThumbnailLoadFailure::InvalidImage);
    }
    let mut decoder = reader
        .into_decoder()
        .map_err(|_| ThumbnailLoadFailure::InvalidImage)?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > MAX_THUMBNAIL_EDGE || height > MAX_THUMBNAIL_EDGE {
        return Err(ThumbnailLoadFailure::InvalidImage);
    }
    // 严格尺寸限制在像素分配前交给解码器；分配预算还覆盖 WebP 的少量工作缓冲。
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_THUMBNAIL_EDGE);
    limits.max_image_height = Some(MAX_THUMBNAIL_EDGE);
    limits.max_alloc = Some(8 * 1024 * 1024);
    decoder
        .set_limits(limits)
        .map_err(|_| ThumbnailLoadFailure::InvalidImage)?;
    let decoded = image::DynamicImage::from_decoder(decoder)
        .map_err(|_| ThumbnailLoadFailure::InvalidImage)?
        .to_rgba8();
    let rgba = decoded.into_raw();
    let expected = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ThumbnailLoadFailure::InvalidImage)?;
    if rgba.len() != expected {
        return Err(ThumbnailLoadFailure::InvalidImage);
    }
    Ok(ThumbnailPixels {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    //! 本组测试只覆盖缩略图线程的路径、尺寸和像素边界，不启动 Slint 事件循环。

    use std::{
        fs,
        path::PathBuf,
        sync::mpsc::sync_channel,
        time::{SystemTime, UNIX_EPOCH},
    };

    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};

    use super::{
        load_thumbnail_pixels, ThumbnailLoadFailure, ThumbnailLoadRequest, ThumbnailLoader,
    };

    /// 创建进程内唯一临时目录，测试结束时可整体回收。
    fn temporary_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时间必须晚于 Unix epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("clipboard-board-thumbnail-{nonce}"));
        fs::create_dir_all(&directory).expect("创建缩略图测试目录失败");
        directory
    }

    /// 将固定颜色测试图片编码成实际 WebP 文件。
    fn write_webp(path: &std::path::Path, width: u32, height: u32) {
        let image = RgbaImage::from_pixel(width, height, Rgba([17, 34, 51, 255]));
        DynamicImage::ImageRgba8(image)
            .save_with_format(path, ImageFormat::WebP)
            .expect("写入测试 WebP 失败");
    }

    #[test]
    fn decodes_bounded_webp_into_rgba_pixels() {
        let directory = temporary_directory();
        let path = directory.join("preview.webp");
        write_webp(&path, 12, 7);

        let pixels = load_thumbnail_pixels(&path).expect("合法缩略图应成功解码");

        assert_eq!((pixels.width, pixels.height), (12, 7));
        assert_eq!(pixels.rgba.len(), 12 * 7 * 4);
        fs::remove_dir_all(directory).expect("清理缩略图测试目录失败");
    }

    #[test]
    fn rejects_relative_and_oversized_thumbnail_inputs() {
        assert_eq!(
            load_thumbnail_pixels(std::path::Path::new("preview.webp")),
            Err(ThumbnailLoadFailure::InvalidPath)
        );
        let directory = temporary_directory();
        let path = directory.join("oversized.webp");
        write_webp(&path, 321, 1);
        assert_eq!(
            load_thumbnail_pixels(&path),
            Err(ThumbnailLoadFailure::InvalidImage)
        );
        fs::remove_dir_all(directory).expect("清理超限缩略图测试目录失败");
    }

    #[test]
    fn worker_echoes_stable_identity_with_decoded_result() {
        let directory = temporary_directory();
        let path = directory.join("worker.webp");
        write_webp(&path, 3, 2);
        let (result_sender, result_receiver) = sync_channel(1);
        let loader = ThumbnailLoader::start(move |result| result_sender.send(result).is_ok())
            .expect("启动缩略图 worker 失败");

        loader
            .sender()
            .try_submit(ThumbnailLoadRequest {
                panel_generation: 8,
                id: 19,
                content_hash: [0x44; 32],
                path,
            })
            .expect("提交缩略图请求失败");
        let result = result_receiver.recv().expect("接收缩略图结果失败");

        assert_eq!(result.panel_generation, 8);
        assert_eq!(result.id, 19);
        assert_eq!(result.content_hash, [0x44; 32]);
        assert!(result.outcome.is_ok());
        loader.stop().expect("停止缩略图 worker 失败");
        fs::remove_dir_all(directory).expect("清理 worker 测试目录失败");
    }
}

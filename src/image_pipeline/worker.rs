//! 此模块用容量一 latest-wins 邮箱把拥有型图片输入串行解码、发布并等待显式 finalize。
//!
//! worker 独占图片存储 capability，不持有剪贴板句柄、SQLite 连接或 UI 对象；发布结果
//! 在 commit/rollback 前持续冻结文件身份，停止或 finalize 发送端断开时保留未知状态资产。

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::{
    domain::{ImageAssetRootId, ImageMetadata},
    image_decode::{
        decode_dib, decode_registered_png, MAX_DIB_ENCODED_BYTES, MAX_PNG_ENCODED_BYTES,
    },
    image_storage::{ImageStorageRootKind, PreparedImageStorage},
};

use super::{
    publish::{publish_image_assets, ImagePublishError},
    PublishedImageAssets,
};

/// finalize 等待轮询间隔；既能及时 stop，又避免忙轮询。
const FINALIZE_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// 图片编码输入格式；DIBV5 与 DIB 保留来源身份但共享同一安全解析器。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageInputFormat {
    /// 注册的 PNG 剪贴板格式。
    RegisteredPng,
    /// `CF_DIBV5`。
    DibV5,
    /// `CF_DIB`。
    Dib,
}

/// 图片输入构造失败的稳定原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageInputError {
    /// 编码字节为空。
    Empty,
    /// 编码字节超过对应 PNG/DIB 固定上限。
    EncodedTooLarge,
}

impl fmt::Display for ImageInputError {
    /// 返回不包含图片字节的中文错误。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(formatter, "图片编码输入为空"),
            Self::EncodedTooLarge => write!(formatter, "图片编码输入超过固定上限"),
        }
    }
}

impl std::error::Error for ImageInputError {}

/// 已脱离剪贴板句柄的拥有型有界图片输入。
pub struct ImageInput {
    /// 决定解码器与诊断摘要的格式。
    format: ImageInputFormat,
    /// 独占编码字节；被 latest-wins 覆盖时立即释放。
    bytes: Vec<u8>,
}

impl fmt::Debug for ImageInput {
    /// Debug 只输出格式与长度，禁止泄漏编码正文。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageInput")
            .field("format", &self.format)
            .field("encoded_len", &self.bytes.len())
            .finish()
    }
}

impl ImageInput {
    /// 构造有界注册 PNG 输入。
    pub fn registered_png(bytes: Vec<u8>) -> Result<Self, ImageInputError> {
        Self::new(
            ImageInputFormat::RegisteredPng,
            bytes,
            MAX_PNG_ENCODED_BYTES,
        )
    }

    /// 构造有界 DIBV5 输入。
    pub fn dib_v5(bytes: Vec<u8>) -> Result<Self, ImageInputError> {
        Self::new(ImageInputFormat::DibV5, bytes, MAX_DIB_ENCODED_BYTES)
    }

    /// 构造有界 DIB 输入。
    pub fn dib(bytes: Vec<u8>) -> Result<Self, ImageInputError> {
        Self::new(ImageInputFormat::Dib, bytes, MAX_DIB_ENCODED_BYTES)
    }

    /// 返回输入格式，不暴露编码字节。
    pub const fn format(&self) -> ImageInputFormat {
        self.format
    }

    /// 统一验证空输入和格式大小上限。
    fn new(
        format: ImageInputFormat,
        bytes: Vec<u8>,
        maximum: usize,
    ) -> Result<Self, ImageInputError> {
        validate_input_length(bytes.len(), maximum)?;
        Ok(Self { format, bytes })
    }
}

/// 纯长度门禁让上限测试无需实际分配 30/72 MiB 输入。
fn validate_input_length(length: usize, maximum: usize) -> Result<(), ImageInputError> {
    if length == 0 {
        return Err(ImageInputError::Empty);
    }
    if length > maximum {
        return Err(ImageInputError::EncodedTooLarge);
    }
    Ok(())
}

/// 从同一次剪贴板捕获的格式中固定选择 PNG > DIBV5 > DIB。
pub fn select_image_input(
    registered_png: Option<Vec<u8>>,
    dib_v5: Option<Vec<u8>>,
    dib: Option<Vec<u8>>,
) -> Result<Option<ImageInput>, ImageInputError> {
    if let Some(bytes) = registered_png {
        ImageInput::registered_png(bytes).map(Some)
    } else if let Some(bytes) = dib_v5 {
        ImageInput::dib_v5(bytes).map(Some)
    } else if let Some(bytes) = dib {
        ImageInput::dib(bytes).map(Some)
    } else {
        Ok(None)
    }
}

/// worker 与 finalize 的稳定错误分类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageWorkerError {
    /// worker 已停止或邮箱断开。
    Disconnected,
    /// 无法创建后台线程。
    ThreadStart,
    /// 后台线程发生 panic。
    ThreadPanicked,
    /// PNG 输入无法解码。
    PngDecodeFailed,
    /// DIB/DIBV5 输入无法解码。
    DibDecodeFailed,
    /// 图片资产编码、发布或验证失败。
    PublishFailed,
    /// finalize 命令无法送达或完成回执断开。
    FinalizeDisconnected,
    /// rollback 无法完整删除本次创建的资产。
    RollbackIncomplete,
}

impl fmt::Display for ImageWorkerError {
    /// 返回不泄漏字节、路径或底层错误文本的中文描述。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => write!(formatter, "图片 worker 已停止"),
            Self::ThreadStart => write!(formatter, "无法启动图片 worker"),
            Self::ThreadPanicked => write!(formatter, "图片 worker 异常退出"),
            Self::PngDecodeFailed => write!(formatter, "PNG 图片解码失败"),
            Self::DibDecodeFailed => write!(formatter, "DIB 图片解码失败"),
            Self::PublishFailed => write!(formatter, "图片资产发布失败"),
            Self::FinalizeDisconnected => write!(formatter, "图片 finalize 通道断开"),
            Self::RollbackIncomplete => write!(formatter, "图片资产回滚不完整"),
        }
    }
}

impl std::error::Error for ImageWorkerError {}

/// 当前 worker 独占图片根的只读注册快照。
#[derive(Clone, Eq, PartialEq)]
pub struct ImageRootSnapshot {
    /// 稳定资产根身份。
    root_id: ImageAssetRootId,
    /// 默认或自定义根类型。
    root_kind: ImageStorageRootKind,
    /// Windows guard 确认的规范根路径。
    canonical_root: PathBuf,
}

impl fmt::Debug for ImageRootSnapshot {
    /// Debug 隐藏本地完整路径，只输出根身份与类型。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageRootSnapshot")
            .field("root_id", &self.root_id)
            .field("root_kind", &self.root_kind)
            .field("canonical_root", &"<redacted>")
            .finish()
    }
}

impl ImageRootSnapshot {
    /// 返回稳定资产根身份。
    pub const fn root_id(&self) -> ImageAssetRootId {
        self.root_id
    }

    /// 返回根类型。
    pub const fn root_kind(&self) -> ImageStorageRootKind {
        self.root_kind
    }

    /// 返回规范根路径，供后续同一 SQLite 事务注册。
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }
}

/// finalize 命令携带一次性完成回执；worker 收到命令后必须执行。
enum FinalizeCommand {
    /// 数据库提交成功，释放文件所有权。
    Commit(SyncSender<Result<(), ImageWorkerError>>),
    /// 数据库提交失败，删除本次创建的资产。
    Rollback(SyncSender<Result<(), ImageWorkerError>>),
}

/// 单次发布结果的 finalize 句柄；消费式 API 防止重复 commit/rollback。
pub struct ImageFinalizeHandle {
    /// 与当前 worker 等待点唯一配对的命令发送端。
    sender: SyncSender<FinalizeCommand>,
}

impl fmt::Debug for ImageFinalizeHandle {
    /// Debug 不输出通道或本地资产信息。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ImageFinalizeHandle")
    }
}

impl ImageFinalizeHandle {
    /// 确认数据库已提交并等待 worker 释放所有权。
    pub fn commit(self) -> Result<(), ImageWorkerError> {
        Self::wait_completion(self.commit_completion()?)
    }

    /// 确认数据库未提交并等待 worker 按身份回滚本次资产。
    pub fn rollback(self) -> Result<(), ImageWorkerError> {
        Self::wait_completion(self.rollback_completion()?)
    }

    /// 发送 commit 并返回一次性完成接收端；接收端被丢弃不撤销已送达命令。
    pub fn commit_completion(
        self,
    ) -> Result<Receiver<Result<(), ImageWorkerError>>, ImageWorkerError> {
        self.begin(FinalizeKind::Commit)
    }

    /// 发送 rollback 并返回一次性完成接收端；接收端被丢弃不撤销已送达命令。
    pub fn rollback_completion(
        self,
    ) -> Result<Receiver<Result<(), ImageWorkerError>>, ImageWorkerError> {
        self.begin(FinalizeKind::Rollback)
    }

    /// 构造完成通道并发送一次性 finalize。
    fn begin(
        self,
        kind: FinalizeKind,
    ) -> Result<Receiver<Result<(), ImageWorkerError>>, ImageWorkerError> {
        let (completion_sender, completion_receiver) = mpsc::sync_channel(1);
        let command = match kind {
            FinalizeKind::Commit => FinalizeCommand::Commit(completion_sender),
            FinalizeKind::Rollback => FinalizeCommand::Rollback(completion_sender),
        };
        self.sender
            .send(command)
            .map_err(|_| ImageWorkerError::FinalizeDisconnected)?;
        Ok(completion_receiver)
    }

    /// 等待同步便捷 API 的完成回执。
    fn wait_completion(
        completion_receiver: Receiver<Result<(), ImageWorkerError>>,
    ) -> Result<(), ImageWorkerError> {
        completion_receiver
            .recv()
            .map_err(|_| ImageWorkerError::FinalizeDisconnected)?
    }
}

/// finalize 的内部分支标签。
enum FinalizeKind {
    /// 保留资产。
    Commit,
    /// 回滚本次资产。
    Rollback,
}

/// worker 成功发布后交给数据库桥的元数据与一次性 finalize 句柄。
pub struct ImageWorkerResult {
    /// 已完成完整回读验证的图片元数据。
    metadata: ImageMetadata,
    /// 数据库事务结束后必须消费的 finalize 句柄。
    finalize: ImageFinalizeHandle,
}

impl fmt::Debug for ImageWorkerResult {
    /// Debug 只输出元数据摘要，不输出文件路径或图片字节。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageWorkerResult")
            .field("content_hash", &self.metadata.content_hash())
            .field("width", &self.metadata.width())
            .field("height", &self.metadata.height())
            .finish_non_exhaustive()
    }
}

impl ImageWorkerResult {
    /// 返回只读元数据供数据库事务使用。
    pub const fn metadata(&self) -> &ImageMetadata {
        &self.metadata
    }

    /// 拆出元数据和 finalize 句柄，便于事务层显式控制顺序。
    pub fn into_parts(self) -> (ImageMetadata, ImageFinalizeHandle) {
        (self.metadata, self.finalize)
    }
}

/// 容量一 latest-wins 邮箱内的单个请求。
struct ImageRequest {
    /// 拥有型有界输入。
    input: ImageInput,
    /// 被替换请求随请求 drop 自动断开响应端。
    response: SyncSender<Result<ImageWorkerResult, ImageWorkerError>>,
}

/// latest-wins 邮箱状态。
#[derive(Default)]
struct InboxState {
    /// 尚未开始的唯一最新请求。
    latest: Option<ImageRequest>,
    /// stop 后拒绝新请求并令等待线程退出。
    closed: bool,
}

/// 共享容量一邮箱；Condvar 只承担唤醒，不承载图片字节。
#[derive(Default)]
struct ImageInbox {
    /// latest 与 closed 由同一把锁保护。
    state: Mutex<InboxState>,
    /// 新请求或关闭事件唤醒 worker。
    changed: Condvar,
}

impl ImageInbox {
    /// 非阻塞替换尚未开始的旧请求。
    fn push_latest(&self, request: ImageRequest) -> Result<(), ImageWorkerError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ImageWorkerError::Disconnected)?;
        if state.closed {
            return Err(ImageWorkerError::Disconnected);
        }
        state.latest = Some(request);
        self.changed.notify_one();
        Ok(())
    }

    /// 等待并取走当前最新请求；关闭且无请求时结束线程。
    fn wait_latest(&self) -> Option<ImageRequest> {
        let mut state = self.state.lock().ok()?;
        loop {
            if let Some(request) = state.latest.take() {
                return Some(request);
            }
            if state.closed {
                return None;
            }
            state = self.changed.wait(state).ok()?;
        }
    }

    /// 关闭入口并丢弃未开始请求。
    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.latest = None;
            self.changed.notify_all();
        }
    }
}

/// 可克隆的图片请求端；提交只替换 latest 槽，不等待解码或磁盘。
#[derive(Clone)]
pub struct ImageWorkerSender {
    /// 与 worker 共享的容量一邮箱。
    inbox: Arc<ImageInbox>,
}

impl ImageWorkerSender {
    /// 提交图片并返回独立结果接收端。
    pub fn submit(
        &self,
        input: ImageInput,
    ) -> Result<Receiver<Result<ImageWorkerResult, ImageWorkerError>>, ImageWorkerError> {
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        self.inbox.push_latest(ImageRequest {
            input,
            response: response_sender,
        })?;
        Ok(response_receiver)
    }
}

/// 独立图片线程的生命周期所有者。
pub struct ImageWorker {
    /// 请求发送端；stop 时先关闭其共享邮箱。
    sender: ImageWorkerSender,
    /// 只读根注册快照。
    root_snapshot: ImageRootSnapshot,
    /// finalize 未决时 stop 也能被 worker 观察。
    stop_requested: Arc<AtomicBool>,
    /// metadata 交付后未收到 finalize 的保留计数。
    finalize_abandoned: Arc<AtomicU64>,
    /// 后台线程 join 句柄。
    join_handle: Option<JoinHandle<()>>,
}

impl ImageWorker {
    /// 启动独占已准备图片存储的后台线程。
    pub fn start(storage: PreparedImageStorage) -> Result<Self, ImageWorkerError> {
        let root_snapshot = ImageRootSnapshot {
            root_id: storage.root_id(),
            root_kind: storage.layout().root_kind(),
            canonical_root: storage.canonical_root().to_path_buf(),
        };
        let inbox = Arc::new(ImageInbox::default());
        let sender = ImageWorkerSender {
            inbox: Arc::clone(&inbox),
        };
        let stop_requested = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop_requested);
        let finalize_abandoned = Arc::new(AtomicU64::new(0));
        let worker_abandoned = Arc::clone(&finalize_abandoned);
        let join_handle = thread::Builder::new()
            .name("ImageWorker".to_owned())
            .spawn(move || worker_loop(storage, inbox, worker_stop, worker_abandoned))
            .map_err(|_| ImageWorkerError::ThreadStart)?;
        Ok(Self {
            sender,
            root_snapshot,
            stop_requested,
            finalize_abandoned,
            join_handle: Some(join_handle),
        })
    }

    /// 返回可克隆的非阻塞请求端。
    pub fn sender(&self) -> ImageWorkerSender {
        self.sender.clone()
    }

    /// 返回启动时固定的图片根注册快照。
    pub const fn root_snapshot(&self) -> &ImageRootSnapshot {
        &self.root_snapshot
    }

    /// 返回因发送端断开或 stop 而保留资产的累计次数。
    pub fn finalize_abandoned_count(&self) -> u64 {
        self.finalize_abandoned.load(Ordering::Acquire)
    }

    /// 关闭入口、唤醒 finalize 等待并 join 后台线程。
    pub fn stop(mut self) -> Result<(), ImageWorkerError> {
        self.stop_requested.store(true, Ordering::Release);
        self.sender.inbox.close();
        self.join_handle
            .take()
            .expect("ImageWorker join 句柄只消费一次")
            .join()
            .map_err(|_| ImageWorkerError::ThreadPanicked)
    }
}

impl Drop for ImageWorker {
    /// 遗忘显式 stop 时仍尽力关闭并回收线程。
    fn drop(&mut self) {
        self.stop_requested.store(true, Ordering::Release);
        self.sender.inbox.close();
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

/// 串行消费 latest 请求，发布成功后等待单次 finalize。
fn worker_loop(
    storage: PreparedImageStorage,
    inbox: Arc<ImageInbox>,
    stop_requested: Arc<AtomicBool>,
    finalize_abandoned: Arc<AtomicU64>,
) {
    while let Some(request) = inbox.wait_latest() {
        let decoded = match request.input.format {
            ImageInputFormat::RegisteredPng => decode_registered_png(&request.input.bytes)
                .map_err(|_| ImageWorkerError::PngDecodeFailed),
            ImageInputFormat::DibV5 | ImageInputFormat::Dib => {
                decode_dib(&request.input.bytes).map_err(|_| ImageWorkerError::DibDecodeFailed)
            }
        };
        let published = decoded
            .and_then(|image| publish_image_assets(&storage, image).map_err(map_publish_error));
        let published = match published {
            Ok(published) => published,
            Err(error) => {
                let _ = request.response.send(Err(error));
                continue;
            }
        };
        handle_published(request, published, &stop_requested, &finalize_abandoned);
    }
}

/// 保留“回滚不完整”独立分类，其他发布细节统一收敛为普通发布失败。
fn map_publish_error(error: ImagePublishError) -> ImageWorkerError {
    match error {
        ImagePublishError::RollbackIncomplete => ImageWorkerError::RollbackIncomplete,
        _ => ImageWorkerError::PublishFailed,
    }
}

/// 交付 metadata 并收敛 commit、rollback、断开和 stop 四种 finalize 结果。
fn handle_published(
    request: ImageRequest,
    published: PublishedImageAssets,
    stop_requested: &AtomicBool,
    finalize_abandoned: &AtomicU64,
) {
    let (finalize_sender, finalize_receiver) = mpsc::sync_channel(1);
    let result = ImageWorkerResult {
        metadata: published.metadata().clone(),
        finalize: ImageFinalizeHandle {
            sender: finalize_sender,
        },
    };
    if request.response.send(Ok(result)).is_err() {
        let _ = published.rollback_created();
        return;
    }

    loop {
        if stop_requested.load(Ordering::Acquire) {
            finalize_abandoned.fetch_add(1, Ordering::AcqRel);
            drop(published);
            return;
        }
        match finalize_receiver.recv_timeout(FINALIZE_POLL_INTERVAL) {
            Ok(FinalizeCommand::Commit(completion)) => {
                let _metadata = published.commit();
                let _ = completion.send(Ok(()));
                return;
            }
            Ok(FinalizeCommand::Rollback(completion)) => {
                let result = published
                    .rollback_created()
                    .map_err(|_| ImageWorkerError::RollbackIncomplete);
                let _ = completion.send(result);
                return;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                finalize_abandoned.fetch_add(1, Ordering::AcqRel);
                drop(published);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块定向覆盖格式优先级、latest-wins、三格式发布与 finalize 停止边界。

    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc::RecvTimeoutError,
        },
        time::{Duration, Instant},
    };

    use crate::{
        domain::CanonicalImagePixels,
        image_pipeline::encode_original_png,
        image_storage::{prepare_image_storage, ImageStoragePreference},
    };

    use super::{
        map_publish_error, select_image_input, validate_input_length, ImageInbox, ImageInput,
        ImageInputError, ImageInputFormat, ImagePublishError, ImageRequest, ImageWorker,
        ImageWorkerError,
    };

    /// 测试目录序号，避免并行用例共享资产根。
    static TEST_DIRECTORY_ID: AtomicU64 = AtomicU64::new(1);

    /// 创建当前用例独占根。
    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "clipboardboard-pipe03-{label}-{}-{}",
            std::process::id(),
            TEST_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// 清理测试根和同级恢复目录。
    fn cleanup(root: &Path) {
        let recovery = root
            .parent()
            .expect("测试根应有父目录")
            .join(".clipboardboard-recovery");
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(recovery);
    }

    /// 构造一像素规范图片。
    fn pixels() -> CanonicalImagePixels {
        CanonicalImagePixels::new(1, 1, vec![7, 8, 9, 255]).expect("构造测试像素失败")
    }

    /// 构造有效注册 PNG。
    fn png_bytes() -> Vec<u8> {
        let mut bytes = Vec::new();
        encode_original_png(&pixels(), &mut bytes).expect("编码测试 PNG 失败");
        bytes
    }

    /// 构造 1×1、32 位、顶向下 BI_RGB DIB。
    fn dib_bytes() -> Vec<u8> {
        let mut bytes = vec![0_u8; 44];
        bytes[0..4].copy_from_slice(&40_u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&1_i32.to_le_bytes());
        bytes[8..12].copy_from_slice(&(-1_i32).to_le_bytes());
        bytes[12..14].copy_from_slice(&1_u16.to_le_bytes());
        bytes[14..16].copy_from_slice(&32_u16.to_le_bytes());
        bytes[40..44].copy_from_slice(&[9, 8, 7, 255]);
        bytes
    }

    /// 轮询条件，避免用固定长 sleep 拖慢定向测试。
    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if condition() {
                return;
            }
            std::thread::yield_now();
        }
        panic!("等待 worker 状态超时");
    }

    /// 选择顺序固定，输入 Debug 不包含实际字节。
    #[test]
    fn input_priority_limits_and_debug_are_stable() {
        let input = select_image_input(Some(vec![1]), Some(vec![2]), Some(vec![3]))
            .expect("选择格式失败")
            .expect("应选择图片");
        assert_eq!(input.format(), ImageInputFormat::RegisteredPng);
        assert_eq!(
            format!("{input:?}"),
            "ImageInput { format: RegisteredPng, encoded_len: 1 }"
        );
        assert_eq!(
            ImageInput::dib(Vec::new()).expect_err("空输入应失败"),
            ImageInputError::Empty
        );
        assert_eq!(
            validate_input_length(31, 30),
            Err(ImageInputError::EncodedTooLarge)
        );
    }

    /// 邮箱尚未消费时，新请求替换旧请求并断开旧响应。
    #[test]
    fn latest_wins_replaces_pending_request() {
        let inbox = ImageInbox::default();
        let (first_sender, first_receiver) = std::sync::mpsc::sync_channel(1);
        let (second_sender, _second_receiver) = std::sync::mpsc::sync_channel(1);
        inbox
            .push_latest(ImageRequest {
                input: ImageInput::dib(vec![1]).expect("构造首请求失败"),
                response: first_sender,
            })
            .expect("提交首请求失败");
        inbox
            .push_latest(ImageRequest {
                input: ImageInput::dib(vec![2]).expect("构造新请求失败"),
                response: second_sender,
            })
            .expect("替换请求失败");
        assert!(matches!(
            first_receiver.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Disconnected)
        ));
        assert_eq!(
            inbox.wait_latest().expect("应取得最新请求").input.bytes,
            vec![2]
        );
    }

    /// PNG、DIBV5、DIB 都能走独立 worker 发布并 commit。
    #[test]
    fn worker_processes_all_three_formats() {
        for (label, input) in [
            (
                "png",
                ImageInput::registered_png(png_bytes()).expect("构造 PNG 输入失败"),
            ),
            (
                "dibv5",
                ImageInput::dib_v5(dib_bytes()).expect("构造 DIBV5 输入失败"),
            ),
            (
                "dib",
                ImageInput::dib(dib_bytes()).expect("构造 DIB 输入失败"),
            ),
        ] {
            let root = test_root(label);
            let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
                .expect("准备存储失败");
            let worker = ImageWorker::start(storage).expect("启动 worker 失败");
            assert_eq!(worker.root_snapshot().canonical_root(), root);
            assert_eq!(worker.root_snapshot().root_kind().as_str(), "custom");
            let response = worker
                .sender()
                .submit(input)
                .expect("提交图片失败")
                .recv_timeout(Duration::from_secs(2))
                .expect("等待图片结果超时")
                .expect("图片处理失败");
            let (_metadata, finalize) = response.into_parts();
            finalize.commit().expect("commit finalize 失败");
            worker.stop().expect("停止 worker 失败");
            cleanup(&root);
        }
    }

    /// 损坏 PNG 必须返回稳定解码分类，worker 随后仍能正常停止。
    #[test]
    fn invalid_png_returns_stable_decode_error() {
        let root = test_root("decode-error");
        let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
            .expect("准备存储失败");
        let worker = ImageWorker::start(storage).expect("启动 worker 失败");
        let error = worker
            .sender()
            .submit(ImageInput::registered_png(vec![1, 2, 3]).expect("构造输入失败"))
            .expect("提交失败")
            .recv_timeout(Duration::from_secs(2))
            .expect("等待错误结果超时")
            .expect_err("损坏 PNG 应失败");
        assert_eq!(error, ImageWorkerError::PngDecodeFailed);
        worker.stop().expect("停止 worker 失败");
        cleanup(&root);
    }

    /// 发布回滚不完整不能被折叠为普通失败，其他发布错误保持统一分类。
    #[test]
    fn publish_error_mapping_preserves_rollback_incomplete() {
        assert_eq!(
            map_publish_error(ImagePublishError::RollbackIncomplete),
            ImageWorkerError::RollbackIncomplete
        );
        assert_eq!(
            map_publish_error(ImagePublishError::PublishFailed),
            ImageWorkerError::PublishFailed
        );
    }

    /// rollback 删除本次资产；metadata 接收端提前断开也自动回滚。
    #[test]
    fn rollback_and_response_disconnect_remove_created_assets() {
        let root = test_root("rollback");
        let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
            .expect("准备存储失败");
        let hash = pixels().content_hash();
        let paths = storage.layout().asset_paths(&hash);
        let worker = ImageWorker::start(storage).expect("启动 worker 失败");
        let result = worker
            .sender()
            .submit(ImageInput::registered_png(png_bytes()).expect("构造输入失败"))
            .expect("提交失败")
            .recv_timeout(Duration::from_secs(2))
            .expect("等待结果超时")
            .expect("处理失败");
        let (_metadata, finalize) = result.into_parts();
        finalize.rollback().expect("rollback finalize 失败");
        assert!(!paths.image_absolute.exists());
        assert!(!paths.thumbnail_absolute.exists());

        let receiver = worker
            .sender()
            .submit(ImageInput::registered_png(png_bytes()).expect("构造输入失败"))
            .expect("再次提交失败");
        drop(receiver);
        wait_until(|| !paths.image_absolute.exists() && !paths.thumbnail_absolute.exists());
        worker.stop().expect("停止 worker 失败");
        assert!(!paths.image_absolute.exists());
        assert!(!paths.thumbnail_absolute.exists());
        cleanup(&root);
    }

    /// finalize 句柄直接丢弃时保留资产并记录 abandoned。
    #[test]
    fn dropped_finalize_preserves_assets_and_marks_abandoned() {
        let root = test_root("abandoned");
        let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
            .expect("准备存储失败");
        let paths = storage.layout().asset_paths(&pixels().content_hash());
        let worker = ImageWorker::start(storage).expect("启动 worker 失败");
        let result = worker
            .sender()
            .submit(ImageInput::registered_png(png_bytes()).expect("构造输入失败"))
            .expect("提交失败")
            .recv_timeout(Duration::from_secs(2))
            .expect("等待结果超时")
            .expect("处理失败");
        drop(result);
        wait_until(|| worker.finalize_abandoned_count() == 1);
        assert!(paths.image_absolute.exists());
        assert!(paths.thumbnail_absolute.exists());
        worker.stop().expect("停止 worker 失败");
        cleanup(&root);
    }

    /// finalize sender 存活但不发送时，stop 必须有界 join 并保留资产。
    #[test]
    fn stop_joins_while_finalize_handle_is_still_alive() {
        let root = test_root("stop-finalize");
        let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
            .expect("准备存储失败");
        let paths = storage.layout().asset_paths(&pixels().content_hash());
        let worker = ImageWorker::start(storage).expect("启动 worker 失败");
        let result = worker
            .sender()
            .submit(ImageInput::registered_png(png_bytes()).expect("构造输入失败"))
            .expect("提交失败")
            .recv_timeout(Duration::from_secs(2))
            .expect("等待结果超时")
            .expect("处理失败");
        let (_metadata, finalize) = result.into_parts();
        worker.stop().expect("stop 应有界完成");
        assert!(paths.image_absolute.exists());
        assert!(paths.thumbnail_absolute.exists());
        assert!(finalize.commit().is_err());
        cleanup(&root);
    }

    /// completion 接收端断开不撤销已经送达的 commit，资产必须保留。
    #[test]
    fn dropped_commit_completion_still_commits() {
        let root = test_root("completion-drop");
        let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
            .expect("准备存储失败");
        let paths = storage.layout().asset_paths(&pixels().content_hash());
        let worker = ImageWorker::start(storage).expect("启动 worker 失败");
        let result = worker
            .sender()
            .submit(ImageInput::registered_png(png_bytes()).expect("构造输入失败"))
            .expect("提交失败")
            .recv_timeout(Duration::from_secs(2))
            .expect("等待结果超时")
            .expect("处理失败");
        let (_metadata, finalize) = result.into_parts();
        let completion = finalize.commit_completion().expect("发送 commit 失败");
        drop(completion);
        wait_until(|| {
            fs::OpenOptions::new()
                .write(true)
                .open(&paths.image_absolute)
                .is_ok()
        });
        worker.stop().expect("停止 worker 失败");
        assert!(paths.image_absolute.exists());
        assert!(paths.thumbnail_absolute.exists());
        cleanup(&root);
    }

    /// completion 接收端断开不撤销已经送达的 rollback，资产必须删除。
    #[test]
    fn dropped_rollback_completion_still_rolls_back() {
        let root = test_root("rollback-completion-drop");
        let storage = prepare_image_storage(ImageStoragePreference::Custom(root.clone()))
            .expect("准备存储失败");
        let paths = storage.layout().asset_paths(&pixels().content_hash());
        let worker = ImageWorker::start(storage).expect("启动 worker 失败");
        let result = worker
            .sender()
            .submit(ImageInput::registered_png(png_bytes()).expect("构造输入失败"))
            .expect("提交失败")
            .recv_timeout(Duration::from_secs(2))
            .expect("等待结果超时")
            .expect("处理失败");
        let (_metadata, finalize) = result.into_parts();
        let completion = finalize.rollback_completion().expect("发送 rollback 失败");
        drop(completion);
        wait_until(|| !paths.image_absolute.exists() && !paths.thumbnail_absolute.exists());
        worker.stop().expect("停止 worker 失败");
        cleanup(&root);
    }
}

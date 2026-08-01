//! 此模块把 Windows 剪贴板捕获结果桥接到 SQLite 文本历史和 UI 事件，并处理按 ID 仅复制。
//!
//! 捕获处理顺序固定为“构造 upsert 输入 → 等待事务结果 → 转换持久化快照 → 投递 UI”，
//! 因此数据库提交失败或 DTO 不可转换时不会产生幽灵卡片；仅复制则先按 ID 读取完整正文，
//! 校验 UI 哈希后登记自身写回预期，避免重放旧选择或把写回事件重新记入历史。

use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    clipboard::{
        ClipboardCaptureInbox, ClipboardCapturePayload, ClipboardCaptureResult,
        ClipboardCopyRequest, ClipboardImageBytes, ClipboardWorkItem, ClipboardWriteError,
        ClipboardWriteExpectationStore, ClipboardWriteFormat, ClipboardWriter,
    },
    command::{UiClipboardItem, UiEvent},
    domain::ClipboardPayload,
    image_copy::{prepare_image_clipboard, ImageCopyError},
    image_decode::decode_dib,
    image_pipeline::{ImageInput, ImageRootSnapshot, ImageWorkerError, ImageWorkerSender},
    storage::{
        HistoryImageSummary, HistoryPayload, ImageUpsertInput, StorageClient, StorageError,
        TextUpsertInput, TextUpsertResult,
    },
};

/// 图片捕获协调所需的发送端和根快照；文件系统 capability 仍只归 ImageWorker 所有。
#[derive(Clone)]
pub struct ImageCaptureContext {
    /// ImageWorker 的有界捕获入口。
    sender: ImageWorkerSender,
    /// worker 启动时冻结的根注册信息。
    root: ImageRootSnapshot,
}

impl ImageCaptureContext {
    /// 从同一个 ImageWorker 的发送端与根快照构造上下文。
    pub fn new(sender: ImageWorkerSender, root: ImageRootSnapshot) -> Self {
        Self { sender, root }
    }
}

/// 单条捕获的 UI 投递状态；结果泵的退出只由 inbox 关闭且排空决定。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureProcessOutcome {
    /// 持久化成功且 UI sink 接收了事件，泵应继续等待下一条捕获。
    Posted,
    /// UI sink 返回 false；泵应禁用后续 UI 投递，但继续持久化并排空已发布 Capture。
    UiClosed,
    /// 未来非 UI 捕获类型的保留状态；当前有效文本捕获不得返回该状态。
    Skipped,
}

/// 捕获处理过程中不会携带正文的错误边界。
#[derive(Debug)]
pub enum CaptureProcessError {
    /// SQLite upsert 或执行器生命周期错误；泵记录后继续等待后续捕获。
    Storage(StorageError),
    /// 持久化事务返回了不能安全转换为 UI 卡片的 DTO。
    InvalidPersistedRecord,
    /// 捕获字节无法构造受限 ImageWorker 输入。
    InvalidImageInput,
    /// 图片 worker、解码、发布或 finalize 协议失败。
    ImagePipeline(ImageWorkerError),
    /// SQLite 失败后本次资产回滚也未完成。
    ImageStorageAndRollback {
        /// 原始存储错误。
        storage: StorageError,
        /// 回滚完成错误。
        rollback: ImageWorkerError,
    },
}

impl fmt::Display for CaptureProcessError {
    /// 输出不含剪贴板正文的错误描述，供结果泵诊断使用。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "捕获持久化失败：{error}"),
            Self::InvalidPersistedRecord => write!(formatter, "持久化结果无法转换为 UI 卡片"),
            Self::InvalidImageInput => formatter.write_str("图片捕获输入无效"),
            Self::ImagePipeline(error) => write!(formatter, "图片处理失败：{error}"),
            Self::ImageStorageAndRollback { storage, rollback } => {
                write!(
                    formatter,
                    "图片事务失败且资产回滚失败：{storage}；{rollback}"
                )
            }
        }
    }
}

impl std::error::Error for CaptureProcessError {}

impl From<StorageError> for CaptureProcessError {
    /// 将存储层错误包裹到捕获桥的业务错误边界。
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<ImageWorkerError> for CaptureProcessError {
    /// 将图片 worker 的有限错误纳入捕获桥边界。
    fn from(error: ImageWorkerError) -> Self {
        Self::ImagePipeline(error)
    }
}

/// 仅复制处理过程中不会携带正文的错误边界。
#[derive(Debug)]
pub enum CopyProcessError {
    /// 按 ID 读取 payload 时发生存储线程错误。
    Storage(StorageError),
    /// 请求的历史 ID 已被删除或不存在。
    NotFound { id: u64 },
    /// payload 类型不是当前支持的文本或图片。
    UnsupportedType,
    /// text 记录缺少完整正文。
    MissingText,
    /// image 记录缺少完整受限资产身份。
    MissingImage,
    /// 数据库主键、哈希长度或 CF_UNICODETEXT 可表示性不满足写回契约。
    InvalidPayload,
    /// UI 卡片哈希与按 ID 读取的数据库哈希不一致，拒绝旧选择写回。
    HashMismatch,
    /// 原图读取、身份复核或 DIBV5 编码失败。
    ImagePrepare(ImageCopyError),
    /// Win32 系统剪贴板写回失败。
    Write(ClipboardWriteError),
}

impl fmt::Display for CopyProcessError {
    /// 输出不含剪贴板正文的仅复制失败描述。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "仅复制读取存储失败：{error}"),
            Self::NotFound { id } => write!(formatter, "仅复制目标历史不存在：{id}"),
            Self::UnsupportedType => formatter.write_str("仅复制目标类型不受支持"),
            Self::MissingText => formatter.write_str("仅复制目标缺少文本正文"),
            Self::MissingImage => formatter.write_str("仅复制目标缺少图片资产"),
            Self::InvalidPayload => formatter.write_str("仅复制目标 payload 不满足写回契约"),
            Self::HashMismatch => formatter.write_str("仅复制目标哈希已变化"),
            Self::ImagePrepare(error) => write!(formatter, "仅复制图片准备失败：{error}"),
            Self::Write(error) => write!(formatter, "仅复制写回失败：{error}"),
        }
    }
}

impl std::error::Error for CopyProcessError {}

impl From<StorageError> for CopyProcessError {
    /// 将存储线程错误包裹到仅复制业务边界。
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<ClipboardWriteError> for CopyProcessError {
    /// 将 Win32 写回错误包裹到仅复制业务边界。
    fn from(error: ClipboardWriteError) -> Self {
        Self::Write(error)
    }
}

impl From<ImageCopyError> for CopyProcessError {
    /// 将图片文件与编码错误包裹到仅复制业务边界。
    fn from(error: ImageCopyError) -> Self {
        Self::ImagePrepare(error)
    }
}

/// 将当前系统时间转换为不会溢出 `i64` 的 Unix 毫秒时间戳。
pub fn unix_millis_now() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// 持续消费共享剪贴板邮箱，直到输入关闭且已发布 Capture 全部排空。
///
/// UI sink 一旦关闭，后续捕获仍会持久化但不再尝试投递 UI；这样进程退出期间已经被
/// ClipboardIO 接受的最后一条 Capture 不会因为事件循环先结束而丢失。
pub fn run_clipboard_pump<F>(
    inbox: ClipboardCaptureInbox,
    storage: StorageClient,
    write_expectations: ClipboardWriteExpectationStore,
    image_context: Option<ImageCaptureContext>,
    emit: F,
) where
    F: FnMut(UiEvent) -> bool,
{
    run_clipboard_pump_with_source_policy(
        inbox,
        storage,
        write_expectations,
        image_context,
        true,
        emit,
    );
}

/// 按启动时固定策略消费捕获；文本和图片在同一分派点共用来源脱敏规则。
///
/// `capture_source_app=false` 只影响历史/UI 结果，不影响 worker 读取前使用请求级
/// `ProcessSourceSnapshot` 做排除判断；运行中设置切换由后续设置原子负责。
pub fn run_clipboard_pump_with_source_policy<F>(
    inbox: ClipboardCaptureInbox,
    storage: StorageClient,
    write_expectations: ClipboardWriteExpectationStore,
    image_context: Option<ImageCaptureContext>,
    capture_source_app: bool,
    mut emit: F,
) where
    F: FnMut(UiEvent) -> bool,
{
    let mut ui_open = true;
    while let Some(work) = inbox.wait_take_work() {
        match work {
            ClipboardWorkItem::Copy(request) => {
                if let Err(error) = process_copy_request(&storage, request, &write_expectations) {
                    // 仅复制失败只输出稳定错误，不把正文写入日志或 UI。
                    eprintln!("仅复制处理失败：{error}");
                }
            }
            ClipboardWorkItem::Capture(event) => {
                let Ok(capture) = event else {
                    // sequence 失配或格式错误只丢弃本次结果，不能终止后续复制事件。
                    continue;
                };
                let copied_at = unix_millis_now();
                // 来源字段在文本/图片分派前统一清空，避免某一类型绕过静态隐私策略。
                let capture = apply_capture_source_policy(capture, capture_source_app);
                let source = capture.source.clone();
                let result = match capture.payload {
                    ClipboardCapturePayload::Text(payload) => process_capture(
                        &storage,
                        ClipboardCaptureResult {
                            sequence: capture.sequence,
                            source: source.clone(),
                            payload: ClipboardCapturePayload::Text(payload),
                        },
                        copied_at,
                        |event| ui_open && emit(event),
                    ),
                    ClipboardCapturePayload::Image(image) => {
                        if suppress_self_image_capture(
                            &write_expectations,
                            capture.sequence,
                            &image,
                            |bytes| decode_dib(bytes).ok().map(|pixels| pixels.content_hash()),
                        ) {
                            Ok(CaptureProcessOutcome::Skipped)
                        } else if let Some(context) = image_context.as_ref() {
                            process_image_capture(
                                &storage,
                                context,
                                source,
                                image,
                                copied_at,
                                |event| ui_open && emit(event),
                            )
                        } else {
                            Ok(CaptureProcessOutcome::Skipped)
                        }
                    }
                };
                match result {
                    Ok(CaptureProcessOutcome::Posted) => {}
                    Ok(CaptureProcessOutcome::UiClosed) => {
                        // UI 已停止时只关闭投递侧，存储侧继续排空已接受的 Capture。
                        ui_open = false;
                    }
                    Ok(CaptureProcessOutcome::Skipped) => {
                        // 当前有效文本不会走此分支，保留分支以兼容未来非 UI 捕获类型。
                    }
                    Err(error) => {
                        // 错误不携带正文；记录后继续处理后续捕获，避免一次失败拖垮常驻工具。
                        eprintln!("剪贴板捕获处理失败：{error}");
                    }
                }
            }
        }
    }
}

/// 应用启动时固定的来源字段策略；payload 类型不会改变该脱敏结果。
fn apply_capture_source_policy(
    mut capture: ClipboardCaptureResult,
    capture_source_app: bool,
) -> ClipboardCaptureResult {
    if !capture_source_app {
        capture.source = None;
    }
    capture
}

/// 仅对存在精确 DIBV5 候选的事件解码哈希，并在三元组匹配时一次性抑制自身写回。
fn suppress_self_image_capture<F>(
    expectations: &ClipboardWriteExpectationStore,
    sequence: u32,
    image: &ClipboardImageBytes,
    decode_hash: F,
) -> bool
where
    F: FnOnce(&[u8]) -> Option<[u8; 32]>,
{
    let ClipboardImageBytes::DibV5(bytes) = image else {
        return false;
    };
    if !expectations.has_candidate(sequence, ClipboardWriteFormat::DibV5) {
        return false;
    }
    let Some(content_hash) = decode_hash(bytes) else {
        return false;
    };
    expectations.consume_if_matches(sequence, content_hash, ClipboardWriteFormat::DibV5)
}

/// 图片经过发布验证和数据库事务后，按最终资产采用结果消费一次 finalize。
fn process_image_capture<F>(
    storage: &StorageClient,
    context: &ImageCaptureContext,
    source: Option<crate::platform::windows::ProcessSource>,
    image: ClipboardImageBytes,
    copied_at: i64,
    mut emit: F,
) -> Result<CaptureProcessOutcome, CaptureProcessError>
where
    F: FnMut(UiEvent) -> bool,
{
    let input = match image {
        ClipboardImageBytes::RegisteredPng(bytes) => ImageInput::registered_png(bytes),
        ClipboardImageBytes::DibV5(bytes) => ImageInput::dib_v5(bytes),
        ClipboardImageBytes::Dib(bytes) => ImageInput::dib(bytes),
    }
    .map_err(|_| CaptureProcessError::InvalidImageInput)?;
    let response = context.sender.submit(input)?;
    let result = response
        .recv()
        .map_err(|_| CaptureProcessError::ImagePipeline(ImageWorkerError::Disconnected))??;
    let (metadata, finalize) = result.into_parts();
    let input = ImageUpsertInput {
        metadata,
        canonical_root: context.root.canonical_root().to_path_buf(),
        root_kind: context.root.root_kind(),
        source_exe: source.as_ref().map(|value| value.executable.clone()),
        source_app: source.as_ref().map(|value| value.display_name.clone()),
        copied_at,
    };
    let upsert = match storage.upsert_image(input) {
        Ok(result) => result,
        Err(storage_error) => {
            return match finalize.rollback() {
                Ok(()) => Err(CaptureProcessError::Storage(storage_error)),
                Err(rollback) => Err(CaptureProcessError::ImageStorageAndRollback {
                    storage: storage_error,
                    rollback,
                }),
            };
        }
    };
    if upsert.adopted_published_assets {
        finalize.commit()?;
    } else {
        // 重复图片继续引用旧资产，只回滚本次发布句柄实际拥有的新文件。
        finalize.rollback()?;
    }
    let item = UiClipboardItem::from_persisted_image_result(&upsert)
        .ok_or(CaptureProcessError::InvalidPersistedRecord)?;
    if emit(UiEvent::ClipboardCaptured {
        item,
        mutation_revision: upsert.mutation_revision,
    }) {
        Ok(CaptureProcessOutcome::Posted)
    } else {
        Ok(CaptureProcessOutcome::UiClosed)
    }
}

/// 先事务性 upsert，再通过可注入 sink 投递唯一 UI 事件。
pub fn process_capture<F>(
    storage: &StorageClient,
    capture: ClipboardCaptureResult,
    copied_at: i64,
    emit: F,
) -> Result<CaptureProcessOutcome, CaptureProcessError>
where
    F: FnMut(UiEvent) -> bool,
{
    process_capture_with_upsert(capture, copied_at, |input| storage.upsert_text(input), emit)
}

/// 按 UI 提供的 ID 读取完整文本，登记自身写回预期后写入系统剪贴板。
pub fn process_copy_request(
    storage: &StorageClient,
    request: ClipboardCopyRequest,
    expectations: &ClipboardWriteExpectationStore,
) -> Result<u32, CopyProcessError> {
    let id = i64::try_from(request.id).map_err(|_| CopyProcessError::InvalidPayload)?;
    let payload = storage
        .get_history_payload(id)?
        .ok_or(CopyProcessError::NotFound { id: request.id })?;
    process_typed_copy_payload(
        &payload,
        request.content_hash,
        |text, content_hash| ClipboardWriter::write_unicode_text(text, content_hash, expectations),
        |image| {
            let prepared = prepare_image_clipboard(image)?;
            ClipboardWriter::write_dib_v5(&prepared, expectations).map_err(CopyProcessError::Write)
        },
    )
}

/// 按持久化类型分派文本或图片复制，同时保持两条路径共用 UI 请求哈希门禁。
fn process_typed_copy_payload<T, I>(
    payload: &HistoryPayload,
    expected_hash: [u8; 32],
    write_text: T,
    write_image: I,
) -> Result<u32, CopyProcessError>
where
    T: FnOnce(&str, [u8; 32]) -> Result<u32, ClipboardWriteError>,
    I: FnOnce(&HistoryImageSummary) -> Result<u32, CopyProcessError>,
{
    match payload.item_type.as_str() {
        "text" => process_copy_payload(payload, expected_hash, write_text),
        "image" => {
            if payload.id <= 0 || payload.text_content.is_some() {
                return Err(CopyProcessError::InvalidPayload);
            }
            let content_hash = <[u8; 32]>::try_from(payload.content_hash.as_slice())
                .map_err(|_| CopyProcessError::InvalidPayload)?;
            let image = payload
                .image
                .as_ref()
                .ok_or(CopyProcessError::MissingImage)?;
            if content_hash != expected_hash || content_hash != *image.metadata.content_hash() {
                return Err(CopyProcessError::HashMismatch);
            }
            write_image(image)
        }
        _ => Err(CopyProcessError::UnsupportedType),
    }
}

/// 校验文本 payload 并调用注入的 writer；抽出纯接缝便于不改写真实剪贴板的测试。
fn process_copy_payload<F>(
    payload: &HistoryPayload,
    expected_hash: [u8; 32],
    write: F,
) -> Result<u32, CopyProcessError>
where
    F: FnOnce(&str, [u8; 32]) -> Result<u32, ClipboardWriteError>,
{
    if payload.id <= 0 || payload.item_type != "text" || payload.image.is_some() {
        return Err(if payload.item_type == "text" {
            CopyProcessError::InvalidPayload
        } else {
            CopyProcessError::UnsupportedType
        });
    }
    let text = payload
        .text_content
        .as_deref()
        .ok_or(CopyProcessError::MissingText)?;
    // CF_UNICODETEXT 以第一个 NUL 作为终止符，内部 NUL 无法无损写回；必须在触碰系统
    // 剪贴板前拒绝，避免写回截断正文后生成与历史哈希不同的自身捕获。
    if text.contains('\0') {
        return Err(CopyProcessError::InvalidPayload);
    }
    let content_hash = <[u8; 32]>::try_from(payload.content_hash.as_slice())
        .map_err(|_| CopyProcessError::InvalidPayload)?;
    let recomputed_hash = ClipboardPayload::from_text(text).summary().content_hash;
    if content_hash != expected_hash || content_hash != recomputed_hash {
        return Err(CopyProcessError::HashMismatch);
    }
    write(text, content_hash).map_err(CopyProcessError::Write)
}

/// 将捕获内容转换为 upsert 输入并消费持久化结果；抽出闭包便于注入 DTO 错误测试。
fn process_capture_with_upsert<F, U>(
    capture: ClipboardCaptureResult,
    copied_at: i64,
    mut upsert: U,
    mut emit: F,
) -> Result<CaptureProcessOutcome, CaptureProcessError>
where
    F: FnMut(UiEvent) -> bool,
    U: FnMut(TextUpsertInput) -> Result<TextUpsertResult, StorageError>,
{
    let ClipboardCapturePayload::Text(payload) = capture.payload else {
        return Ok(CaptureProcessOutcome::Skipped);
    };
    let summary = payload.summary();
    let input = TextUpsertInput {
        content_hash: summary.content_hash,
        text_content: payload.as_text().to_owned(),
        preview_text: summary.preview,
        source_exe: capture
            .source
            .as_ref()
            .map(|source| source.executable.clone()),
        source_app: capture
            .source
            .as_ref()
            .map(|source| source.display_name.clone()),
        copied_at,
    };

    let result = upsert(input)?;
    let item = UiClipboardItem::from_persisted_result(&result)
        .ok_or(CaptureProcessError::InvalidPersistedRecord)?;
    if emit(UiEvent::ClipboardCaptured {
        item,
        mutation_revision: result.mutation_revision,
    }) {
        Ok(CaptureProcessOutcome::Posted)
    } else {
        Ok(CaptureProcessOutcome::UiClosed)
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块覆盖捕获提交顺序、存储失败续处理、DTO 防伪和 UI sink 生命周期。

    use std::{
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc::sync_channel,
        },
        thread,
    };

    use rusqlite::{params, Connection};

    use super::{
        apply_capture_source_policy, process_capture, process_capture_with_upsert,
        process_copy_payload, process_image_capture, process_typed_copy_payload,
        run_clipboard_pump, suppress_self_image_capture, CaptureProcessError,
        CaptureProcessOutcome, CopyProcessError, ImageCaptureContext,
    };
    use crate::{
        clipboard::{
            ClipboardCaptureInbox, ClipboardCapturePayload, ClipboardCaptureResult,
            ClipboardImageBytes, ClipboardWriteError, ClipboardWriteExpectationStore,
            ClipboardWriteFormat,
        },
        command::{UiClipboardItemKind, UiEvent},
        domain::{CanonicalImagePixels, ClipboardPayload, ImageAssetRootId, ImageMetadata},
        history_restore::load_startup_snapshot,
        image_pipeline::{encode_original_png, ImageWorker},
        image_storage::{
            parse_image_storage_preference, prepare_image_storage, windows_path_eq,
            ImageStoragePreference,
        },
        platform::windows::ProcessSource,
        storage::{HistoryImageSummary, HistoryPayload, StorageExecutor, TextUpsertResult},
    };

    /// 创建独立临时目录，避免并行测试共享生产数据库或互相污染触发器。
    fn test_directory(label: &str) -> std::path::PathBuf {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!("clipboard-board-18a-{label}-{id}"));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("创建捕获测试目录失败");
        directory
    }

    /// 构造带来源的文本捕获，统一测试正文、来源和哈希的一致性。
    fn capture(sequence: u32, text: &str) -> ClipboardCaptureResult {
        ClipboardCaptureResult {
            sequence,
            source: Some(ProcessSource {
                executable: "editor.exe".to_owned(),
                display_name: "编辑器".to_owned(),
                process_id: 10,
            }),
            payload: ClipboardPayload::from_text(text).into(),
        }
    }

    /// 静态来源策略必须同时覆盖文本和图片，关闭时只保留 payload 不保留来源字段。
    #[test]
    fn 来源静态脱敏同时覆盖文本和图片() {
        let text = apply_capture_source_policy(capture(1, "正文"), false);
        assert!(text.source.is_none());

        let image = apply_capture_source_policy(
            ClipboardCaptureResult {
                sequence: 2,
                source: capture(2, "占位").source,
                payload: ClipboardCapturePayload::Image(ClipboardImageBytes::RegisteredPng(vec![
                    1, 2, 3,
                ])),
            },
            false,
        );
        assert!(image.source.is_none());
        assert!(matches!(image.payload, ClipboardCapturePayload::Image(_)));

        let retained = apply_capture_source_policy(capture(3, "保留来源"), true);
        assert_eq!(
            retained
                .source
                .as_ref()
                .map(|source| source.executable.as_str()),
            Some("editor.exe")
        );
    }

    /// 构造一张 1×1、顶向下 BGRA 的最小合法 DIBV5，供真实解码抑制接缝使用。
    fn one_pixel_dib_v5() -> Vec<u8> {
        let mut dib = vec![0_u8; 128];
        dib[0..4].copy_from_slice(&124_u32.to_le_bytes());
        dib[4..8].copy_from_slice(&1_i32.to_le_bytes());
        dib[8..12].copy_from_slice(&(-1_i32).to_le_bytes());
        dib[12..14].copy_from_slice(&1_u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32_u16.to_le_bytes());
        dib[16..20].copy_from_slice(&3_u32.to_le_bytes());
        dib[20..24].copy_from_slice(&4_u32.to_le_bytes());
        dib[40..44].copy_from_slice(&0x00ff_0000_u32.to_le_bytes());
        dib[44..48].copy_from_slice(&0x0000_ff00_u32.to_le_bytes());
        dib[48..52].copy_from_slice(&0x0000_00ff_u32.to_le_bytes());
        dib[52..56].copy_from_slice(&0xff00_0000_u32.to_le_bytes());
        dib[124..128].copy_from_slice(&[3, 2, 1, 255]);
        dib
    }

    /// IMG-HIST-03 激活前，图片捕获必须安全跳过且不得调用文本 upsert 或投递 UI。
    #[test]
    fn image_capture_is_skipped_without_text_persistence() {
        let capture = ClipboardCaptureResult {
            sequence: 99,
            source: None,
            payload: ClipboardCapturePayload::Image(ClipboardImageBytes::RegisteredPng(vec![
                1, 2, 3,
            ])),
        };
        let mut upsert_calls = 0;
        let mut emit_calls = 0;
        let outcome = process_capture_with_upsert(
            capture,
            100,
            |_| {
                upsert_calls += 1;
                Err(crate::storage::StorageError::ChannelClosed)
            },
            |_| {
                emit_calls += 1;
                true
            },
        )
        .expect("图片应安全跳过");

        assert_eq!(outcome, CaptureProcessOutcome::Skipped);
        assert_eq!(upsert_calls, 0);
        assert_eq!(emit_calls, 0);
    }

    /// 没有候选或格式不符时不得调用抑制解码器，避免普通图片重复解码。
    #[test]
    fn 普通图片无候选时不执行抑制解码() {
        let expectations = ClipboardWriteExpectationStore::new();
        let image = ClipboardImageBytes::DibV5(one_pixel_dib_v5());
        let mut decode_calls = 0;
        assert!(!suppress_self_image_capture(
            &expectations,
            90,
            &image,
            |_| {
                decode_calls += 1;
                None
            },
        ));
        assert_eq!(decode_calls, 0);

        let png = ClipboardImageBytes::RegisteredPng(vec![1, 2, 3]);
        assert!(!suppress_self_image_capture(
            &expectations,
            90,
            &png,
            |_| {
                decode_calls += 1;
                None
            },
        ));
        assert_eq!(decode_calls, 0);
    }

    /// 候选哈希不匹配不能消费；随后同一三元组匹配时只消费一次。
    #[test]
    fn 图片自身预期按三元组一次性消费() {
        let expectations = ClipboardWriteExpectationStore::new();
        let token = expectations
            .arm([21; 32], ClipboardWriteFormat::DibV5)
            .expect("登记图片自身预期失败");
        expectations.bind_sequence(token, 91);
        let image = ClipboardImageBytes::DibV5(one_pixel_dib_v5());

        assert!(!suppress_self_image_capture(
            &expectations,
            91,
            &image,
            |_| Some([22; 32]),
        ));
        assert!(expectations.has_candidate(91, ClipboardWriteFormat::DibV5));
        assert!(suppress_self_image_capture(
            &expectations,
            91,
            &image,
            |_| Some([21; 32]),
        ));
        assert!(!suppress_self_image_capture(
            &expectations,
            91,
            &image,
            |_| panic!("已消费预期不能再次解码"),
        ));
    }

    /// pump 必须在真实图片 worker 前抑制自身 DIBV5，且不发布资产、写数据库或投递 UI。
    #[test]
    fn 自身_dibv5_在图片流水线前被抑制() {
        let directory = test_directory("suppress-self-dibv5");
        let image_root = directory.join("images");
        let storage = StorageExecutor::open_at(&directory).expect("启动自身抑制测试存储失败");
        let worker = ImageWorker::start(
            prepare_image_storage(ImageStoragePreference::Custom(image_root.clone()))
                .expect("准备自身抑制图片目录失败"),
        )
        .expect("启动自身抑制图片 worker 失败");
        let context = ImageCaptureContext::new(worker.sender(), worker.root_snapshot().clone());
        let inbox = ClipboardCaptureInbox::new();
        let expectations = ClipboardWriteExpectationStore::new();
        let hash = CanonicalImagePixels::new(1, 1, vec![1, 2, 3, 255])
            .expect("构造自身抑制规范像素失败")
            .content_hash();
        let token = expectations
            .arm(hash, ClipboardWriteFormat::DibV5)
            .expect("登记自身 DIBV5 预期失败");
        expectations.bind_sequence(token, 92);
        inbox.publish(Ok(ClipboardCaptureResult {
            sequence: 92,
            source: None,
            payload: ClipboardCapturePayload::Image(ClipboardImageBytes::DibV5(one_pixel_dib_v5())),
        }));
        inbox.close();
        let mut emit_calls = 0;

        run_clipboard_pump(
            inbox,
            storage.client(),
            expectations.clone(),
            Some(context),
            |_| {
                emit_calls += 1;
                true
            },
        );

        assert_eq!(emit_calls, 0);
        assert!(!expectations.has_candidate(92, ClipboardWriteFormat::DibV5));
        assert!(storage
            .list_history_summaries(None, 30)
            .expect("查询自身抑制历史失败")
            .items
            .is_empty());
        assert!(std::fs::read_dir(image_root.join("original"))
            .expect("读取自身抑制原图目录失败")
            .next()
            .is_none());
        assert!(std::fs::read_dir(image_root.join("thumbnail"))
            .expect("读取自身抑制缩略图目录失败")
            .next()
            .is_none());
        worker.stop().expect("停止自身抑制图片 worker 失败");
        drop(storage);
        std::fs::remove_dir_all(directory).expect("清理自身抑制测试目录失败");
    }

    /// sequence、哈希或实际格式不匹配时，pump 必须继续发布图片并更新 SQLite。
    #[test]
    fn 错误图片预期不会阻止真实捕获发布() {
        let directory = test_directory("publish-mismatched-expectation");
        let image_root = directory.join("images");
        let storage = StorageExecutor::open_at(&directory).expect("启动错配预期测试存储失败");
        let worker = ImageWorker::start(
            prepare_image_storage(ImageStoragePreference::Custom(image_root))
                .expect("准备错配预期图片目录失败"),
        )
        .expect("启动错配预期图片 worker 失败");
        let context = ImageCaptureContext::new(worker.sender(), worker.root_snapshot().clone());
        let inbox = ClipboardCaptureInbox::new();
        let expectations = ClipboardWriteExpectationStore::new();
        let pixels =
            CanonicalImagePixels::new(1, 1, vec![1, 2, 3, 255]).expect("构造错配预期规范像素失败");
        let hash = pixels.content_hash();
        let dib = one_pixel_dib_v5();
        let mut png = Vec::new();
        encode_original_png(&pixels, &mut png).expect("编码错配预期 PNG 失败");

        for (expected_sequence, expected_hash) in
            [(200, hash), (202, [99; 32]), (203, hash), (204, hash)]
        {
            let token = expectations
                .arm(expected_hash, ClipboardWriteFormat::DibV5)
                .expect("登记错配图片预期失败");
            expectations.bind_sequence(token, expected_sequence);
        }
        let pump_inbox = inbox.clone();
        let pump_storage = storage.client();
        let (event_sender, event_receiver) = sync_channel(4);
        let pump = thread::spawn(move || {
            run_clipboard_pump(
                pump_inbox,
                pump_storage,
                expectations,
                Some(context),
                |event| event_sender.send(event).is_ok(),
            );
        });
        let mut events = Vec::new();
        for capture in [
            ClipboardCaptureResult {
                sequence: 201,
                source: None,
                payload: ClipboardCapturePayload::Image(ClipboardImageBytes::DibV5(dib.clone())),
            },
            ClipboardCaptureResult {
                sequence: 202,
                source: None,
                payload: ClipboardCapturePayload::Image(ClipboardImageBytes::DibV5(dib.clone())),
            },
            ClipboardCaptureResult {
                sequence: 203,
                source: None,
                payload: ClipboardCapturePayload::Image(ClipboardImageBytes::RegisteredPng(png)),
            },
            ClipboardCaptureResult {
                sequence: 204,
                source: None,
                payload: ClipboardCapturePayload::Image(ClipboardImageBytes::Dib(dib)),
            },
        ] {
            inbox.publish(Ok(capture));
            events.push(
                event_receiver
                    .recv()
                    .expect("错配图片捕获未经过真实流水线投递"),
            );
        }
        inbox.close();
        pump.join().expect("错配图片结果泵异常退出");

        assert_eq!(events.len(), 4);
        let page = storage
            .list_history_summaries(None, 30)
            .expect("查询错配预期历史失败");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].copy_count, 4);
        worker.stop().expect("停止错配预期图片 worker 失败");
        drop(storage);
        std::fs::remove_dir_all(directory).expect("清理错配预期测试目录失败");
    }

    /// 图片文件发布、数据库提交和 finalize 必须形成可重复去重的完整顺序。
    #[test]
    fn image_capture_persists_then_posts_typed_ui_event() {
        let directory = test_directory("image-persist");
        let image_root = directory.join("images");
        let storage = StorageExecutor::open_at(&directory).expect("启动图片事务存储失败");
        let prepared = prepare_image_storage(ImageStoragePreference::Custom(image_root.clone()))
            .expect("准备图片测试目录失败");
        let worker = ImageWorker::start(prepared).expect("启动图片 worker 失败");
        let context = ImageCaptureContext::new(worker.sender(), worker.root_snapshot().clone());
        let pixels =
            CanonicalImagePixels::new(1, 1, vec![10, 20, 30, 255]).expect("构造测试像素失败");
        let mut png = Vec::new();
        encode_original_png(&pixels, &mut png).expect("编码测试 PNG 失败");

        let mut events = Vec::new();
        let first = process_image_capture(
            &storage.client(),
            &context,
            None,
            ClipboardImageBytes::RegisteredPng(png.clone()),
            100,
            |event| {
                events.push(event);
                true
            },
        )
        .expect("首次图片捕获失败");
        let duplicate = process_image_capture(
            &storage.client(),
            &context,
            None,
            ClipboardImageBytes::RegisteredPng(png),
            200,
            |event| {
                events.push(event);
                true
            },
        )
        .expect("重复图片捕获失败");
        assert_eq!(first, CaptureProcessOutcome::Posted);
        assert_eq!(duplicate, CaptureProcessOutcome::Posted);
        assert_eq!(events.len(), 2);
        for event in &events {
            let UiEvent::ClipboardCaptured { item, .. } = event else {
                panic!("图片捕获只能投递 ClipboardCaptured");
            };
            assert!(matches!(item.kind, UiClipboardItemKind::Image(_)));
            assert!(item.copy_enabled());
        }

        let connection =
            Connection::open(directory.join("clipboard.db")).expect("打开图片事务数据库失败");
        let (copy_count, image_path, thumbnail_path): (i64, String, String) = connection
            .query_row(
                "SELECT copy_count, image_path, thumbnail_path FROM clipboard_items WHERE item_type = 'image'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("读取图片事务结果失败");
        assert_eq!(copy_count, 2);
        assert!(image_root.join("original").join(image_path).is_file());
        assert!(image_root.join("thumbnail").join(thumbnail_path).is_file());

        drop(connection);
        worker.stop().expect("停止图片 worker 失败");
        drop(storage);
        std::fs::remove_dir_all(directory).expect("清理图片事务目录失败");
    }

    /// 跨根重复图片必须继续定位 SQLite 最终保留的旧资产根，不能拼接当前 worker 根。
    #[test]
    fn duplicate_image_event_uses_persisted_asset_root() {
        let directory = test_directory("image-cross-root");
        let root_a = directory.join("images-a");
        let root_b = directory.join("images-b");
        let storage = StorageExecutor::open_at(&directory).expect("启动跨根图片存储失败");
        let pixels =
            CanonicalImagePixels::new(1, 1, vec![30, 40, 50, 255]).expect("构造跨根像素失败");
        let mut png = Vec::new();
        encode_original_png(&pixels, &mut png).expect("编码跨根 PNG 失败");

        let worker_a = ImageWorker::start(
            prepare_image_storage(ImageStoragePreference::Custom(root_a.clone()))
                .expect("准备根 A 失败"),
        )
        .expect("启动根 A worker 失败");
        let context_a =
            ImageCaptureContext::new(worker_a.sender(), worker_a.root_snapshot().clone());
        process_image_capture(
            &storage.client(),
            &context_a,
            None,
            ClipboardImageBytes::RegisteredPng(png.clone()),
            100,
            |_| true,
        )
        .expect("根 A 首次捕获失败");
        worker_a.stop().expect("停止根 A worker 失败");

        let worker_b = ImageWorker::start(
            prepare_image_storage(ImageStoragePreference::Custom(root_b.clone()))
                .expect("准备根 B 失败"),
        )
        .expect("启动根 B worker 失败");
        let context_b =
            ImageCaptureContext::new(worker_b.sender(), worker_b.root_snapshot().clone());
        let mut event = None;
        process_image_capture(
            &storage.client(),
            &context_b,
            None,
            ClipboardImageBytes::RegisteredPng(png),
            200,
            |posted| {
                event = Some(posted);
                true
            },
        )
        .expect("根 B 重复捕获失败");
        let Some(UiEvent::ClipboardCaptured { item, .. }) = event else {
            panic!("跨根重复捕获缺少 UI 事件");
        };
        let UiClipboardItemKind::Image(image) = item.kind else {
            panic!("跨根重复结果应为图片");
        };
        assert!(image.thumbnail_path.starts_with(root_a.join("thumbnail")));
        assert!(!image.thumbnail_path.starts_with(root_b.join("thumbnail")));

        worker_b.stop().expect("停止根 B worker 失败");
        drop(storage);
        std::fs::remove_dir_all(directory).expect("清理跨根测试目录失败");
    }

    /// 配置切换到 B 后，新图片写入 B；启动历史仍从 SQLite 根注册表恢复 A 的旧缩略图。
    #[test]
    fn switched_image_root_keeps_old_assets_readable() {
        let directory = test_directory("image-root-switch");
        let root_a = directory.join("images-a");
        let root_b = directory.join("images-b");
        let preference_a =
            parse_image_storage_preference(Some(root_a.to_str().expect("根 A 必须是 UTF-8")))
                .expect("解析根 A 设置失败");
        let preference_b =
            parse_image_storage_preference(Some(root_b.to_str().expect("根 B 必须是 UTF-8")))
                .expect("解析根 B 设置失败");
        let mut storage = StorageExecutor::open_at(&directory).expect("启动切换根存储失败");

        let pixels_a =
            CanonicalImagePixels::new(1, 1, vec![30, 40, 50, 255]).expect("构造根 A 像素失败");
        let mut png_a = Vec::new();
        encode_original_png(&pixels_a, &mut png_a).expect("编码根 A PNG 失败");
        let worker_a =
            ImageWorker::start(prepare_image_storage(preference_a).expect("准备根 A 图片目录失败"))
                .expect("启动根 A 图片 worker 失败");
        let context_a =
            ImageCaptureContext::new(worker_a.sender(), worker_a.root_snapshot().clone());
        process_image_capture(
            &storage.client(),
            &context_a,
            None,
            ClipboardImageBytes::RegisteredPng(png_a),
            100,
            |_| true,
        )
        .expect("根 A 图片捕获失败");
        worker_a.stop().expect("停止根 A 图片 worker 失败");

        let pixels_b =
            CanonicalImagePixels::new(1, 1, vec![80, 90, 100, 255]).expect("构造根 B 像素失败");
        let mut png_b = Vec::new();
        encode_original_png(&pixels_b, &mut png_b).expect("编码根 B PNG 失败");
        let worker_b =
            ImageWorker::start(prepare_image_storage(preference_b).expect("准备根 B 图片目录失败"))
                .expect("启动根 B 图片 worker 失败");
        let context_b =
            ImageCaptureContext::new(worker_b.sender(), worker_b.root_snapshot().clone());
        process_image_capture(
            &storage.client(),
            &context_b,
            None,
            ClipboardImageBytes::RegisteredPng(png_b),
            200,
            |_| true,
        )
        .expect("根 B 图片捕获失败");
        worker_b.stop().expect("停止根 B 图片 worker 失败");

        let page = storage
            .client()
            .list_history_summaries(None, 10)
            .expect("查询切换根历史失败");
        assert_eq!(page.items.len(), 2);
        let summary_a = page
            .items
            .iter()
            .find(|item| item.content_hash == pixels_a.content_hash())
            .expect("缺少根 A 历史");
        let summary_b = page
            .items
            .iter()
            .find(|item| item.content_hash == pixels_b.content_hash())
            .expect("缺少根 B 历史");
        let image_a = summary_a.image.as_ref().expect("根 A 历史缺少图片元数据");
        let image_b = summary_b.image.as_ref().expect("根 B 历史缺少图片元数据");
        assert!(windows_path_eq(image_a.canonical_root.as_path(), &root_a));
        assert!(windows_path_eq(image_b.canonical_root.as_path(), &root_b));
        assert!(image_a.thumbnail_absolute_path().is_file());
        assert!(image_b.thumbnail_absolute_path().is_file());
        assert!(image_a
            .canonical_root
            .join("original")
            .join(image_a.metadata.image_path().as_path())
            .is_file());
        assert!(image_b
            .canonical_root
            .join("original")
            .join(image_b.metadata.image_path().as_path())
            .is_file());

        let connection =
            Connection::open(directory.join("clipboard.db")).expect("打开切换根数据库失败");
        let registered_roots = connection
            .prepare("SELECT root_path FROM image_asset_roots")
            .expect("查询图片根注册表失败")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("读取图片根注册表失败")
            .map(|row| PathBuf::from(row.expect("读取图片根路径失败")))
            .collect::<Vec<_>>();
        assert_eq!(registered_roots.len(), 2);
        assert!(registered_roots
            .iter()
            .any(|root| windows_path_eq(root, &root_a)));
        assert!(registered_roots
            .iter()
            .any(|root| windows_path_eq(root, &root_b)));
        drop(connection);

        let startup_snapshot = load_startup_snapshot(&mut storage).expect("恢复切换根启动历史失败");
        let restored_a = startup_snapshot
            .items
            .iter()
            .find(|item| item.content_hash == pixels_a.content_hash())
            .expect("启动历史缺少根 A 图片");
        let UiClipboardItemKind::Image(restored_image_a) = &restored_a.kind else {
            panic!("根 A 启动历史类型不是图片");
        };
        assert!(windows_path_eq(
            &restored_image_a.thumbnail_path,
            &image_a.thumbnail_absolute_path()
        ));
        assert!(windows_path_eq(
            &restored_image_a.thumbnail_path,
            &root_a
                .join("thumbnail")
                .join(image_a.metadata.thumbnail_path().as_path())
        ));

        drop(storage);
        std::fs::remove_dir_all(directory).expect("清理切换根测试目录失败");
    }

    /// SQLite 拒绝图片事务时必须 rollback 本次刚发布的两份资产。
    #[test]
    fn image_storage_failure_rolls_back_published_assets() {
        let directory = test_directory("image-rollback");
        let image_root = directory.join("images");
        let storage = StorageExecutor::open_at(&directory).expect("启动图片回滚存储失败");
        let connection =
            Connection::open(directory.join("clipboard.db")).expect("打开故障注入数据库失败");
        connection
            .execute_batch(
                "CREATE TRIGGER reject_image_insert BEFORE INSERT ON clipboard_items
                 WHEN NEW.item_type = 'image' BEGIN SELECT RAISE(ABORT, 'reject'); END;",
            )
            .expect("创建图片拒绝触发器失败");
        drop(connection);
        let prepared = prepare_image_storage(ImageStoragePreference::Custom(image_root.clone()))
            .expect("准备图片回滚目录失败");
        let worker = ImageWorker::start(prepared).expect("启动图片回滚 worker 失败");
        let context = ImageCaptureContext::new(worker.sender(), worker.root_snapshot().clone());
        let pixels = CanonicalImagePixels::new(1, 1, vec![1, 2, 3, 255]).expect("构造回滚像素失败");
        let mut png = Vec::new();
        encode_original_png(&pixels, &mut png).expect("编码回滚 PNG 失败");

        let error = process_image_capture(
            &storage.client(),
            &context,
            None,
            ClipboardImageBytes::RegisteredPng(png),
            100,
            |_| true,
        )
        .expect_err("SQLite 故障应拒绝图片捕获");
        assert!(matches!(error, CaptureProcessError::Storage(_)));
        let original_files = std::fs::read_dir(image_root.join("original"))
            .expect("读取原图目录失败")
            .flat_map(|entry| {
                std::fs::read_dir(entry.expect("读取原图分片失败").path())
                    .expect("读取原图分片目录失败")
            })
            .count();
        let thumbnail_files = std::fs::read_dir(image_root.join("thumbnail"))
            .expect("读取缩略图目录失败")
            .flat_map(|entry| {
                std::fs::read_dir(entry.expect("读取缩略图分片失败").path())
                    .expect("读取缩略图分片目录失败")
            })
            .count();
        assert_eq!(original_files, 0);
        assert_eq!(thumbnail_files, 0);

        worker.stop().expect("停止图片回滚 worker 失败");
        drop(storage);
        std::fs::remove_dir_all(directory).expect("清理图片回滚目录失败");
    }

    /// UI 在首条投递时关闭，也必须继续排空停止期间已经发布的后续 Capture。
    #[test]
    fn ui_关闭后结果泵仍排空已发布_capture() {
        let directory = test_directory("drain-after-ui-close");
        let storage = StorageExecutor::open_at(&directory).expect("启动测试存储失败");
        let inbox = ClipboardCaptureInbox::new();
        inbox.publish(Ok(capture(1, "在途首条")));
        let pump_inbox = inbox.clone();
        let pump_storage = storage.client();
        let (sink_entered_sender, sink_entered_receiver) = sync_channel(1);
        let (sink_release_sender, sink_release_receiver) = sync_channel(1);

        let pump = thread::spawn(move || {
            run_clipboard_pump(
                pump_inbox,
                pump_storage,
                ClipboardWriteExpectationStore::new(),
                None,
                |_event| {
                    sink_entered_sender.send(()).expect("通知 UI sink 进入失败");
                    sink_release_receiver.recv().expect("等待 UI sink 释放失败");
                    false
                },
            );
        });

        sink_entered_receiver.recv().expect("首条未进入 UI sink");
        inbox.publish(Ok(capture(2, "停止期尾条")));
        inbox.close();
        sink_release_sender.send(()).expect("释放 UI sink 失败");
        pump.join().expect("结果泵线程异常退出");

        let page = storage
            .list_history_summaries(None, 30)
            .expect("读取排空后的历史失败");
        let previews = page
            .items
            .iter()
            .map(|item| item.preview_text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(previews.len(), 2);
        assert!(previews.contains(&"在途首条"));
        assert!(previews.contains(&"停止期尾条"));

        drop(storage);
        std::fs::remove_dir_all(directory).expect("清理排空测试目录失败");
    }

    /// 成功路径必须先观察数据库记录，再收到带持久化 ID 的 UI 事件。
    #[test]
    fn 成功路径先提交再投递() {
        let directory = test_directory("success");
        let storage = StorageExecutor::open_at(&directory).expect("启动测试存储失败");
        let mut events = Vec::new();

        let outcome = process_capture(&storage.client(), capture(1, "首条文本"), 123, |event| {
            events.push(event);
            true
        })
        .expect("成功捕获应完成处理");

        assert_eq!(outcome, CaptureProcessOutcome::Posted);
        assert_eq!(events.len(), 1);
        let UiEvent::ClipboardCaptured {
            item,
            mutation_revision,
        } = &events[0]
        else {
            panic!("成功捕获必须投递 ClipboardCaptured");
        };
        assert_eq!(*mutation_revision, 1);
        let payload = storage
            .get_history_payload(item.id as i64)
            .expect("成功 upsert 后应可读取记录")
            .expect("成功 upsert 后 ID 必须存在");
        assert_eq!(payload.text_content.as_deref(), Some("首条文本"));
        assert_eq!(item.id, 1);
        assert_eq!(item.source, "编辑器");
        assert_eq!(item.preview, "首条文本");
        assert_eq!(item.copy_count, 1);
        assert!(!item.is_pinned);
        assert_eq!(item.relative_time, "刚刚");
    }

    /// upsert 错误不能投递 UI；同一个执行器随后仍必须处理新的哈希。
    #[test]
    fn 存储失败后同一执行器继续处理新哈希() {
        let directory = test_directory("storage-error");
        let storage = StorageExecutor::open_at(&directory).expect("启动测试存储失败");
        let hash = ClipboardPayload::from_text("旧正文").summary().content_hash;
        {
            let connection = Connection::open(storage.database_path()).expect("打开注入连接失败");
            connection
                .execute(
                    "INSERT INTO clipboard_items (item_type, text_content, preview_text, content_hash, source_exe, source_app, copy_count, is_pinned, created_at, copied_at, last_used_at) VALUES ('text', '旧正文', '旧预览', ?1, 'old.exe', '旧应用', 1, 0, 1, 1, NULL)",
                    params![hash.as_slice()],
                )
                .expect("预置重复记录失败");
            connection
                .execute(
                    "CREATE TRIGGER fail_capture_update BEFORE UPDATE OF copied_at ON clipboard_items BEGIN SELECT RAISE(ABORT, 'capture update blocked'); END",
                    [],
                )
                .expect("创建失败触发器失败");
        }

        let mut events = Vec::new();
        let failed = process_capture(&storage.client(), capture(2, "旧正文"), 2, |event| {
            events.push(event);
            true
        });
        assert!(matches!(failed, Err(CaptureProcessError::Storage(_))));
        assert!(events.is_empty());
        assert!(storage.status().is_ok());

        let succeeded = process_capture(&storage.client(), capture(3, "新正文"), 3, |event| {
            events.push(event);
            true
        })
        .expect("新哈希应绕过失败触发器");
        assert_eq!(succeeded, CaptureProcessOutcome::Posted);
        assert_eq!(events.len(), 1);
    }

    /// 不可转换的持久化 DTO 必须返回专门错误且不调用 sink。
    #[test]
    fn dto_转换失败不投递事件() {
        let mut events = Vec::new();
        let result = process_capture_with_upsert(
            capture(4, "DTO 错误"),
            4,
            |_input| {
                Ok(TextUpsertResult {
                    mutation_revision: 1,
                    id: -1,
                    content_hash: [2; 32],
                    preview_text: "DTO 错误".to_owned(),
                    source_exe: None,
                    source_app: None,
                    copy_count: 1,
                    is_pinned: false,
                    created_at: 4,
                    copied_at: 4,
                    last_used_at: None,
                })
            },
            |event| {
                events.push(event);
                true
            },
        );

        assert!(matches!(
            result,
            Err(CaptureProcessError::InvalidPersistedRecord)
        ));
        assert!(events.is_empty());
    }

    /// sink 返回 false 时必须返回 UiClosed，结果泵据此只关闭 UI 投递侧。
    #[test]
    fn sink_关闭返回_ui_closed() {
        let directory = test_directory("ui-closed");
        let storage = StorageExecutor::open_at(&directory).expect("启动测试存储失败");
        let outcome = process_capture(&storage.client(), capture(5, "UI 关闭"), 5, |_event| {
            false
        })
        .expect("sink 关闭不是存储错误");

        assert_eq!(outcome, CaptureProcessOutcome::UiClosed);
    }

    /// 当前输入域只有有效文本，成功处理不得返回未来保留的 Skipped 状态。
    #[test]
    fn 有效文本不会返回_skipped() {
        let directory = test_directory("not-skipped");
        let storage = StorageExecutor::open_at(&directory).expect("启动测试存储失败");
        let outcome = process_capture(&storage.client(), capture(6, "不可跳过"), 6, |_event| {
            true
        })
        .expect("有效文本应成功处理");

        assert_eq!(outcome, CaptureProcessOutcome::Posted);
        assert_ne!(outcome, CaptureProcessOutcome::Skipped);
    }

    /// 构造可写回的文本 payload；测试 writer 会记录正文和哈希而不触碰系统剪贴板。
    fn copy_payload(text: &str) -> HistoryPayload {
        HistoryPayload {
            id: 9,
            item_type: "text".to_owned(),
            text_content: Some(text.to_owned()),
            preview_text: text.to_owned(),
            content_hash: ClipboardPayload::from_text(text)
                .summary()
                .content_hash
                .to_vec(),
            source_exe: None,
            source_app: None,
            copy_count: 2,
            is_pinned: false,
            created_at: 1,
            copied_at: 2,
            last_used_at: None,
            image: None,
        }
    }

    /// 构造只用于协调器接缝测试的完整图片 payload，不读取实际文件。
    fn image_copy_payload() -> HistoryPayload {
        let pixels =
            CanonicalImagePixels::new(1, 1, vec![4, 5, 6, 255]).expect("构造图片复制接缝像素失败");
        let hash = pixels.content_hash();
        let hash_hex = hash
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        HistoryPayload {
            id: 10,
            item_type: "image".to_owned(),
            text_content: None,
            preview_text: "图片 1 × 1".to_owned(),
            content_hash: hash.to_vec(),
            source_exe: None,
            source_app: None,
            copy_count: 1,
            is_pinned: false,
            created_at: 1,
            copied_at: 2,
            last_used_at: None,
            image: Some(HistoryImageSummary {
                metadata: ImageMetadata::new(
                    hash,
                    ImageAssetRootId::new([8; 32]),
                    format!("{}/{hash_hex}.png", &hash_hex[..2]),
                    format!("{}/{hash_hex}.webp", &hash_hex[..2]),
                    1,
                    1,
                    128,
                )
                .expect("构造图片复制接缝元数据失败"),
                canonical_root: std::path::PathBuf::from(r"C:\ClipboardBoard\images"),
            }),
        }
    }

    /// 仅复制必须把按 ID 读取的完整正文和哈希原样交给 writer，不能使用 UI 预览替代正文。
    #[test]
    fn 仅复制校验后写回完整正文() {
        let payload = copy_payload("完整正文\n第二行");
        let expected_hash = <[u8; 32]>::try_from(payload.content_hash.as_slice()).unwrap();
        let mut observed = None;
        let sequence = process_copy_payload(&payload, expected_hash, |text, hash| {
            observed = Some((text.to_owned(), hash));
            Ok::<u32, ClipboardWriteError>(77)
        })
        .expect("有效文本 payload 应允许写回");

        assert_eq!(sequence, 77);
        assert_eq!(
            observed,
            Some(("完整正文\n第二行".to_owned(), expected_hash))
        );
    }

    /// 图片 payload 必须按 UI 哈希和元数据哈希复核后进入图片准备接缝。
    #[test]
    fn 图片复制校验后进入图片接缝() {
        let payload = image_copy_payload();
        let expected_hash = <[u8; 32]>::try_from(payload.content_hash.as_slice()).unwrap();
        let mut observed = None;

        let sequence = process_typed_copy_payload(
            &payload,
            expected_hash,
            |_, _| panic!("图片 payload 不得进入文本 writer"),
            |image| {
                observed = Some(*image.metadata.content_hash());
                Ok(78)
            },
        )
        .expect("有效图片 payload 应进入图片接缝");

        assert_eq!(sequence, 78);
        assert_eq!(observed, Some(expected_hash));
    }

    /// 缺失资产、正文残留、旧 UI 哈希和未知类型必须在图片 writer 前拒绝。
    #[test]
    fn 图片复制拒绝不完整或不一致_payload() {
        let payload = image_copy_payload();
        let expected_hash = <[u8; 32]>::try_from(payload.content_hash.as_slice()).unwrap();
        let mut image_writes = 0;

        let mut missing_image = payload.clone();
        missing_image.image = None;
        assert!(matches!(
            process_typed_copy_payload(
                &missing_image,
                expected_hash,
                |_, _| unreachable!(),
                |_| {
                    image_writes += 1;
                    Ok(1)
                },
            ),
            Err(CopyProcessError::MissingImage)
        ));

        let mut residual_text = payload.clone();
        residual_text.text_content = Some("不应存在".to_owned());
        assert!(matches!(
            process_typed_copy_payload(
                &residual_text,
                expected_hash,
                |_, _| unreachable!(),
                |_| {
                    image_writes += 1;
                    Ok(1)
                },
            ),
            Err(CopyProcessError::InvalidPayload)
        ));

        assert!(matches!(
            process_typed_copy_payload(
                &payload,
                [1; 32],
                |_, _| unreachable!(),
                |_| {
                    image_writes += 1;
                    Ok(1)
                },
            ),
            Err(CopyProcessError::HashMismatch)
        ));

        let mut unknown = payload;
        unknown.item_type = "future".to_owned();
        assert!(matches!(
            process_typed_copy_payload(
                &unknown,
                expected_hash,
                |_, _| unreachable!(),
                |_| {
                    image_writes += 1;
                    Ok(1)
                },
            ),
            Err(CopyProcessError::UnsupportedType)
        ));
        assert_eq!(image_writes, 0);
    }

    /// stale UI 哈希、非 text、缺正文和坏哈希都必须在写回前拒绝。
    #[test]
    fn 仅复制拒绝不一致或不完整_payload() {
        let payload = copy_payload("稳定文本");
        let mut writes = 0;
        let hash = <[u8; 32]>::try_from(payload.content_hash.as_slice()).unwrap();
        assert!(matches!(
            process_copy_payload(&payload, [1; 32], |_, _| {
                writes += 1;
                Ok::<u32, ClipboardWriteError>(1)
            }),
            Err(CopyProcessError::HashMismatch)
        ));

        let mut non_text = payload.clone();
        non_text.item_type = "image".to_owned();
        assert!(matches!(
            process_copy_payload(&non_text, hash, |_, _| {
                writes += 1;
                Ok::<u32, ClipboardWriteError>(1)
            }),
            Err(CopyProcessError::UnsupportedType)
        ));

        let mut missing_text = payload.clone();
        missing_text.text_content = None;
        assert!(matches!(
            process_copy_payload(&missing_text, hash, |_, _| {
                writes += 1;
                Ok::<u32, ClipboardWriteError>(1)
            }),
            Err(CopyProcessError::MissingText)
        ));

        let mut bad_hash = payload;
        bad_hash.content_hash = vec![1, 2, 3];
        assert!(matches!(
            process_copy_payload(&bad_hash, hash, |_, _| {
                writes += 1;
                Ok::<u32, ClipboardWriteError>(1)
            }),
            Err(CopyProcessError::InvalidPayload)
        ));

        // 数据库哈希被篡改为合法长度时，仍必须用完整正文重算并拒绝写回，不能让 UI
        // 同步携带的错误哈希把自身写回事件伪装成真实用户复制。
        let mut tampered_hash = copy_payload("稳定文本");
        tampered_hash.content_hash = vec![9; 32];
        assert!(matches!(
            process_copy_payload(&tampered_hash, [9; 32], |_, _| {
                writes += 1;
                Ok::<u32, ClipboardWriteError>(1)
            }),
            Err(CopyProcessError::HashMismatch)
        ));

        // CF_UNICODETEXT 无法无损表示内部 NUL，必须在打开系统剪贴板前拒绝该 payload。
        let mut embedded_nul = copy_payload("前\0后");
        embedded_nul.content_hash = ClipboardPayload::from_text("前\0后")
            .summary()
            .content_hash
            .to_vec();
        assert!(matches!(
            process_copy_payload(
                &embedded_nul,
                <[u8; 32]>::try_from(embedded_nul.content_hash.as_slice()).unwrap(),
                |_, _| {
                    writes += 1;
                    Ok::<u32, ClipboardWriteError>(1)
                }
            ),
            Err(CopyProcessError::InvalidPayload)
        ));
        assert_eq!(writes, 0);
    }
}

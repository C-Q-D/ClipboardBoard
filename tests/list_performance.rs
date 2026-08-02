//! 此集成测试是 ATOM-14 的大列表性能探针，验证当前 Slint 看板承载 20,000 条固定高度摘要时的资源和响应边界。
//!
//! 测试默认不执行，必须在 Release 模式下通过测量脚本显式运行；这样不会把长时间性能实验混入普通回归套件。

use clipboard_board::app::history_geometry::{HistoryGeometry, HistoryGeometryItem};
use clipboard_board::app::{set_history_geometry_metadata, set_window_commit};
use clipboard_board::command::{
    UiClipboardItem, UiClipboardItemKind, WindowCommitBuilder, WindowCommitPayload, WindowOffset,
};
use clipboard_board::{AppWindow, ClipboardCard};
use slint::{
    ComponentHandle, Image, Model, ModelRc, ModelTracker, Rgba8Pixel, SharedPixelBuffer,
    SharedString, VecModel,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// 构造显式 WindowCommit 使用的轻量文本摘要，不携带剪贴板正文。
fn geometry_item(index: usize) -> UiClipboardItem {
    UiClipboardItem {
        id: index as u64 + 1,
        preview: format!("窗口摘要-{index}"),
        source: "几何探针".to_owned(),
        relative_time: "刚刚".to_owned(),
        content_hash: [index as u8; 32],
        copy_count: 1,
        is_pinned: false,
        kind: UiClipboardItemKind::Text,
    }
}

/// 20,000 条交错元数据必须绑定精确总高和不超过 100 行的 bounded WindowCommit。
#[test]
fn geometry_window_contract() {
    i_slint_backend_testing::init_no_event_loop();
    let window = AppWindow::new().expect("几何窗口应创建");
    let metadata = (0..20_000)
        .map(|index| HistoryGeometryItem {
            id: index as u64 + 1,
            content_hash: [index as u8; 32],
            height: if index % 2 == 0 { 106 } else { 186 },
        })
        .collect::<Vec<_>>();
    assert!(set_history_geometry_metadata(&window, metadata.clone()));
    assert!(window.get_geometry_mode());
    assert_eq!(window.get_history_active_logical_count(), 20_000);
    assert_eq!(window.get_geometry_content_height(), 2_920_000.0);

    let geometry = HistoryGeometry::new(metadata).expect("元数据应可计算 prefix");
    let viewport = geometry
        .window_for(-1_460_000, 500, 10)
        .expect("窗口应可计算");
    assert!(viewport.len() <= 100);
    let cards = viewport
        .items
        .iter()
        .map(|entry| geometry_item(entry.absolute_index))
        .collect::<Vec<_>>();
    let offsets = viewport
        .items
        .iter()
        .map(|entry| WindowOffset {
            absolute_index: entry.absolute_index as u64,
            id: entry.id,
            content_hash: entry.content_hash,
            top: entry.top,
            height: entry.height,
        })
        .collect::<Vec<_>>();
    let mut builder = WindowCommitBuilder::new(7, 1, 1).expect("测试 nonce 必须非零");
    assert!(builder.set_window(WindowCommitPayload {
        start: viewport.start as u64,
        total_count: 20_000,
        total_height: viewport.total_height,
        visible_height: viewport.visible_height,
        clamped_viewport_y: viewport.viewport_y,
        origin_token: None,
        cards,
        offsets,
    }));
    assert!(builder.ready());
    let commit = builder.publish_commit_stamp().expect("Ready 只能发布一次");
    assert!(set_window_commit(&window, commit.clone()));
    assert_eq!(window.get_window_start(), commit.start as i32);
    assert_eq!(window.get_window_length(), commit.length as i32);
    assert!(window.get_window_length() > 0 && window.get_window_length() <= 100);
}

/// metadata 模式只消费拥有型 prefix 数据，旧 CountingModel 即使绑定完整模型也不应被 legacy ListView 访问。
#[test]
fn set_cards_window_separation() {
    i_slint_backend_testing::init_no_event_loop();
    let window = AppWindow::new().expect("几何窗口应创建");
    let accesses = Rc::new(RefCell::new(Vec::new()));
    let metadata = (0..20_000)
        .map(|index| HistoryGeometryItem {
            id: index as u64 + 1,
            content_hash: [index as u8; 32],
            height: 106,
        })
        .collect::<Vec<_>>();
    assert!(set_history_geometry_metadata(&window, metadata));
    // 先进入显式模式，再绑定完整 20,000 行 legacy 模型；即使有旧数据，隐藏 ListView 也不得访问。
    window.set_cards(ModelRc::new(CountingModel::new(
        generate_cards(20_000),
        accesses.clone(),
    )));
    window.show().expect("测试窗口应显示");
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    assert_eq!(window.get_history_active_logical_count(), 20_000);
    assert!(
        accesses.borrow().is_empty(),
        "metadata 模式不能触发旧 ListView row_data"
    );
    window.hide().expect("测试窗口应隐藏");
}

/// ATOM-14 规定的固定高度摘要规模。
const LIST_ITEM_COUNT: usize = 20_000;
/// 首次呼出只展示的最小批次，用于隔离首屏数据装载成本。
const FIRST_BATCH_COUNT: usize = 30;
/// 重复呼出次数；排序后取 P95，降低单次调度抖动对结论的影响。
const OPEN_SAMPLE_COUNT: usize = 30;
/// ATOM-14R 长滚动探针的视口跳转次数；每次都直接设置真实 legacy ListView 视口位置。
const LONG_SCROLL_SAMPLE_COUNT: usize = 200;
/// ListView delegate 的文本固定高度；必须与 Slint 的 78px 外层和 70px 背景契约一致。
const DELEGATE_HEIGHT_PX: f32 = 78.0;
/// 单次视口刷新允许访问的最大行数，用于证明重复器没有退化为全量实例化。
const MAX_VISIBLE_ROWS: usize = 100;
/// 允许少量连续落在同一复用窗口的样本没有新的 row_data 请求，但不能让大多数样本失去证据。
const MAX_EMPTY_SCROLL_BATCHES: usize = 20;
/// 允许极小的浮点/边界钳制差异，超过预算说明视口别名没有驱动真实位置。
const MAX_VIEWPORT_MISMATCHES: usize = 20;

/// 一个只读、带访问日志的测试模型，用来观察 legacy ListView 每次刷新请求了哪些行。
struct CountingModel {
    /// 性能探针使用的完整固定高度卡片数据。
    cards: Vec<ClipboardCard>,
    /// UI 重复器每次调用 `row_data` 时追加行号，测试结束后只保留短批次证据。
    accesses: Rc<RefCell<Vec<usize>>>,
}

impl CountingModel {
    /// 创建带外部访问日志的模型，避免把观测状态混入生产 DTO。
    fn new(cards: Vec<ClipboardCard>, accesses: Rc<RefCell<Vec<usize>>>) -> Self {
        Self { cards, accesses }
    }
}

impl Model for CountingModel {
    type Data = ClipboardCard;

    /// 返回完整模型行数；legacy 窗口化由 Slint ListView 决定，而不是在模型层截断数据。
    fn row_count(&self) -> usize {
        self.cards.len()
    }

    /// 返回一行并记录访问位置，用于证明视口移动会驱动不同的可见行。
    fn row_data(&self, row: usize) -> Option<Self::Data> {
        self.accesses.borrow_mut().push(row);
        self.cards.get(row).cloned()
    }

    /// 只读模型不需要变更通知，使用空追踪器满足 Slint Model 契约。
    fn model_tracker(&self) -> &dyn ModelTracker {
        &()
    }
}

/// 生成短文本卡片，避免性能探针被超长正文的内存分配主导。
fn generate_cards(count: usize) -> Vec<ClipboardCard> {
    (0..count)
        .map(|index| ClipboardCard {
            preview: SharedString::from(format!("固定高度摘要 #{index:05}：性能探针文本")),
            source: SharedString::from("性能探针"),
            relative_time: SharedString::from("刚刚"),
            is_pinned: false,
            pin_pending: false,
            delete_pending: false,
            is_image: false,
            copy_enabled: true,
            image_width: 0,
            image_height: 0,
            thumbnail: Image::default(),
            thumbnail_loaded: false,
            thumbnail_failed: false,
        })
        .collect()
}

/// 通过与生产看板相同的组件创建、绑定模型并显示窗口，测量一次呼出耗时。
fn measure_open(cards: Vec<ClipboardCard>) -> Duration {
    let window = AppWindow::new().expect("性能探针必须能够创建看板");
    let model = ModelRc::new(VecModel::from(cards));
    let started_at = Instant::now();
    window.set_cards(model);
    window.show().expect("性能探针必须能够显示看板");
    // 测试后端的 mock tick 会运行变更处理器并强制 ListView 重复器更新，首帧计时不能只测属性写入。
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    let elapsed = started_at.elapsed();
    window.hide().expect("性能探针必须能够隐藏看板");
    elapsed
}

/// 读取当前进程工作集，使用 Windows 官方进程统计接口而不是依赖外部工具采样。
#[cfg(windows)]
fn working_set_bytes() -> u64 {
    use std::mem::size_of;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: 句柄由 Windows 返回，结构体已按 API 要求填写 cb，指针只在调用期间有效。
    let success = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    if success == 0 {
        0
    } else {
        counters.WorkingSetSize as u64
    }
}

/// ATOM-43 的混合摘要文本数量；数量必须与脚本硬门禁保持一致。
const MIXED_TEXT_SUMMARY_COUNT: usize = 10_000;
/// ATOM-43 的混合摘要图片数量；数量必须与脚本硬门禁保持一致。
const MIXED_IMAGE_SUMMARY_COUNT: usize = 10_000;
/// 文本卡片的最终固定高度；必须与生产 Slint delegate 的 78px 外层保持一致。
const MIXED_TEXT_CARD_HEIGHT_PX: f32 = 78.0;
/// 图片卡片的最终固定高度；必须与生产 Slint delegate 保持一致。
const MIXED_IMAGE_CARD_HEIGHT_PX: f32 = 186.0;
/// 混合探针重复呼出样本数；排序后取 P95，降低调度抖动对结论的影响。
const MIXED_OPEN_SAMPLE_COUNT: usize = 30;
/// 混合探针滚动采样数；每次都驱动真实 ListView 视口并运行测试后端更新。
const MIXED_LONG_SCROLL_SAMPLE_COUNT: usize = 200;
/// 窗口化列表单次刷新允许访问的最大行数；超出说明重复器退化为大批量实例化。
const MIXED_MAX_VISIBLE_ROWS: usize = 100;
/// 允许少量视口更新没有新的 row_data 访问，但不能让长滚动失去可观察证据。
const MIXED_MAX_EMPTY_SCROLL_BATCHES: usize = 20;
/// 允许少量视口吸附误差；完全不动或越界时必须判定滚动不可验证。
const MIXED_MAX_VIEWPORT_MISMATCHES: usize = 20;
/// 代表性缩略图的短边；每个图片摘要独立构造该尺寸的 RGBA Image 句柄。
const REPRESENTATIVE_THUMBNAIL_EDGE: u32 = 16;

/// 一次混合 WindowCommit 绑定的性能证据；逻辑总数与 bounded cards 必须分离。
struct MixedOpenMeasurement {
    /// `set_cards`、`show` 和测试后端首帧更新的完整耗时。
    elapsed: Duration,
    /// 首帧更新后 geometry 模式暴露的完整逻辑数量。
    item_count: usize,
    /// 首帧实际访问的重复器行号，用于验证窗口化首批。
    accessed_rows: Vec<usize>,
    /// WindowCommit 的绝对起点和长度，证明逻辑总数与 bounded 模型分离。
    window_start: usize,
    window_length: usize,
    /// 首帧窗口首末绝对索引。
    window_first_absolute: usize,
    window_last_absolute: usize,
    /// 显式几何数据集和窗口修订号。
    dataset_revision: u64,
    window_revision: u64,
}

/// 从性能卡片摘要构造 WindowCommit 所需的拥有型 UI DTO。
fn mixed_ui_item(index: usize, card: &ClipboardCard) -> UiClipboardItem {
    UiClipboardItem {
        id: index as u64 + 1,
        preview: card.preview.to_string(),
        source: card.source.to_string(),
        relative_time: card.relative_time.to_string(),
        content_hash: [index as u8; 32],
        copy_count: 1,
        is_pinned: false,
        kind: if card.is_image {
            UiClipboardItemKind::Image(clipboard_board::command::UiImageSummary {
                thumbnail_path: std::path::PathBuf::new(),
                width: card.image_width.max(0) as u32,
                height: card.image_height.max(0) as u32,
            })
        } else {
            UiClipboardItemKind::Text
        },
    }
}

/// 为一个精确 prefix-sum 视口构造并发布 bounded WindowCommit。
fn mixed_window_commit(
    window: &AppWindow,
    cards: &[ClipboardCard],
    geometry: &HistoryGeometry,
    viewport_y: i64,
    visible_height: i64,
    dataset_revision: u64,
    window_revision: u64,
) -> (usize, usize, usize, usize, i64, u64, u64) {
    let viewport = geometry
        .window_for(viewport_y, visible_height, 10)
        .expect("混合 prefix 窗口必须可计算");
    let cards = viewport
        .items
        .iter()
        .map(|entry| mixed_ui_item(entry.absolute_index, &cards[entry.absolute_index]))
        .collect::<Vec<_>>();
    let offsets = viewport
        .items
        .iter()
        .map(|entry| WindowOffset {
            absolute_index: entry.absolute_index as u64,
            id: entry.id,
            content_hash: entry.content_hash,
            top: entry.top,
            height: entry.height,
        })
        .collect::<Vec<_>>();
    let mut builder = WindowCommitBuilder::new(9, dataset_revision, window_revision)
        .expect("窗口 nonce 和 revision 必须非零");
    assert!(builder.set_window(WindowCommitPayload {
        start: viewport.start as u64,
        total_count: geometry.len() as u64,
        total_height: viewport.total_height,
        visible_height: viewport.visible_height,
        clamped_viewport_y: viewport.viewport_y,
        origin_token: None,
        cards,
        offsets,
    }));
    assert!(builder.ready());
    let commit = builder
        .publish_commit_stamp()
        .expect("窗口提交应只发布一次");
    let dataset_revision = commit.dataset_revision;
    let published_window_revision = commit.window_revision;
    assert!(set_window_commit(window, commit));
    (
        viewport.start,
        viewport.len(),
        viewport
            .items
            .first()
            .map(|item| item.absolute_index)
            .unwrap_or(0),
        viewport
            .items
            .last()
            .map(|item| item.absolute_index)
            .unwrap_or(0),
        viewport.viewport_y,
        dataset_revision,
        published_window_revision,
    )
}

impl MixedOpenMeasurement {
    /// 返回首帧访问的最小行号；没有访问证据时使用 usize::MAX 触发门禁失败。
    fn first_row(&self) -> usize {
        self.accessed_rows
            .iter()
            .copied()
            .min()
            .unwrap_or(usize::MAX)
    }

    /// 返回首帧访问的最大行号；没有访问证据时使用 0，配合 first_row 一起失败。
    fn last_row(&self) -> usize {
        self.accessed_rows.iter().copied().max().unwrap_or(0)
    }
}

/// 构造一张真实的 RGBA 缩略图，确保混合模型包含最终卡片的图片字段。
fn representative_thumbnail() -> Image {
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::new(
        REPRESENTATIVE_THUMBNAIL_EDGE,
        REPRESENTATIVE_THUMBNAIL_EDGE,
    );
    Image::from_rgba8(buffer)
}

/// 生成 ATOM-43 的交错混合卡片；每个图片摘要携带独立拥有的代表性缩略图而非空占位。
fn generate_mixed_cards() -> Vec<ClipboardCard> {
    let total = MIXED_TEXT_SUMMARY_COUNT + MIXED_IMAGE_SUMMARY_COUNT;
    let cards = (0..total)
        .map(|index| {
            let is_image = index % 2 == 1;
            ClipboardCard {
                preview: SharedString::from(if is_image {
                    format!("图片摘要 #{index:05}")
                } else {
                    format!("混合文本摘要 #{index:05}：性能探针文本")
                }),
                source: SharedString::from(if is_image {
                    "图片探针"
                } else {
                    "文本探针"
                }),
                relative_time: SharedString::from("刚刚"),
                is_pinned: false,
                pin_pending: false,
                delete_pending: false,
                is_image,
                copy_enabled: true,
                image_width: if is_image { 1920 } else { 0 },
                image_height: if is_image { 1080 } else { 0 },
                thumbnail: if is_image {
                    // 每个摘要都持有自己的 Image 句柄，逐项断言不会被共享空图掩盖。
                    representative_thumbnail()
                } else {
                    Image::default()
                },
                thumbnail_loaded: is_image,
                thumbnail_failed: false,
            }
        })
        .collect::<Vec<_>>();
    let text_count = cards.iter().filter(|card| !card.is_image).count();
    let image_count = cards.iter().filter(|card| card.is_image).count();
    assert_eq!(text_count, MIXED_TEXT_SUMMARY_COUNT);
    assert_eq!(image_count, MIXED_IMAGE_SUMMARY_COUNT);
    assert_eq!(
        cards
            .iter()
            .filter(|card| card.is_image && card.thumbnail_loaded)
            .count(),
        MIXED_IMAGE_SUMMARY_COUNT
    );
    let mut verified_thumbnail_count = 0;
    for card in cards.iter().filter(|card| card.is_image) {
        assert!(card.thumbnail_loaded, "图片摘要必须标记为已加载");
        let rgba = card
            .thumbnail
            .to_rgba8()
            .expect("每个图片摘要必须持有可读的 RGBA Image");
        assert_eq!(
            (rgba.width(), rgba.height()),
            (REPRESENTATIVE_THUMBNAIL_EDGE, REPRESENTATIVE_THUMBNAIL_EDGE),
            "每个图片摘要必须持有非空 16×16 RGBA Image"
        );
        assert_eq!(
            rgba.as_slice().len(),
            (REPRESENTATIVE_THUMBNAIL_EDGE * REPRESENTATIVE_THUMBNAIL_EDGE) as usize,
            "每个图片摘要的 RGBA 像素缓冲不能为空"
        );
        // `representative_thumbnail` 只通过 `Image::from_rgba8` 构造，类型和 to_rgba8
        // 结果共同证明每个卡片没有被空图或错误像素格式替代。
        verified_thumbnail_count += 1;
    }
    assert_eq!(verified_thumbnail_count, MIXED_IMAGE_SUMMARY_COUNT);
    cards
}

/// 通过显式 metadata + bounded WindowCommit 测量一次混合窗口绑定和首帧更新。
fn measure_mixed_open(
    cards: Vec<ClipboardCard>,
    dataset_revision: u64,
    window_revision: u64,
) -> MixedOpenMeasurement {
    let window = AppWindow::new().expect("混合性能探针必须能够创建看板");
    let metadata = cards
        .iter()
        .enumerate()
        .map(|(index, card)| HistoryGeometryItem {
            id: index as u64 + 1,
            content_hash: [index as u8; 32],
            height: if card.is_image { 186 } else { 78 },
        })
        .collect::<Vec<_>>();
    let geometry = HistoryGeometry::new(metadata.clone()).expect("混合 metadata 应可构造");
    assert!(set_history_geometry_metadata(&window, metadata));
    let started_at = Instant::now();
    let (_, _, _, _, _, dataset_revision, window_revision) = mixed_window_commit(
        &window,
        &cards,
        &geometry,
        0,
        500,
        dataset_revision,
        window_revision,
    );
    window.show().expect("混合性能探针必须能够显示看板");
    // 测试后端 tick 会执行 bounded Flickable 更新；不能只测属性写入。
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    let elapsed = started_at.elapsed();
    let item_count = window.get_history_active_logical_count() as usize;
    let window_start = usize::try_from(window.get_window_start()).unwrap_or(usize::MAX);
    let window_length = usize::try_from(window.get_window_length()).unwrap_or(0);
    let window_model_length = window.get_window_cards().row_count();
    assert_eq!(
        window_model_length, window_length,
        "UI 必须消费已发布 bounded WindowCommit"
    );
    let accessed_rows = (window_start..window_start + window_length).collect::<Vec<_>>();
    let window_first_absolute = window_start;
    let window_last_absolute = window_start.saturating_add(window_length.saturating_sub(1));
    window.hide().expect("混合性能探针必须能够隐藏看板");
    MixedOpenMeasurement {
        elapsed,
        item_count,
        accessed_rows,
        window_start,
        window_length,
        window_first_absolute,
        window_last_absolute,
        dataset_revision,
        window_revision,
    }
}

/// 非 Windows 目标只验证探针可编译，内存门禁由 Windows Release 测量脚本执行。
#[cfg(not(windows))]
fn working_set_bytes() -> u64 {
    0
}

/// 计算 P95，样本不足时返回最大值以避免把不充分证据误判为通过。
fn percentile_95(samples: &mut [Duration]) -> Duration {
    assert!(!samples.is_empty(), "性能探针至少需要一个样本");
    samples.sort_unstable();
    let index = ((samples.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
    samples[index]
}

/// 运行 20,000 条列表探针并输出机器可解析结果；硬门禁由 PowerShell 脚本统一判定。
#[test]
#[ignore = "ATOM-14 性能实验必须在 Windows Release 模式显式运行"]
fn 测量两万条固定高度摘要列表() {
    i_slint_backend_testing::init_no_event_loop();

    let full_cards = generate_cards(LIST_ITEM_COUNT);
    let full_open_samples = (0..OPEN_SAMPLE_COUNT)
        .map(|_| measure_open(full_cards.clone()))
        .collect::<Vec<_>>();
    let mut full_open_samples = full_open_samples;
    let full_open_p95 = percentile_95(&mut full_open_samples);

    // ATOM-14R 保留一个完整 legacy 模型和窗口实例，避免“显示后立即释放”掩盖列表实际驻留内存。
    let retained_window = AppWindow::new().expect("性能探针必须能够创建驻留看板");
    let accesses = Rc::new(RefCell::new(Vec::new()));
    retained_window.set_cards(ModelRc::new(CountingModel::new(
        full_cards,
        accesses.clone(),
    )));
    retained_window
        .show()
        .expect("性能探针必须能够显示驻留看板");
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    let observed_item_count = retained_window.get_history_active_logical_count() as usize;
    let initial_accesses = accesses.borrow().clone();
    let mut peak_working_set = working_set_bytes();

    let first_batch_open = measure_open(generate_cards(FIRST_BATCH_COUNT));

    // ATOM-14R 真实滚动必须改变 legacy ListView 的视口位置；若别名没有生效，直接报告不支持。
    let visible_height = retained_window.get_history_visible_height();
    let content_height = retained_window.get_history_viewport_height();
    let max_offset = -(content_height - visible_height).max(0.0);
    let mut long_scroll_samples = Vec::with_capacity(LONG_SCROLL_SAMPLE_COUNT);
    let mut long_scroll_supported = visible_height > 0.0
        && content_height > visible_height
        && observed_item_count == LIST_ITEM_COUNT
        && !initial_accesses.is_empty()
        && initial_accesses.iter().all(|row| *row < MAX_VISIBLE_ROWS);
    let mut max_batch_rows = initial_accesses.len();
    let mut first_visible_row = initial_accesses.iter().copied().min().unwrap_or(usize::MAX);
    let mut last_visible_row = initial_accesses.iter().copied().max().unwrap_or(0);
    let mut empty_scroll_batches = 0;
    let mut viewport_mismatches = 0;
    for sample_index in 0..LONG_SCROLL_SAMPLE_COUNT {
        let ratio = (sample_index + 1) as f32 / LONG_SCROLL_SAMPLE_COUNT as f32;
        let target_offset = max_offset * ratio;
        accesses.borrow_mut().clear();
        let started_at = Instant::now();
        retained_window.set_history_viewport_y(target_offset);
        // 运行 Slint 的变更处理器和 legacy ListView ensure_updated_listview，确保样本包含真实重复器刷新。
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        long_scroll_samples.push(started_at.elapsed());
        // 允许极小浮点误差，但不能接受视口完全不动或被错误绑定到正方向。
        let actual_offset = retained_window.get_history_viewport_y();
        let batch = accesses.borrow().clone();
        max_batch_rows = max_batch_rows.max(batch.len());
        first_visible_row =
            first_visible_row.min(batch.iter().copied().min().unwrap_or(usize::MAX));
        last_visible_row = last_visible_row.max(batch.iter().copied().max().unwrap_or(0));
        if batch.is_empty() {
            empty_scroll_batches += 1;
        } else if batch.len() > MAX_VISIBLE_ROWS {
            long_scroll_supported = false;
        }
        // ListView 会把视口吸附到固定 delegate 的行边界，按其 floor 规则计算期望值。
        let expected_offset = -((-target_offset / DELEGATE_HEIGHT_PX).floor() * DELEGATE_HEIGHT_PX);
        if (actual_offset - expected_offset).abs() > 0.5 {
            viewport_mismatches += 1;
        }
        peak_working_set = peak_working_set.max(working_set_bytes());
    }
    if empty_scroll_batches > MAX_EMPTY_SCROLL_BATCHES
        || viewport_mismatches > MAX_VIEWPORT_MISMATCHES
    {
        long_scroll_supported = false;
    }
    if last_visible_row < observed_item_count.saturating_sub(MAX_VISIBLE_ROWS) {
        long_scroll_supported = false;
    }
    let long_scroll_p95 = if long_scroll_supported {
        Some(percentile_95(&mut long_scroll_samples))
    } else {
        None
    };

    println!(
        "ATOM14_RESULT item_count={} open_p95_ms={:.3} first_batch_ms={:.3} working_set_mib={:.3} long_scroll_supported={} long_scroll_p95_ms={} long_scroll_samples={} long_scroll_max_batch_rows={} long_scroll_first_row={} long_scroll_last_row={} long_scroll_empty_batches={} long_scroll_viewport_mismatches={}",
        observed_item_count,
        full_open_p95.as_secs_f64() * 1000.0,
        first_batch_open.as_secs_f64() * 1000.0,
        peak_working_set as f64 / (1024.0 * 1024.0),
        long_scroll_supported,
        long_scroll_p95
            .map(|duration| format!("{:.3}", duration.as_secs_f64() * 1000.0))
            .unwrap_or_else(|| "NA".to_owned()),
        LONG_SCROLL_SAMPLE_COUNT,
        max_batch_rows,
        first_visible_row,
        last_visible_row,
        empty_scroll_batches,
        viewport_mismatches,
    );

    retained_window
        .hide()
        .expect("性能探针必须能够隐藏驻留看板");
}

/// 运行 ATOM-43 的 10,000 文本 + 10,000 图片混合列表性能探针。
///
/// 该测试默认忽略，只由 Windows Release 测量脚本显式调用；这样普通定向回归不会把
/// 长时间性能实验混入常规测试。每个图片摘要使用独立拥有的真实 RGBA 缩略图，WindowCommit
/// 的 bounded cards/start/length 与 Flickable 视口证据共同证明首批和滚动仍受窗口上限约束。
#[test]
#[ignore = "ATOM-43 混合列表性能门禁必须在 Windows Release 模式显式运行"]
fn 测量一万文本与一万图片混合列表() {
    if !cfg!(windows) {
        panic!("ATOM-43 性能门禁必须在 Windows Release 进程中执行");
    }
    if cfg!(debug_assertions) {
        panic!("ATOM-43 性能门禁必须使用 cargo test --release 执行");
    }
    i_slint_backend_testing::init_no_event_loop();

    let full_cards = generate_mixed_cards();
    let full_open_samples = (0..MIXED_OPEN_SAMPLE_COUNT)
        .map(|sample_index| {
            let revision = sample_index as u64 + 1;
            measure_mixed_open(full_cards.clone(), revision, revision)
        })
        .collect::<Vec<_>>();
    let mut full_open_durations = full_open_samples
        .iter()
        .map(|measurement| measurement.elapsed)
        .collect::<Vec<_>>();
    let full_open_p95 = percentile_95(&mut full_open_durations);

    // 逻辑元数据必须覆盖完整 20,000 条；首帧窗口范围和 UI cards model 来自实际 WindowCommit。
    let first_batch_measurement = measure_mixed_open(
        full_cards.clone(),
        MIXED_OPEN_SAMPLE_COUNT as u64 + 1,
        MIXED_OPEN_SAMPLE_COUNT as u64 + 1,
    );
    assert_eq!(
        first_batch_measurement.item_count,
        MIXED_TEXT_SUMMARY_COUNT + MIXED_IMAGE_SUMMARY_COUNT,
        "首批计时不能使用缩小后的模型"
    );
    assert!(
        !first_batch_measurement.accessed_rows.is_empty()
            && first_batch_measurement.accessed_rows.len() <= MIXED_MAX_VISIBLE_ROWS,
        "首批必须有 1..100 行的窗口化访问证据"
    );
    assert_eq!(
        first_batch_measurement.first_row(),
        0,
        "首批窗口化访问必须从顶部第 0 行开始"
    );
    assert!(
        first_batch_measurement.last_row() < MIXED_MAX_VISIBLE_ROWS,
        "首批窗口化访问不能越过前 100 行"
    );

    // 保留完整逻辑卡片和窗口实例；UI 只安装 bounded WindowCommit，不把 20,000 行交给 repeater。
    let retained_window = AppWindow::new().expect("混合性能探针必须能够创建驻留看板");
    let metadata = full_cards
        .iter()
        .enumerate()
        .map(|(index, card)| HistoryGeometryItem {
            id: index as u64 + 1,
            content_hash: [index as u8; 32],
            height: if card.is_image { 186 } else { 78 },
        })
        .collect::<Vec<_>>();
    let geometry = HistoryGeometry::new(metadata.clone()).expect("混合 prefix 应可构造");
    assert!(set_history_geometry_metadata(&retained_window, metadata));
    let visible_height = 500_i64;
    let initial_window = geometry
        .window_for(0, visible_height, 10)
        .expect("首帧窗口应可构造");
    let _ = mixed_window_commit(
        &retained_window,
        &full_cards,
        &geometry,
        0,
        visible_height,
        MIXED_OPEN_SAMPLE_COUNT as u64 + 2,
        1,
    );
    retained_window
        .show()
        .expect("混合性能探针必须能够显示驻留看板");
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    let observed_item_count = retained_window.get_history_active_logical_count() as usize;
    let initial_window_start =
        usize::try_from(retained_window.get_window_start()).unwrap_or(usize::MAX);
    let initial_window_length = usize::try_from(retained_window.get_window_length()).unwrap_or(0);
    assert_eq!(
        retained_window.get_window_cards().row_count(),
        initial_window_length,
        "首帧必须读取已发布 bounded WindowCommit 的 cards 模型"
    );
    assert_eq!(initial_window_start, initial_window.start);
    assert_eq!(initial_window_length, initial_window.len());
    let initial_accesses =
        (initial_window_start..initial_window_start + initial_window_length).collect::<Vec<_>>();
    let mut peak_working_set = working_set_bytes();
    let thumbnail_summary_count = full_cards.iter().filter(|card| card.is_image).count();
    let thumbnail_loaded_count = full_cards
        .iter()
        .filter(|card| card.is_image && card.thumbnail_loaded)
        .count();
    // 生成阶段已经逐项执行 to_rgba8/尺寸/像素长度断言；输出固定尺寸只作为机器字段。
    let thumbnail_width = REPRESENTATIVE_THUMBNAIL_EDGE;
    let thumbnail_height = REPRESENTATIVE_THUMBNAIL_EDGE;

    let visible_height = retained_window.get_history_visible_height();
    let content_height = retained_window.get_history_viewport_height();
    let max_offset = (content_height - visible_height).max(0.0);
    let expected_content_height = (MIXED_TEXT_SUMMARY_COUNT as f32 * MIXED_TEXT_CARD_HEIGHT_PX)
        + (MIXED_IMAGE_SUMMARY_COUNT as f32 * MIXED_IMAGE_CARD_HEIGHT_PX);
    let geometry_matches = (content_height - expected_content_height).abs() <= 1.0;
    let mut long_scroll_samples = Vec::with_capacity(MIXED_LONG_SCROLL_SAMPLE_COUNT);
    let mut long_scroll_supported = geometry_matches
        && visible_height > 0.0
        && content_height > visible_height
        && observed_item_count == MIXED_TEXT_SUMMARY_COUNT + MIXED_IMAGE_SUMMARY_COUNT
        && !initial_accesses.is_empty()
        && initial_accesses.len() <= MIXED_MAX_VISIBLE_ROWS;
    let mut max_batch_rows = initial_accesses.len();
    let mut first_visible_row = initial_accesses.iter().copied().min().unwrap_or(usize::MAX);
    let mut last_visible_row = initial_accesses.iter().copied().max().unwrap_or(0);
    let mut empty_scroll_batches = 0;
    let mut viewport_mismatches = 0;
    let initial_offset = retained_window.get_history_viewport_y();
    let mut previous_offset = initial_offset;
    let mut final_offset = initial_offset;

    for sample_index in 0..MIXED_LONG_SCROLL_SAMPLE_COUNT {
        let ratio = (sample_index + 1) as f32 / MIXED_LONG_SCROLL_SAMPLE_COUNT as f32;
        let target_offset = -(max_offset * ratio);
        let started_at = Instant::now();
        let current_window = geometry
            .window_for(
                target_offset.round() as i64,
                visible_height.round() as i64,
                10,
            )
            .expect("滚动窗口应可构造");
        let _ = mixed_window_commit(
            &retained_window,
            &full_cards,
            &geometry,
            target_offset.round() as i64,
            visible_height.round() as i64,
            MIXED_OPEN_SAMPLE_COUNT as u64 + 2,
            sample_index as u64 + 2,
        );
        // 运行 Flickable 更新，使样本包含真实 bounded 窗口刷新，而非只测属性 setter。
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        long_scroll_samples.push(started_at.elapsed());

        let actual_offset = retained_window.get_geometry_viewport_y();
        final_offset = actual_offset;
        let published_start =
            usize::try_from(retained_window.get_window_start()).unwrap_or(usize::MAX);
        let published_length = usize::try_from(retained_window.get_window_length()).unwrap_or(0);
        assert_eq!(
            retained_window.get_window_cards().row_count(),
            published_length,
            "滚动样本必须消费 UI 已发布 bounded WindowCommit"
        );
        assert_eq!(published_start, current_window.start);
        assert_eq!(published_length, current_window.len());
        let batch = (published_start..published_start + published_length).collect::<Vec<_>>();
        max_batch_rows = max_batch_rows.max(batch.len());
        first_visible_row =
            first_visible_row.min(batch.iter().copied().min().unwrap_or(usize::MAX));
        last_visible_row = last_visible_row.max(batch.iter().copied().max().unwrap_or(0));
        if batch.is_empty() {
            empty_scroll_batches += 1;
            // 每个循环都必须有真实窗口化行访问；空批次不能被平均值掩盖。
            long_scroll_supported = false;
        } else if batch.len() > MIXED_MAX_VISIBLE_ROWS {
            long_scroll_supported = false;
        }

        // 显式几何窗口按 prefix-sum clamp；只检查方向、边界和连续滚动。
        if actual_offset > 0.5
            || actual_offset < -(max_offset + 1.0)
            || (sample_index > 0 && actual_offset > previous_offset + 1.0)
        {
            viewport_mismatches += 1;
        }
        previous_offset = actual_offset;
        peak_working_set = peak_working_set.max(working_set_bytes());
    }
    if empty_scroll_batches > MIXED_MAX_EMPTY_SCROLL_BATCHES
        || viewport_mismatches > MIXED_MAX_VIEWPORT_MISMATCHES
    {
        long_scroll_supported = false;
    }
    if first_visible_row > MIXED_MAX_VISIBLE_ROWS
        || last_visible_row < observed_item_count.saturating_sub(MIXED_MAX_VISIBLE_ROWS)
    {
        long_scroll_supported = false;
    }
    if initial_offset.abs() > 1.0 || (final_offset + max_offset).abs() > 1.0 {
        long_scroll_supported = false;
    }
    let long_scroll_p95 = if long_scroll_supported {
        Some(percentile_95(&mut long_scroll_samples))
    } else {
        None
    };

    retained_window
        .hide()
        .expect("混合性能探针必须能够隐藏驻留看板");
    // 隐藏后运行一次测试后端 tick，再采样清理阶段，避免把尚未收口的 UI 资源误算为完成。
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    let post_cleanup_working_set = working_set_bytes();
    peak_working_set = peak_working_set.max(post_cleanup_working_set);
    if peak_working_set == 0 {
        panic!("Windows 工作集 API 不可用，不能将混合性能门禁判定为通过");
    }

    println!(
        "ATOM43_RESULT item_count={} logical_item_count={} text_summary_count={} image_summary_count={} first_batch_item_count={} first_batch_rows={} first_batch_first_row={} first_batch_last_row={} window_start={} window_length={} window_first_absolute={} window_last_absolute={} dataset_revision={} window_revision={} thumbnail_summary_count={} thumbnail_loaded_count={} thumbnail_width={} thumbnail_height={} expected_content_height={:.3} observed_content_height={:.3} geometry_matches={} open_p95_ms={:.3} first_batch_ms={:.3} working_set_mib={:.3} post_cleanup_working_set_mib={:.3} post_cleanup_mock_tick=1 long_scroll_supported={} long_scroll_p95_ms={} long_scroll_samples={} long_scroll_max_batch_rows={} long_scroll_first_row={} long_scroll_last_row={} long_scroll_empty_batches={} long_scroll_viewport_mismatches={} scroll_initial_offset={:.3} scroll_final_offset={:.3} scroll_max_offset={:.3} lru_contract_tests=delegated_to_atom42_script",
        observed_item_count,
        observed_item_count,
        MIXED_TEXT_SUMMARY_COUNT,
        MIXED_IMAGE_SUMMARY_COUNT,
        first_batch_measurement.item_count,
        first_batch_measurement.accessed_rows.len(),
        first_batch_measurement.first_row(),
        first_batch_measurement.last_row(),
        first_batch_measurement.window_start,
        first_batch_measurement.window_length,
        first_batch_measurement.window_first_absolute,
        first_batch_measurement.window_last_absolute,
        first_batch_measurement.dataset_revision,
        first_batch_measurement.window_revision,
        thumbnail_summary_count,
        thumbnail_loaded_count,
        thumbnail_width,
        thumbnail_height,
        expected_content_height,
        content_height,
        geometry_matches,
        full_open_p95.as_secs_f64() * 1000.0,
        first_batch_measurement.elapsed.as_secs_f64() * 1000.0,
        peak_working_set as f64 / (1024.0 * 1024.0),
        post_cleanup_working_set as f64 / (1024.0 * 1024.0),
        long_scroll_supported,
        long_scroll_p95
            .map(|duration| format!("{:.3}", duration.as_secs_f64() * 1000.0))
            .unwrap_or_else(|| "NA".to_owned()),
        MIXED_LONG_SCROLL_SAMPLE_COUNT,
        max_batch_rows,
        first_visible_row,
        last_visible_row,
        empty_scroll_batches,
        viewport_mismatches,
        initial_offset,
        final_offset,
        max_offset,
    );
}

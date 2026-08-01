//! 此集成测试是 ATOM-14 的大列表性能探针，验证当前 Slint 看板承载 20,000 条固定高度摘要时的资源和响应边界。
//!
//! 测试默认不执行，必须在 Release 模式下通过测量脚本显式运行；这样不会把长时间性能实验混入普通回归套件。

use clipboard_board::{AppWindow, ClipboardCard};
use slint::{
    ComponentHandle, Image, Model, ModelRc, ModelTracker, Rgba8Pixel, SharedPixelBuffer,
    SharedString, VecModel,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// ATOM-14 规定的固定高度摘要规模。
const LIST_ITEM_COUNT: usize = 20_000;
/// 首次呼出只展示的最小批次，用于隔离首屏数据装载成本。
const FIRST_BATCH_COUNT: usize = 30;
/// 重复呼出次数；排序后取 P95，降低单次调度抖动对结论的影响。
const OPEN_SAMPLE_COUNT: usize = 30;
/// 长滚动探针的视口跳转次数；每次都直接设置真实 ListView 视口位置。
const LONG_SCROLL_SAMPLE_COUNT: usize = 200;
/// ListView delegate 的固定高度；必须与 `ui/app-window.slint` 的 96px 卡片加 10px 间隔一致。
const DELEGATE_HEIGHT_PX: f32 = 106.0;
/// 单次视口刷新允许访问的最大行数，用于证明重复器没有退化为全量实例化。
const MAX_VISIBLE_ROWS: usize = 100;
/// 允许少量连续落在同一复用窗口的样本没有新的 row_data 请求，但不能让大多数样本失去证据。
const MAX_EMPTY_SCROLL_BATCHES: usize = 20;
/// 允许极小的浮点/边界钳制差异，超过预算说明视口别名没有驱动真实位置。
const MAX_VIEWPORT_MISMATCHES: usize = 20;

/// 一个只读、带访问日志的测试模型，用来观察 ListView 每次刷新请求了哪些行。
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

    /// 返回完整模型行数；窗口化由 Slint ListView 决定，而不是在模型层截断数据。
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
/// 文本卡片的最终固定高度；必须与生产 Slint delegate 保持一致。
const MIXED_TEXT_CARD_HEIGHT_PX: f32 = 106.0;
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

/// 一次混合模型绑定的性能证据；首批必须与完整模型长度绑定，不能另绑小模型。
struct MixedOpenMeasurement {
    /// `set_cards`、`show` 和测试后端首帧更新的完整耗时。
    elapsed: Duration,
    /// 首帧更新后 ListView 仍然持有的模型行数。
    item_count: usize,
    /// 首帧实际访问的重复器行号，用于验证窗口化首批。
    accessed_rows: Vec<usize>,
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

/// 通过最终 AppWindow 与混合 ListView 模型测量一次绑定、窗口显示和首帧更新。
fn measure_mixed_open(cards: Vec<ClipboardCard>) -> MixedOpenMeasurement {
    let window = AppWindow::new().expect("混合性能探针必须能够创建看板");
    let accesses = Rc::new(RefCell::new(Vec::new()));
    let model = ModelRc::new(CountingModel::new(cards, accesses.clone()));
    let started_at = Instant::now();
    window.set_cards(model);
    window.show().expect("混合性能探针必须能够显示看板");
    // 测试后端 tick 会执行 ListView 重复器更新；不能只测 set_cards 属性写入。
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    let elapsed = started_at.elapsed();
    let item_count = window.get_history_model_length() as usize;
    let accessed_rows = accesses.borrow().clone();
    window.hide().expect("混合性能探针必须能够隐藏看板");
    MixedOpenMeasurement {
        elapsed,
        item_count,
        accessed_rows,
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

    // 保留一个完整模型和窗口实例，避免“显示后立即释放”掩盖列表实际驻留内存。
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
    let observed_item_count = retained_window.get_history_model_length() as usize;
    let initial_accesses = accesses.borrow().clone();
    let mut peak_working_set = working_set_bytes();

    let first_batch_open = measure_open(generate_cards(FIRST_BATCH_COUNT));

    // 真实滚动必须改变 ListView 的视口位置；若别名没有生效，直接报告不支持。
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
        // 运行 Slint 的变更处理器和 ListView ensure_updated_listview，确保样本包含真实重复器刷新。
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
/// 长时间性能实验混入常规测试。每个图片摘要使用独立拥有的真实 RGBA 缩略图，ListView 访问
/// 日志则证明首批和滚动仍由窗口化重复器承载。
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
        .map(|_| measure_mixed_open(full_cards.clone()))
        .collect::<Vec<_>>();
    let mut full_open_durations = full_open_samples
        .iter()
        .map(|measurement| measurement.elapsed)
        .collect::<Vec<_>>();
    let full_open_p95 = percentile_95(&mut full_open_durations);

    // 首批也必须绑定完整 20,000 条模型；只依赖真实 ListView 首帧访问范围证明窗口化。
    let first_batch_measurement = measure_mixed_open(full_cards.clone());
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

    // 保留完整混合模型和窗口实例，确保工作集包含卡片摘要、图片字段和 ListView 重复器。
    let retained_window = AppWindow::new().expect("混合性能探针必须能够创建驻留看板");
    let accesses = Rc::new(RefCell::new(Vec::new()));
    retained_window.set_cards(ModelRc::new(CountingModel::new(
        full_cards.clone(),
        accesses.clone(),
    )));
    retained_window
        .show()
        .expect("混合性能探针必须能够显示驻留看板");
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    let observed_item_count = retained_window.get_history_model_length() as usize;
    let initial_accesses = accesses.borrow().clone();
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
        && initial_accesses
            .iter()
            .all(|row| *row < MIXED_MAX_VISIBLE_ROWS);
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
        accesses.borrow_mut().clear();
        let started_at = Instant::now();
        retained_window.set_history_viewport_y(target_offset);
        // 运行 ListView 更新，使样本包含真实重复器刷新，而非只测属性 setter。
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        long_scroll_samples.push(started_at.elapsed());

        let actual_offset = retained_window.get_history_viewport_y();
        final_offset = actual_offset;
        let batch = accesses.borrow().clone();
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

        // ListView 允许按混合卡片高度吸附；只检查方向、边界和连续滚动，不伪造固定行高。
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
        "ATOM43_RESULT item_count={} text_summary_count={} image_summary_count={} first_batch_item_count={} first_batch_rows={} first_batch_first_row={} first_batch_last_row={} thumbnail_summary_count={} thumbnail_loaded_count={} thumbnail_width={} thumbnail_height={} expected_content_height={:.3} observed_content_height={:.3} geometry_matches={} open_p95_ms={:.3} first_batch_ms={:.3} working_set_mib={:.3} post_cleanup_working_set_mib={:.3} post_cleanup_mock_tick=1 long_scroll_supported={} long_scroll_p95_ms={} long_scroll_samples={} long_scroll_max_batch_rows={} long_scroll_first_row={} long_scroll_last_row={} long_scroll_empty_batches={} long_scroll_viewport_mismatches={} scroll_initial_offset={:.3} scroll_final_offset={:.3} scroll_max_offset={:.3} lru_contract_tests=delegated_to_atom42_script",
        observed_item_count,
        MIXED_TEXT_SUMMARY_COUNT,
        MIXED_IMAGE_SUMMARY_COUNT,
        first_batch_measurement.item_count,
        first_batch_measurement.accessed_rows.len(),
        first_batch_measurement.first_row(),
        first_batch_measurement.last_row(),
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

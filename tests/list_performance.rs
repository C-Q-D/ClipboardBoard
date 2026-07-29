//! 此集成测试是 ATOM-14 的大列表性能探针，验证当前 Slint 看板承载 20,000 条固定高度摘要时的资源和响应边界。
//!
//! 测试默认不执行，必须在 Release 模式下通过测量脚本显式运行；这样不会把长时间性能实验混入普通回归套件。

use clipboard_board::{AppWindow, ClipboardCard};
use slint::{ComponentHandle, Model, ModelRc, ModelTracker, SharedString, VecModel};
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

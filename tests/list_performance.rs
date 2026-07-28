//! 此集成测试是 ATOM-14 的大列表性能探针，验证当前 Slint 看板承载 20,000 条固定高度摘要时的资源和响应边界。
//!
//! 测试默认不执行，必须在 Release 模式下通过测量脚本显式运行；这样不会把长时间性能实验混入普通回归套件。

use clipboard_board::{AppWindow, ClipboardCard};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::time::{Duration, Instant};

/// ATOM-14 规定的固定高度摘要规模。
const LIST_ITEM_COUNT: usize = 20_000;
/// 首次呼出只展示的最小批次，用于隔离首屏数据装载成本。
const FIRST_BATCH_COUNT: usize = 30;
/// 重复呼出次数；排序后取 P95，降低单次调度抖动对结论的影响。
const OPEN_SAMPLE_COUNT: usize = 30;
/// 长滚动探针的窗口替换次数；当前 UI 若没有滚动容器，会明确输出不支持而不是伪造通过。
const LONG_SCROLL_SAMPLE_COUNT: usize = 200;

/// 生成短文本卡片，避免性能探针被超长正文的内存分配主导。
fn generate_cards(count: usize) -> Vec<ClipboardCard> {
    (0..count)
        .map(|index| ClipboardCard {
            preview: SharedString::from(format!("固定高度摘要 #{index:05}：性能探针文本")),
            source: SharedString::from("性能探针"),
            relative_time: SharedString::from("刚刚"),
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
    let full_open_p95 = percentile_95(&mut full_open_samples.clone());

    // 保留一个完整模型和窗口实例，避免“显示后立即释放”掩盖列表实际驻留内存。
    let retained_window = AppWindow::new().expect("性能探针必须能够创建驻留看板");
    retained_window.set_cards(ModelRc::new(VecModel::from(full_cards)));
    retained_window
        .show()
        .expect("性能探针必须能够显示驻留看板");
    let retained_working_set = working_set_bytes();

    let first_batch_open = measure_open(generate_cards(FIRST_BATCH_COUNT));

    // 当前生产看板是 VerticalLayout，没有 ScrollView 或窗口化 Model；必须诚实报告不可滚动。
    let long_scroll_supported = false;
    let long_scroll_p95 = if long_scroll_supported {
        let mut samples = Vec::with_capacity(LONG_SCROLL_SAMPLE_COUNT);
        for offset in 0..LONG_SCROLL_SAMPLE_COUNT {
            let started_at = Instant::now();
            let window_cards = generate_cards(50);
            retained_window.set_cards(ModelRc::new(VecModel::from(window_cards)));
            let _ = offset;
            samples.push(started_at.elapsed());
        }
        Some(percentile_95(&mut samples))
    } else {
        None
    };

    println!(
        "ATOM14_RESULT item_count={} open_p95_ms={:.3} first_batch_ms={:.3} working_set_mib={:.3} long_scroll_supported={} long_scroll_p95_ms={}",
        LIST_ITEM_COUNT,
        full_open_p95.as_secs_f64() * 1000.0,
        first_batch_open.as_secs_f64() * 1000.0,
        retained_working_set as f64 / (1024.0 * 1024.0),
        long_scroll_supported,
        long_scroll_p95
            .map(|duration| format!("{:.3}", duration.as_secs_f64() * 1000.0))
            .unwrap_or_else(|| "NA".to_owned()),
    );

    retained_window
        .hide()
        .expect("性能探针必须能够隐藏驻留看板");
}

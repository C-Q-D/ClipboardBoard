//! 此集成测试验证后台线程提交的 UI 事件只会在 Slint 事件循环线程更新状态。
//!
//! 测试后端只在这个独立测试二进制中初始化一次，避免 Slint 全局平台重复初始化。

use clipboard_board::app::{post_ui_event, ui_state_snapshot};
use clipboard_board::command::{UiClipboardItem, UiEvent, UiSnapshot};
use std::sync::mpsc;
use std::thread;

/// 编译期约束跨线程 DTO 必须拥有 Send 和 Sync 能力。
fn assert_send_sync<T: Send + Sync>() {}

/// 后台线程投递事件后，事件循环线程才可以观察到 reducer 更新。
#[test]
fn 后台事件在事件循环线程更新状态() {
    i_slint_backend_testing::init_integration_test_with_mock_time();
    assert_send_sync::<UiEvent>();
    assert_send_sync::<UiSnapshot>();
    assert_send_sync::<UiClipboardItem>();

    let ui_thread_id = thread::current().id();
    let (worker_result_sender, worker_result_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let worker_thread_id = thread::current().id();
        let event = UiEvent::ReplaceSnapshot(UiSnapshot {
            items: vec![UiClipboardItem {
                id: 7,
                preview: "来自后台线程的结果".to_owned(),
                source: "测试来源".to_owned(),
                relative_time: "刚刚".to_owned(),
                content_hash: [7; 32],
                copy_count: 1,
                is_pinned: false,
            }],
            selected_index: Some(0),
        });
        let result = post_ui_event(event);
        worker_result_sender
            .send((worker_thread_id, result))
            .expect("测试结果接收端不应提前关闭");
    });

    let (worker_thread_id, dispatch_result) = worker_result_receiver
        .recv()
        .expect("后台线程必须返回投递结果");
    worker.join().expect("后台投递线程不应发生 panic");
    assert_ne!(worker_thread_id, ui_thread_id);
    assert!(dispatch_result.is_ok());

    let (ack_sender, ack_receiver) = mpsc::channel();
    slint::invoke_from_event_loop(move || {
        let state = ui_state_snapshot();
        ack_sender
            .send((thread::current().id(), state))
            .expect("测试确认接收端不应提前关闭");
        slint::quit_event_loop().expect("测试事件循环应该允许退出");
    })
    .expect("确认回调必须能够进入事件循环");

    slint::run_event_loop().expect("测试事件循环应该正常结束");
    let (applied_thread_id, state) = ack_receiver
        .recv()
        .expect("事件循环必须返回 reducer 的确认结果");

    assert_eq!(applied_thread_id, ui_thread_id);
    assert_eq!(state.applied_on_thread, Some(ui_thread_id));
    assert_eq!(state.applied_event_count, 1);
    assert_eq!(state.snapshot.items[0].preview, "来自后台线程的结果");
    assert_eq!(state.snapshot.selected_index, Some(0));
}

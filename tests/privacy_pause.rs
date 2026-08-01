//! 此集成测试使用显式配置根和假时钟验证暂停持久化，不访问真实剪贴板、托盘或默认目录。

use std::fs;
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clipboard_board::privacy::{
    PauseClock, PauseCommand, PauseStatus, PauseTimeError, PrivacyRuntimeOwner,
    SettingsClientRpcAdapter,
};
use clipboard_board::settings::{RecordingPause, SettingsWorker};

/// 测试临时根 token。
static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

/// 独占创建仅属于 ATOM-45 的配置根。
fn temporary_directory(label: &str) -> PathBuf {
    for _ in 0..64 {
        let token = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "clipboard-board-atom45-privacy-runtime-{nanos}-{}-{token}-{label}",
            process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("创建隐私测试根失败：{error}"),
        }
    }
    panic!("隐私测试根持续碰撞");
}

/// 原子可推进的测试双时钟。
struct ManualClock {
    wall: AtomicU64,
    monotonic: AtomicU64,
}

impl ManualClock {
    fn new(wall: u64) -> Self {
        Self {
            wall: AtomicU64::new(wall),
            monotonic: AtomicU64::new(0),
        }
    }
}

impl PauseClock for ManualClock {
    fn wall_now_millis(&self) -> Result<u64, PauseTimeError> {
        Ok(self.wall.load(Ordering::Acquire))
    }

    fn monotonic_now(&self) -> Duration {
        Duration::from_millis(self.monotonic.load(Ordering::Acquire))
    }
}

/// 在有限时间内等待异步 controller 达到目标状态。
fn wait_until(mut predicate: impl FnMut() -> bool) {
    for _ in 0..200 {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("暂停 controller 未在有限时间内达到目标状态");
}

/// 无限暂停通过生产 SettingsClient adapter 保存，关闭重启后仍保持。
#[test]
fn indefinite_pause_persists_across_restart() {
    let directory = temporary_directory("indefinite");
    let settings = SettingsWorker::start_at(&directory).unwrap();
    let observer = settings.client();
    let initial = observer.snapshot().unwrap();
    let adapter = SettingsClientRpcAdapter::new(settings.client());
    let runtime = PrivacyRuntimeOwner::start_with(
        settings,
        initial,
        Box::new(adapter),
        Arc::new(ManualClock::new(1_000_000)),
    )
    .unwrap();
    let sender = runtime.sender();
    sender.try_submit(PauseCommand::PauseIndefinitely).unwrap();
    wait_until(|| sender.status() == PauseStatus::PausedIndefinite);
    assert_eq!(
        observer
            .snapshot()
            .unwrap()
            .settings()
            .privacy
            .recording_pause,
        RecordingPause::Indefinite
    );
    runtime.stop().unwrap();

    let restarted = SettingsWorker::start_at(&directory).unwrap();
    assert_eq!(
        restarted
            .client()
            .snapshot()
            .unwrap()
            .settings()
            .privacy
            .recording_pause,
        RecordingPause::Indefinite
    );
    let mut restarted = restarted;
    restarted.begin_closing().unwrap();
    restarted.finish_shutdown().unwrap();
    fs::remove_dir_all(directory).unwrap();
}

/// 四类动作共用 latest-wins 槽，异类快速点击最终采用三十分钟目标。
#[test]
fn heterogeneous_commands_converge_to_latest_target() {
    let directory = temporary_directory("latest");
    let clock = Arc::new(ManualClock::new(2_000_000));
    let settings = SettingsWorker::start_at(&directory).unwrap();
    let observer = settings.client();
    let initial = observer.snapshot().unwrap();
    let runtime = PrivacyRuntimeOwner::start_with(
        settings,
        initial,
        Box::new(SettingsClientRpcAdapter::new(observer.clone())),
        clock,
    )
    .unwrap();
    let sender = runtime.sender();
    sender.try_submit(PauseCommand::PauseFiveMinutes).unwrap();
    sender.try_submit(PauseCommand::Resume).unwrap();
    sender.try_submit(PauseCommand::PauseThirtyMinutes).unwrap();
    wait_until(|| {
        observer.snapshot().is_ok_and(|snapshot| {
            snapshot.settings().privacy.recording_pause
                == RecordingPause::UntilUnixMillis(3_800_000)
        })
    });
    assert_eq!(sender.status(), PauseStatus::PausedTimed);
    runtime.stop().unwrap();
    fs::remove_dir_all(directory).unwrap();
}

/// 顶层、history 和 privacy 未知字段在保存新隐私状态后全部保留。
#[test]
fn recursive_unknown_fields_survive_privacy_save() {
    let directory = temporary_directory("unknown");
    fs::write(
        directory.join("settings.json"),
        r#"{
          "schema_version": 1,
          "future_top": {"keep": 1},
          "history": {"future_history": {"keep": 2}},
          "privacy": {
            "recording_pause": {"mode": "active"},
            "future_privacy": {"keep": 3}
          }
        }"#,
    )
    .unwrap();
    let worker = SettingsWorker::start_at(&directory).unwrap();
    let client = worker.client();
    let snapshot = client.snapshot().unwrap();
    let mut next = snapshot.settings().clone();
    next.privacy.recording_pause = RecordingPause::Indefinite;
    client.save(snapshot.revision(), next).unwrap();
    let document: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("settings.json")).unwrap()).unwrap();
    assert_eq!(document["future_top"]["keep"], 1);
    assert_eq!(document["history"]["future_history"]["keep"], 2);
    assert_eq!(document["privacy"]["future_privacy"]["keep"], 3);
    assert_eq!(document["privacy"]["recording_pause"]["mode"], "indefinite");

    let mut worker = worker;
    worker.begin_closing().unwrap();
    worker.finish_shutdown().unwrap();
    fs::remove_dir_all(directory).unwrap();
}

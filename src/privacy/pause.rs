//! 此文件实现正文读取许可、暂停更新屏障和可注入的双时钟恢复语义。

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::clipboard::ClipboardReadError;
use crate::settings::RecordingPause;

/// 生产和测试共用的墙上/单调时钟边界。
pub trait PauseClock: Send + Sync + 'static {
    /// 返回 UTC Unix epoch 毫秒；系统时间非法时返回无正文错误。
    fn wall_now_millis(&self) -> Result<u64, PauseTimeError>;

    /// 返回从当前时钟实例原点起的单调时长。
    fn monotonic_now(&self) -> Duration;
}

/// 暂停时间无法安全表示。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PauseTimeError {
    /// 墙上时钟早于 Unix epoch。
    BeforeUnixEpoch,
    /// deadline 加法溢出。
    DeadlineOverflow,
}

/// 生产双时钟；`Instant` 只在当前进程内使用。
pub struct SystemPauseClock {
    /// 单调时钟原点。
    origin: Instant,
}

impl SystemPauseClock {
    /// 创建当前进程的生产时钟。
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemPauseClock {
    fn default() -> Self {
        Self::new()
    }
}

impl PauseClock for SystemPauseClock {
    fn wall_now_millis(&self) -> Result<u64, PauseTimeError> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| PauseTimeError::BeforeUnixEpoch)?
            .as_millis();
        u64::try_from(millis).map_err(|_| PauseTimeError::DeadlineOverflow)
    }

    fn monotonic_now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// 门禁当前是否允许正文读取。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GateMode {
    /// 正常记录。
    Active,
    /// 禁止构造 backend 或读取正文。
    Paused,
}

/// 门禁内部共享状态。
struct GateState {
    /// 权威运行时模式。
    mode: GateMode,
    /// 状态事务已关闭新许可，正在等待或持久化。
    update_pending: bool,
    /// 已取得 RAII 许可且尚未发布完结果的 reader 数。
    active_readers: usize,
}

/// 可跨线程共享的记录门禁。
#[derive(Clone)]
pub struct RecordingGate {
    /// 同一互斥状态同时线性化读取准入与更新。
    inner: Arc<GateInner>,
}

struct GateInner {
    /// 门禁状态。
    state: Mutex<GateState>,
    /// reader 归零或更新完成时唤醒等待者。
    wake: Condvar,
}

impl RecordingGate {
    /// 用明确初始模式创建门禁。
    pub fn new(mode: GateMode) -> Self {
        Self {
            inner: Arc::new(GateInner {
                state: Mutex::new(GateState {
                    mode,
                    update_pending: false,
                    active_readers: 0,
                }),
                wake: Condvar::new(),
            }),
        }
    }

    /// 在构造正文 backend 前尝试取得许可；更新中与暂停均立即拒绝。
    pub fn try_read(&self) -> Result<RecordingReadPermit, ClipboardReadError> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| ClipboardReadError::Paused)?;
        if state.update_pending || state.mode == GateMode::Paused {
            return Err(ClipboardReadError::Paused);
        }
        state.active_readers = state.active_readers.saturating_add(1);
        Ok(RecordingReadPermit { gate: self.clone() })
    }

    /// 先关闭新准入，再仅等待已经活动的 reader 释放许可。
    pub fn begin_update(&self) -> GateUpdate {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.update_pending = true;
        while state.active_readers != 0 {
            state = self
                .inner
                .wake
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
        GateUpdate {
            gate: self.clone(),
            finished: false,
        }
    }

    /// 返回不含正文的当前模式快照。
    pub fn mode(&self) -> GateMode {
        self.inner
            .state
            .lock()
            .map(|state| state.mode)
            .unwrap_or(GateMode::Paused)
    }
}

/// 覆盖正文读取到结果发布的 RAII 许可。
pub struct RecordingReadPermit {
    /// 归还 reader 计数所需门禁。
    gate: RecordingGate,
}

impl Drop for RecordingReadPermit {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.active_readers = state.active_readers.saturating_sub(1);
        if state.active_readers == 0 {
            self.gate.inner.wake.notify_all();
        }
    }
}

/// 独占状态更新令牌；Drop 默认 fail-closed。
pub struct GateUpdate {
    /// 被更新的门禁。
    gate: RecordingGate,
    /// 是否已经显式提交。
    finished: bool,
}

impl GateUpdate {
    /// 提交明确模式并重新允许准入判断。
    pub fn finish(mut self, mode: GateMode) {
        self.apply(mode);
        self.finished = true;
    }

    /// 在同一锁内提交模式。
    fn apply(&self, mode: GateMode) {
        let mut state = self
            .gate
            .inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.mode = mode;
        state.update_pending = false;
        self.gate.inner.wake.notify_all();
    }
}

impl Drop for GateUpdate {
    fn drop(&mut self) {
        if !self.finished {
            self.apply(GateMode::Paused);
        }
    }
}

/// 从持久化状态恢复运行时模式和可选单调 deadline。
pub fn restore_pause(
    pause: &RecordingPause,
    clock: &dyn PauseClock,
) -> Result<(GateMode, Option<Duration>), PauseTimeError> {
    match pause {
        RecordingPause::Active => Ok((GateMode::Active, None)),
        RecordingPause::Indefinite => Ok((GateMode::Paused, None)),
        RecordingPause::UntilUnixMillis(deadline) => {
            let wall_now = clock.wall_now_millis()?;
            if *deadline <= wall_now {
                return Ok((GateMode::Active, None));
            }
            let remaining = Duration::from_millis(*deadline - wall_now);
            let monotonic_deadline = clock
                .monotonic_now()
                .checked_add(remaining)
                .ok_or(PauseTimeError::DeadlineOverflow)?;
            Ok((GateMode::Paused, Some(monotonic_deadline)))
        }
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块用假时钟验证重启解释和 update_pending 读取屏障。

    use super::{restore_pause, GateMode, PauseClock, PauseTimeError, RecordingGate};
    use crate::settings::RecordingPause;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// 可由测试原子推进的双时钟。
    struct ManualClock {
        wall: AtomicU64,
        monotonic: AtomicU64,
    }

    impl PauseClock for ManualClock {
        fn wall_now_millis(&self) -> Result<u64, PauseTimeError> {
            Ok(self.wall.load(Ordering::Acquire))
        }

        fn monotonic_now(&self) -> Duration {
            Duration::from_millis(self.monotonic.load(Ordering::Acquire))
        }
    }

    /// timed 重启只按当前 UTC deadline 解释，墙钟回拨会继续暂停。
    #[test]
    fn 恢复语义明确保留墙钟回拨限制() {
        let clock = ManualClock {
            wall: AtomicU64::new(1_000),
            monotonic: AtomicU64::new(50),
        };
        assert_eq!(
            restore_pause(&RecordingPause::UntilUnixMillis(1_500), &clock).unwrap(),
            (GateMode::Paused, Some(Duration::from_millis(550)))
        );
        clock.wall.store(2_000, Ordering::Release);
        assert_eq!(
            restore_pause(&RecordingPause::UntilUnixMillis(1_500), &clock).unwrap(),
            (GateMode::Active, None)
        );
        clock.wall.store(500, Ordering::Release);
        assert_eq!(
            restore_pause(&RecordingPause::UntilUnixMillis(1_500), &clock)
                .unwrap()
                .0,
            GateMode::Paused
        );
        assert_eq!(
            restore_pause(&RecordingPause::Indefinite, &clock).unwrap(),
            (GateMode::Paused, None)
        );
    }

    /// update_pending 立即拒绝 B，只等待已取得许可的 A 完成。
    #[test]
    fn 更新屏障只排空活动reader并立即拒绝新许可() {
        let gate = RecordingGate::new(GateMode::Active);
        let permit_a = gate.try_read().expect("A 应取得读取许可");
        let update_gate = gate.clone();
        let (started_sender, started_receiver) = mpsc::channel();
        let updater = thread::spawn(move || {
            started_sender.send(()).unwrap();
            update_gate.begin_update().finish(GateMode::Paused);
        });
        started_receiver.recv().unwrap();

        for _ in 0..10_000 {
            if gate.try_read().is_err() {
                break;
            }
            thread::yield_now();
        }
        assert!(gate.try_read().is_err(), "B 必须在 A 释放前立即被拒绝");
        drop(permit_a);
        updater.join().unwrap();
        assert_eq!(gate.mode(), GateMode::Paused);
    }
}

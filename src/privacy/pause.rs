//! 此文件实现正文读取许可、暂停更新屏障和可注入的双时钟恢复语义。

use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::clipboard::ClipboardReadError;
use crate::platform::windows::{ProcessSource, ProcessSourceSnapshot};
use crate::settings::{
    normalize_excluded_app_rule, normalize_process_image_path, RecordingPause, MAX_EXCLUDED_APPS,
};

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

/// 规则解析失败时的稳定错误；不携带用户输入，避免错误诊断回显路径。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExcludedAppsError {
    /// 至少一条规则为空、超限或违反 Windows 路径边界。
    InvalidRule,
}

impl fmt::Display for ExcludedAppsError {
    /// 只返回固定错误，不输出原始规则。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRule => write!(formatter, "排除程序规则无效"),
        }
    }
}

impl std::error::Error for ExcludedAppsError {}

/// 一次运行时使用的不可变排除规则快照；规则正文不会出现在 Debug 中。
#[derive(Clone, Eq, PartialEq)]
pub struct ExcludedAppsSnapshot {
    rules: Arc<[ExcludedAppRule]>,
}

/// 规则匹配的两种精确形式。
#[derive(Clone, Eq, PartialEq)]
enum ExcludedAppRule {
    /// 只匹配来源映像最终文件名。
    Basename(String),
    /// 只匹配规范化绝对 DOS/UNC 映像路径。
    AbsolutePath(String),
}

impl fmt::Debug for ExcludedAppsSnapshot {
    /// 仅输出规则数量，禁止泄露用户配置的程序名称或路径。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExcludedAppsSnapshot")
            .field("rule_count", &self.rules.len())
            .finish()
    }
}

impl Default for ExcludedAppsSnapshot {
    /// 默认不排除任何来源程序。
    fn default() -> Self {
        Self {
            rules: Arc::from([]),
        }
    }
}

impl ExcludedAppsSnapshot {
    /// 从持久化字符串构造去重后的不可变快照，保留首次出现顺序。
    pub fn from_rules(rules: &[String]) -> Result<Self, ExcludedAppsError> {
        if rules.len() > MAX_EXCLUDED_APPS {
            return Err(ExcludedAppsError::InvalidRule);
        }
        let mut normalized = Vec::with_capacity(rules.len());
        for raw in rules {
            let value = normalize_excluded_app_rule(raw).ok_or(ExcludedAppsError::InvalidRule)?;
            let rule = if value.contains('\\') || value.as_bytes().get(1) == Some(&b':') {
                ExcludedAppRule::AbsolutePath(value)
            } else {
                ExcludedAppRule::Basename(value)
            };
            if !normalized.iter().any(|existing| same_rule(existing, &rule)) {
                normalized.push(rule);
            }
        }
        Ok(Self {
            rules: Arc::from(normalized),
        })
    }

    /// 返回规则数量，供设置摘要和确定性测试使用。
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// 返回规则快照是否为空，供调用方在不暴露规则正文的情况下判断。
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// 使用事件来源快照判断是否命中，不访问文件系统或剪贴板正文。
    pub fn matches(&self, source: Option<&ProcessSourceSnapshot>) -> bool {
        let executable = source.map(|value| basename(&value.source.executable));
        let normalized_path = source
            .and_then(|value| value.image_path.as_deref())
            .and_then(normalize_process_image_path);
        self.rules.iter().any(|rule| match rule {
            ExcludedAppRule::Basename(rule) => {
                executable.is_some_and(|value| same_text(rule, value))
            }
            ExcludedAppRule::AbsolutePath(rule) => normalized_path
                .as_deref()
                .is_some_and(|value| same_text(rule, value)),
        })
    }
}

/// 提取来源映像最终文件名；测试请求可传入完整路径而不改变历史来源 DTO。
fn basename(value: &str) -> &str {
    value.rsplit(['\\', '/']).next().unwrap_or(value)
}

/// Windows 序号忽略大小写比较；仅非 Windows 构建使用 Unicode fallback。
fn same_text(left: &str, right: &str) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Globalization::{CompareStringOrdinal, CSTR_EQUAL};
        let left: Vec<u16> = left.encode_utf16().collect();
        let right: Vec<u16> = right.encode_utf16().collect();
        unsafe {
            CompareStringOrdinal(
                left.as_ptr(),
                left.len() as i32,
                right.as_ptr(),
                right.len() as i32,
                1,
            ) == CSTR_EQUAL
        }
    }
    #[cfg(not(windows))]
    {
        left.to_lowercase() == right.to_lowercase()
    }
}

/// 比较两个规则的语义值，避免大小写差异造成重复快照项。
fn same_rule(left: &ExcludedAppRule, right: &ExcludedAppRule) -> bool {
    match (left, right) {
        (ExcludedAppRule::Basename(left), ExcludedAppRule::Basename(right))
        | (ExcludedAppRule::AbsolutePath(left), ExcludedAppRule::AbsolutePath(right)) => {
            same_text(left, right)
        }
        _ => false,
    }
}

/// 门禁内部共享状态。
struct GateState {
    /// 权威运行时模式。
    mode: GateMode,
    excluded_apps: ExcludedAppsSnapshot,
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
        Self::new_with_excluded_apps(mode, ExcludedAppsSnapshot::default())
    }

    /// 从启动配置快照创建门禁，规则正文只保留在受控内存中。
    pub fn new_with_excluded_apps(mode: GateMode, excluded_apps: ExcludedAppsSnapshot) -> Self {
        Self {
            inner: Arc::new(GateInner {
                state: Mutex::new(GateState {
                    mode,
                    excluded_apps,
                    update_pending: false,
                    active_readers: 0,
                }),
                wake: Condvar::new(),
            }),
        }
    }

    /// 在构造正文 backend 前尝试取得许可；更新中与暂停均立即拒绝。
    pub fn try_read(&self) -> Result<RecordingReadPermit, ClipboardReadError> {
        self.try_read_for_snapshot(None)
    }

    /// 兼容无路径来源调用；内部仍转入同一个快照门禁，避免复制第二套判断。
    pub fn try_read_for_source(
        &self,
        source: Option<&ProcessSource>,
        image_path: Option<&str>,
    ) -> Result<RecordingReadPermit, ClipboardReadError> {
        let snapshot = source.map(|value| ProcessSourceSnapshot {
            source: value.clone(),
            image_path: image_path.map(str::to_owned),
        });
        self.try_read_for_snapshot(snapshot.as_ref())
    }

    /// 先检查暂停，再匹配请求级来源快照；成功时才增加 active reader 计数。
    pub fn try_read_for_snapshot(
        &self,
        source: Option<&ProcessSourceSnapshot>,
    ) -> Result<RecordingReadPermit, ClipboardReadError> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| ClipboardReadError::Paused)?;
        if state.update_pending || state.mode == GateMode::Paused {
            return Err(ClipboardReadError::Paused);
        }
        if source.is_some_and(|value| !value.is_safe_for_rules()) {
            // 来源路径无法安全转换时不能让 basename 规则被绕过；在读取正文前拒绝。
            return Err(ClipboardReadError::ExcludedApp);
        }
        if state.excluded_apps.matches(source) {
            return Err(ClipboardReadError::ExcludedApp);
        }
        state.active_readers = state.active_readers.saturating_add(1);
        Ok(RecordingReadPermit { gate: self.clone() })
    }

    /// 先关闭新准入，再仅等待已经活动的 reader 释放许可。
    pub fn begin_update(&self) -> GateUpdate {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        // 更新令牌本身也必须串行化；否则两个调用者都可能在同一屏障内读取旧模式，
        // 后完成者会覆盖先完成者的暂停状态。
        while state.update_pending {
            state = self
                .inner
                .wake
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
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

    /// 在线性化更新屏障内替换规则，暂停模式保持当前权威值。
    pub fn replace_excluded_apps(&self, excluded_apps: ExcludedAppsSnapshot) {
        let update = self.begin_update();
        update.finish_preserving_current_mode(excluded_apps);
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
        self.apply(mode, None);
        self.finished = true;
    }

    /// 在同一锁内提交模式和新的规则快照。
    pub fn finish_with_excluded_apps(
        mut self,
        mode: GateMode,
        excluded_apps: ExcludedAppsSnapshot,
    ) {
        self.apply(mode, Some(excluded_apps));
        self.finished = true;
    }

    /// 在同一锁内读取当前模式并替换规则，避免与并发暂停更新交错覆盖状态。
    pub fn finish_preserving_current_mode(mut self, excluded_apps: ExcludedAppsSnapshot) {
        let mut state = self
            .gate
            .inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.excluded_apps = excluded_apps;
        state.update_pending = false;
        self.gate.inner.wake.notify_all();
        self.finished = true;
    }

    /// 在同一锁内提交模式；未提供规则时保留原规则。
    fn apply(&self, mode: GateMode, excluded_apps: Option<ExcludedAppsSnapshot>) {
        let mut state = self
            .gate
            .inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.mode = mode;
        if let Some(excluded_apps) = excluded_apps {
            state.excluded_apps = excluded_apps;
        }
        state.update_pending = false;
        self.gate.inner.wake.notify_all();
    }
}

impl Drop for GateUpdate {
    fn drop(&mut self) {
        if !self.finished {
            self.apply(GateMode::Paused, None);
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

    use super::{
        restore_pause, ExcludedAppsSnapshot, GateMode, PauseClock, PauseTimeError, RecordingGate,
    };
    use crate::platform::windows::{ProcessSource, ProcessSourceSnapshot};
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

    /// 规则替换必须等待已有 GateUpdate，并在同锁内保留并发暂停提交的模式。
    #[test]
    fn 规则替换与暂停更新不会交错打开门禁() {
        let gate = RecordingGate::new(GateMode::Active);
        let first = gate.begin_update();
        let replacing = gate.clone();
        let next_rules = ExcludedAppsSnapshot::from_rules(&["secret.exe".to_owned()]).unwrap();
        let (started_sender, started_receiver) = mpsc::channel();
        let (finished_sender, finished_receiver) = mpsc::channel();
        let thread = thread::spawn(move || {
            started_sender.send(()).unwrap();
            replacing.replace_excluded_apps(next_rules);
            finished_sender.send(()).unwrap();
        });
        started_receiver.recv().unwrap();
        assert!(
            finished_receiver
                .recv_timeout(Duration::from_millis(10))
                .is_err(),
            "第二个更新必须等待第一个令牌提交"
        );
        first.finish(GateMode::Paused);
        thread.join().unwrap();
        assert_eq!(gate.mode(), GateMode::Paused);
        let source = ProcessSourceSnapshot::from(ProcessSource {
            executable: "secret.exe".to_owned(),
            display_name: "测试来源".to_owned(),
            process_id: 1,
        });
        assert!(gate.try_read_for_snapshot(Some(&source)).is_err());
    }
}

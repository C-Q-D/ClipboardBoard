# ATOM-45 暂停剪贴板记录原子计划

## 计划元数据

- 计划 ID：ATOM-45
- 类型：atomic-development
- 修订版本：7
- 状态：complete
- 父级 ID：WCB-PLAN-001 / UNIT-13
- 风险等级：L3
- 创建基线：`d3640179bac208db9dc0b3e3bb36d301044c7486`
- 计划提交：`feat(privacy): [ATOM-45] 支持暂停剪贴板记录`

### 修订记录

- 修订 2：按编码前 DDD `ddd_atom45_precode` 的 `REVISE_PLAN` 结论，补齐按方向事务、
  `update_pending` 关闭准入、RPC helper 与单调 timer 分离、`Paused` reader 结果、
  全局 latest-wins 托盘命令、`Reconciling` 状态、UTC 回拨限制和统一资源 owner。
- 修订 3：按第二轮编码前 DDD 的 `REVISE_PLAN` 结论，增加可注入
  `SettingsRpcPort`、固定单一关闭依赖顺序与资源唯一所有权，并把跨持久化、并发门禁和
  Windows 生命周期的风险等级提升为 L3。
- 修订 4：消除 controller/RPC helper 所有权和关闭顺序的两处旧表述，记录根计划 L3
  回写证据。
- 修订 5：按最终 DDD 补齐 `main` 启动失败与正常退出的统一 `RuntimeCleanup`，并明确
  捕获 inbox 必须在结果泵 join 前关闭；controller 后续 RPC 入队失败也统一收敛为
  `Reconciling`。
- 修订 6：补齐 `Reconciling` 前态下初始 Resume/新命令入队失败的 fail-closed 保护，
  并将托盘对账状态改为允许最新命令覆盖；Clipboard capture Debug 改为类型/长度摘要。
- 修订 7：完成提交前 DDD，确认 `RuntimeCleanup`、事务在途状态、RPC 失败收敛、
  `Reconciling` fail-closed、托盘可恢复性和诊断脱敏均有窄验证证据；原子状态改为
  `complete`。

主 Agent 已把共享根计划中的 ATOM-45 风险等级同步提升为 L3，并提交、推送
`bf79b6eb21d48f57e829c7ade58ff2e853766b6f`。该提交不在本 Worker 的旧 worktree
基线中，本 Worker 只记录证据，不 merge、cherry-pick 或修改共享根计划。

## 执行声明与依赖证明

- 原子状态：complete，已完成本地验证，等待主 Agent 合并。
- 分支：`codex/atom45-46-privacy-runtime`。
- worktree：
  `F:\workspace\small-projects\windows-copy-worktrees\privacy-runtime`。
- `base_main_sha`：
  `d3640179bac208db9dc0b3e3bb36d301044c7486`。
- 硬依赖：ATOM-44 配置持久化。
- ATOM-44 主线提交：
  `d3640179bac208db9dc0b3e3bb36d301044c7486`。
- 已执行
  `git merge-base --is-ancestor d3640179bac208db9dc0b3e3bb36d301044c7486 HEAD`，
  退出码为 `0`；当前 `HEAD` 也精确等于该提交，因此依赖已进入本 worktree 基线。
- 远端约束：只创建本地原子提交，不设置 upstream、不 push、不创建远端分支。

## 唯一目标

用户通过系统托盘把剪贴板记录暂停 5 分钟、30 分钟或无限期；暂停门禁必须位于
ClipboardIO 读取文本或图片正文之前。定时暂停到期自动恢复，无限暂停跨进程重启保持。

本原子只实现暂停策略、持久化、托盘命令和读取前门禁，不实现排除程序、设置页面、
托盘图标换色、通知气泡、快捷键修改、历史清理或新的剪贴板格式。

## 已确认的现有接缝

### 配置

- ATOM-44 已提供 `AppSettings`、`SettingsSnapshot`、`SettingsClient` 和
  `SettingsWorker`。
- `SettingsClient::snapshot/save` 是可能阻塞的同步接口，公开注释已经禁止从 Slint
  回调直接调用。
- `save` 使用进程内 revision compare-and-save；冲突、回执丢失和 worker 关闭均有稳定
  错误。
- 当前 `AppSettings` 只有 `history`；持久化层保留顶层及 `history` 未知字段。

### ClipboardIO

- `WM_CLIPBOARDUPDATE` 只在消息线程捕获 sequence 和来源，然后调用
  `ClipboardIoWorker::request_capture`。
- `ClipboardIoWorker` 采用容量一 latest-wins 队列；真正的
  `read_capture_payload_with_backend` 在 worker 线程执行，并统一读取文本或图片。
- 捕获成功后才把拥有型正文发布到 `ClipboardCaptureInbox`，历史泵随后写入 SQLite。
- 因此门禁必须进入 worker 主循环、紧邻 backend 正文读取之前；只在消息窗口、历史泵
  或 SQLite 前过滤都太晚。

### 托盘与生命周期

- 托盘与热键、剪贴板 listener 共用 message-only HWND 线程；当前菜单只有“打开/退出”。
- 托盘回调当前同步创建菜单并把 UI 动作投递给 `post_ui_event`，不适合直接调用阻塞的
  `SettingsClient`。
- `HotkeyManager::stop` 令消息线程先注销 listener，再停止并 join ClipboardIO worker。
- `main` 当前在 UI 事件循环前创建所有 worker，退出时先停止热键/捕获入口和业务线程，
  最后关闭 SQLite；ATOM-45 必须把隐私 worker 与 SettingsWorker 纳入同一显式清理链。

## 目标模型

### 持久化 DTO

在 `AppSettings` 增加：

```text
privacy: PrivacySettings
└── recording_pause: RecordingPause
    ├── Active
    ├── UntilUnixMillis(u64)
    └── Indefinite
```

- 默认值为 `Active`。
- JSON 使用稳定、显式 tagged 表示，不依赖 Rust 枚举序号。
- `UntilUnixMillis` 是 UTC Unix epoch 毫秒，只用于跨重启恢复；托盘命令只会生成
  “当前时间 + 5 分钟”或“当前时间 + 30 分钟”。
- `Indefinite` 必须持久化，重启后仍关闭读取门禁。
- 缺少 `privacy` 或 `recording_pause` 时使用默认值，兼容 ATOM-44 文件。
- 顶层未知字段、`history` 未知字段、`privacy` 未知字段都必须继续保留；持久化合并改为
  对完整 typed DTO 做对象递归合并，当前所有已知键覆盖旧同名 raw，未知键保留。
- 非法 tagged 值、非对象 privacy、非整数/负数/分数 deadline 均使该副本按 ATOM-44
  统一规则成为 `Corrupt`，不部分采用。

### 时钟与重启语义

- 运行时使用可注入时钟，生产实现同时提供墙上时钟和单调时钟。
- 接收 5/30 分钟命令时：
  - 持久化 deadline 由墙上时钟计算；
  - 当前进程到期调度使用单调时钟，系统时间向前或向后跳变都不缩短或延长本次已确认的
    5/30 分钟。
- 重启后无法恢复旧 `Instant`，按持久化 UTC deadline 与当前墙上时钟比较：
  - deadline 在未来：以剩余时长重建单调 deadline；
  - deadline 已到或墙上时钟恰好等于 deadline：立即视为 `Active`；
  - 无限暂停：保持 `Indefinite`。
- 系统时间早于 Unix epoch、加法溢出或 deadline 无法转换为运行时 duration 时返回无正文
  错误，不开启一个不可证明的暂停。
- UTC deadline 固有依赖重启时的墙上时钟。若设备关机期间或重启前把时钟向后调整，
  timed pause 可能比用户最初选择的实际时长更久；本原子不声称这种情况下必然立即
  Active，也不引入可信网络时钟。测试必须锁定这一限制，而非伪造无法证明的保证。
- 定时到期以运行时门禁恢复为优先，不因配置清理写入失败而无限延长暂停；正常前进的
  墙上时钟会在下次重启时把旧 deadline 解释为 Active，墙钟回拨限制仍按上一条处理。
  配置归一化失败只记录无正文诊断并允许后续命令重试。

## 正文读取前门禁

建立独立深模块 `privacy`，核心类型为 `RecordingGate`：

- `RecordingGate` 是 `Arc` 共享、内部使用同一个 mutex 和 condition variable 的状态；
  状态至少包含 `mode`、`update_pending` 和 `active_readers`，不携带正文、窗口句柄或
  配置文件句柄。
- 任意状态更新必须先在同一 mutex 内设置 `update_pending=true`；从这一瞬间开始，
  新读取许可立即返回 `ClipboardReadError::Paused`，不能排在旧 reader 后等待。
- 更新方随后只等待已经取得许可的 `active_readers` 降到 0，不等待尚未取得许可的请求。
- ClipboardIO worker 必须在构造 Win32 backend 或测试 reader factory 之前取得许可。
- RAII 读取许可覆盖 backend/factory 构造、正文读取、捕获结果形成和
  `ClipboardCaptureInbox::publish`；Drop 在同一 mutex 内减少 `active_readers` 并唤醒
  更新方，异常路径也不能泄漏 reader 计数。
- 暂停命令完成线性化后：
  - 尚未开始的 latest-wins 请求不得调用任何正文 backend；
  - 后续新请求可进入容量一队列，但 worker 只返回 `Paused`/丢弃结果，不读取文本或图片；
  - inbox 不发布正文，历史泵没有可保存内容。
- 已在暂停线性化点前取得读取许可的唯一在途捕获允许完成；暂停 worker 必须等它退出
  许可区后才报告命令完成。这样“命令完成后没有迟到正文写入”可由同步测试证明。
- 确定性并发测试必须建立 A 已取得许可且阻塞、B 已在队列等待的布局；更新方置
  `update_pending` 后释放 A，断言 A 先完成发布、B 随后得到 Paused 且 backend/factory
  构造次数为 0。
- 恢复命令打开门禁后，只有下一次真实 `WM_CLIPBOARDUPDATE` 才触发捕获；本原子不主动
  重读暂停期间的当前剪贴板，避免恢复时补录敏感内容。
- ATOM-46 后续在同一读取许可前追加来源排除判断，不复制第二套正文门禁。

## 暂停控制 worker 与配置 revision

新增专用 `PauseController`/`PauseCommandSender`：

- 托盘线程只向全局容量 1、跨四类动作 latest-wins 的命令槽非阻塞投递以下拥有型命令，
  不等待配置 IO：
  `PauseFiveMinutes`、`PauseThirtyMinutes`、`PauseIndefinitely`、`Resume`。
- 无论动作是否同类，新动作都原子替换尚未开始的旧动作；已经进入事务的动作不被取消。
  `try_submit` 成功只表示动作被槽接收，不表示门禁或配置已经改变。关闭后稳定拒绝新命令。
- controller 暴露轻量只读 `PauseStatus`：
  `Active`、`PausedTimed`、`PausedIndefinite`、`Updating`、`Reconciling`。托盘只读取该状态
  决定菜单启用或提示，不调用 `SettingsClient`，状态中不含正文或 JSON。
- controller worker 独占：
  - 当前 `SettingsSnapshot`；
  - 到期计时与门禁排他更新；
  - 暂停配置字段的事务状态机。
- 阻塞的 `SettingsClient::save/snapshot` 不在 gate/timer controller 线程执行。单独的
  串行 settings RPC helper 接收请求并异步回传拥有型结果；controller 在 RPC 在途时仍
  能被命令、假时钟到期和关闭唤醒。
- privacy/controller 边界定义对象安全、可注入的 `SettingsRpcPort`，只暴露：
  - `snapshot() -> Result<SettingsSnapshot, SettingsError>`；
  - `save(expected_revision, AppSettings) -> Result<SettingsSnapshot, SettingsError>`。
  参数和返回值均为拥有型或克隆快照，不暴露 channel、文件路径、JSON 或 worker 句柄。
- 生产 `SettingsClientRpcAdapter` 是唯一允许委托 `SettingsClient` 的实现，只做一对一
  转发，不缓存 revision、不改变错误、不自行重试。settings RPC helper 独占
  `Box<dyn SettingsRpcPort + Send>` 并执行阻塞调用；controller 只持权威
  `SettingsSnapshot`、gate、timer、PauseStatus 和异步 helper 通道，不能直接取得
  `SettingsClient`。
- 测试 fake port 可以按脚本返回成功、`RevisionConflict`、`OutcomeUnknown`、snapshot
  失败或永久阻塞直到栅栏释放，用于确定性覆盖对账与 timer/RPC 竞争；另有生产 adapter
  窄测试验证 snapshot/save 的参数、结果和错误原样转发，结构检查证明 controller 类型
  中没有 `SettingsClient` 字段。
- **Active → Pause** 的方向事务：
  1. 记录当前权威 snapshot 为回滚基线；
  2. 在 gate 同一 mutex 内设置 `update_pending`，立即关闭新许可并只排空在途 reader；
  3. 通过 RPC helper 持久化目标 Pause；
  4. 权威 snapshot 确认目标 Pause 后提交 Closed gate 与 Paused 状态；
  5. 确定失败时按“最新权威回滚基线”恢复 gate，而不是盲目恢复事务开始时的旧值。
- **Pause → Active** 的方向事务：
  1. gate 保持 Closed/update_pending，恢复命令不能先开门；
  2. 通过 RPC helper 持久化 Active；
  3. 只有权威 snapshot 明确确认 Active 后才打开 gate。
- **Pause → 另一 Pause** 同样保持关门，只更新 deadline/mode；**Active → Active**
  是幂等命令。
- `RevisionConflict` 时调用 `snapshot()` 取得最新完整配置，把本次目标 privacy 字段
  合并到新快照后有界重试；绝不能用旧完整 DTO 覆盖其他设置。每次刷新成功都必须同时
  更新回滚基线，使最终确定失败恢复最新权威状态。
- `OutcomeUnknown` 时必须先 `snapshot()` 对账：
  - 权威快照已包含目标暂停状态，按成功提交；
  - 否则使用权威 revision 有界重试。
- `OutcomeUnknown` 后连权威 snapshot 也不可读取时，进入
  `ReconciliationRequired`/`PauseStatus::Reconciling` 并 fail-closed；不得猜测 Active
  或恢复旧开门状态。后续 controller/helper 重试对账，只有权威 Active 才开门。
- 冲突连续超过固定小次数或其他确定保存失败时按最新权威基线恢复；无法取得权威基线时
  同样进入 Reconciling 并保持关门，不无限自旋。
- 定时器和 gate controller 不得被 settings RPC 阻塞。timer wait 可被假时钟推进、
  新命令和关闭动作立即唤醒。
- timed pause 的单调 deadline 到达时必须恢复 Active，即使旧 Pause 保存仍在 RPC
  helper 中；这是“Pause → Active 必须权威确认”规则的唯一到期例外，因为运行时已确认
  的有限暂停不能被慢磁盘无限延长。旧 Pause RPC 晚到成功时，只排队 Active 归一化，
  不得重新关闭 gate；归一化失败也不得重新关门。
- controller 关闭顺序为：拒绝新托盘命令、取消计时等待并 stop + join controller
  恰好一次；controller 不等待仍在 helper 中的阻塞 RPC。随后 stop + join settings RPC
  helper 恰好一次，由 helper 收敛已准入调用；最后才允许关闭 SettingsWorker。

## 托盘命令

菜单扩展为：

```text
打开
────────
暂停记录 5 分钟
暂停记录 30 分钟
暂停记录（无限期）
恢复记录
────────
退出
```

- 暂停命令和恢复命令使用固定、互不重叠的 Win32 menu ID。
- `TrackPopupMenu` 返回值先转换为不含系统资源的 `TrayAction`；打开/退出继续投递
  `UiEvent`，暂停相关 action 交给 `PauseCommandSender`。
- 当前已暂停时仍允许提交新的 5/30/无限命令，以最新命令重新定义期限；恢复在 Active
  状态为幂等操作。
- 菜单读取 `PauseStatus` 只用于展示/启用状态；`Updating` 与 `Reconciling` 不得触发
  同步查询。用户点击后菜单关闭不代表命令已经完成。
- 本原子不在菜单中显示剩余秒数，不启动周期 UI 刷新，不修改托盘图标。
- 菜单构造、取消和销毁失败保持现有资源清理协议；配置保存错误不得阻塞或破坏 Shell
  消息循环。

## 启动与关闭接线

新增集中生命周期所有者 `PrivacyRuntimeOwner`，对 SettingsWorker、PauseController 和
settings RPC helper 各保持唯一一个可 `take` 的所有权槽；外部只能取得 gate、命令
sender 和只读 status。HotkeyManager、capture pump 和其他业务 worker 仍由应用 runtime
拥有，但统一关闭协调器对每项资源也只能 take/join 一次，错误路径不得再次 join。
HotkeyManager 仍独占 message-only window/ClipboardIO/tray。生产启动顺序：

1. 单实例与诊断初始化。
2. 启动 `SettingsWorker` 并取得初始 snapshot。
3. 由 `PrivacyRuntimeOwner` 用配置和生产时钟建立 settings RPC helper、
   PauseController 与共享 `RecordingGate`。
4. 把 gate 和暂停命令发送端交给 `HotkeyManager`/message-only window。
5. message-only window 用同一 gate 启动 ClipboardIO worker，随后注册 listener 与托盘。
6. 继续现有 SQLite、图片、历史泵和 UI 启动流程。

任何后续启动步骤失败，都必须进入与正常退出相同的单一收敛函数，不在 `main` 的每个
分支复制顺序。唯一关闭依赖链固定为：

```text
关闭 UI copy / 业务 sender 入口
    ↓
HotkeyManager::stop
  注销 listener → stop/join ClipboardIO → close inbox
    ↓
join capture pump 与其他业务 worker
    ↓
PauseController stop + join（恰好一次）
    ↓
settings RPC helper stop + join（恰好一次）
    ↓
SettingsWorker begin_closing + finish_shutdown（恰好一次）
```

资源矩阵如下：

| 已创建资源 | 收敛顺序 |
|---|---|
| SettingsWorker | SettingsWorker |
| + RPC helper/controller | controller → RPC helper → SettingsWorker |
| + Hotkey/ClipboardIO/tray | 入口 → Hotkey/ClipboardIO/inbox → controller → RPC helper → SettingsWorker |
| + capture pump/业务 worker | 入口 → Hotkey/ClipboardIO/inbox → capture pump/业务 worker → controller → RPC helper → SettingsWorker |

测试用 fake handles 逐阶段注入启动失败，断言只关闭已创建资源、严格逆序、每个资源一次；
fake capture pump 必须阻塞等待 fake inbox close，只有 fake Hotkey stop 关闭 inbox 后才
允许 join 成功，以证明依赖链不会把 pump join 放到 inbox close 前造成死锁。不得访问
默认配置以外的额外位置。

正常关闭和任一启动失败都调用上述同一收敛函数：

1. UI Quit 或失败收敛先关闭 copy、查询、mutation 等业务 sender 入口。
2. `HotkeyManager::stop` 注销 clipboard listener，停止并 join ClipboardIO worker，
   随后 close inbox，阻止新的正文和唤醒。
3. inbox 已关闭后 join capture pump，再 join 其他已启动业务 worker。
4. PauseController stop + join 一次，取消 timer 并拒绝托盘命令；不等待 helper 的
   在途 RPC。
5. settings RPC helper stop + join 一次；已准入 RPC 先返回或按明确关闭错误完成。
6. `SettingsWorker::begin_closing/finish_shutdown` 一次。
7. 其余与隐私链无依赖的资源沿用现有关闭协议，但不得改变上述相对顺序。

SettingsWorker 不得先于 PauseController 关闭；否则到期归一化或最后一个已准入托盘命令
可能得到不确定结果。

本轮接线落地：`src/main.rs` 的 `RuntimeCleanup` 以 `Option` 槽唯一拥有已创建资源；所有
启动步骤使用同一 `?` 早退路径，由 `Drop` 调用与正常退出相同的 `stop`。收敛顺序固定为
业务 sender/inbox、Hotkey、捕获/业务 worker、缩略图、ImageWorker、PrivacyRuntimeOwner
（controller→RPC helper→SettingsWorker）和 StorageExecutor。`ClipboardCaptureInbox::close`
已公开为生命周期协调器的唤醒点。`runtime_cleanup_tests::阶段线程失败仍只收敛一次` 用
panic fake worker 证明阶段失败会被记录、句柄只消费一次且可幂等收敛。

## 允许修改

- `docs/planning/原子计划-ATOM-45-暂停剪贴板记录.md`
- `src/settings/model.rs`
- `src/settings/persistence.rs`
- `src/settings/mod.rs`
- `src/privacy/mod.rs`
- `src/privacy/pause.rs`
- `src/privacy/controller.rs`
- `src/lib.rs`
- `src/clipboard/io_worker.rs`
- `src/clipboard/reader.rs`
- `src/clipboard/mod.rs`
- `src/platform/windows/tray.rs`
- `src/platform/windows/system_window.rs`
- `src/platform/windows/hotkey.rs`
- `src/platform/windows/mod.rs`
- `src/main.rs`
- `src/command.rs`（仅当托盘 action 仍需跨现有事件边界，不把配置 IO 放进 UI）
- `tests/privacy_pause.rs`
- ATOM-45 直接相关的现有模块内测试

## 明确禁止修改

- `AGENTS.md`
- `原子开发任务计划.md`
- `docs/planning/开发计划.md`
- `docs/planning/并行开发执行计划.md`
- `docs/ai-project/项目工作台.md`
- `docs/ai-project/项目阶段记录.md`
- Slint 页面、卡片、搜索、分页、收藏、删除与清空 reducer
- SQLite schema、历史清理、图片资产清理与图片格式解析
- ATOM-46 排除程序规则
- 默认 `%LOCALAPPDATA%\ClipboardBoard` 中的真实文件
- 真实剪贴板、真实托盘、真实应用、注册表、单实例状态
- 远端分支、push、upstream 或 PR

若实现必须越出允许文件，先停止并修订原子边界，不自行扩大范围。

## 实现步骤

1. 先为暂停 DTO、默认兼容、时钟边界与未知字段保留建立 RED 测试。
2. 扩展 `AppSettings` 和递归已知字段合并，复用 ATOM-44 的 load/save 验证与恢复协议。
3. 以可注入墙上/单调时钟实现 `RecordingGate`、读取许可、重启恢复和到期判断。
4. 建立 controller 与串行 settings RPC helper，按方向实现 revision 冲突、
   OutcomeUnknown/ReconciliationRequired、最新权威回滚基线、到期和关闭。
5. 在 `ClipboardReadError` 增加不含正文的 `Paused`；为 io_worker 加窄 reader/factory
   注入接缝，生产与测试共用同一条 `gate → factory/backend → read → result → publish`
   路径。
6. 把托盘返回值拆为 `TrayAction`，接入四类暂停/恢复命令，不让消息线程等待配置。
7. 在 `main` 接入 SettingsWorker、PauseController、HotkeyManager，并闭合全部启动失败和
   正常关闭分支。
8. 运行 ATOM-45 窄测试、格式、Clippy、中文注释和 diff 检查，进入提交前 DDD。

## 测试与验收

所有测试使用显式唯一临时配置根、假时钟、假正文读取闭包或假 backend；不得调用
`SettingsWorker::start()`、不得触碰默认 LOCALAPPDATA、真实 HWND、Shell 或剪贴板。

1. **默认兼容**：ATOM-44 旧配置缺少 privacy 时加载为 Active；保存后顶层、history、
   privacy 三层未知字段逐值保持。
2. **持久化模型**：Active、UntilUnixMillis、Indefinite round-trip；非法 tagged 值和
   非法 deadline 被拒绝。
3. **5/30 分钟**：假墙上时钟生成精确 deadline，假单调时钟在到期前保持暂停、边界时刻
   恢复。
4. **墙上时钟跳变**：当前进程已确认的 5/30 分钟只受单调时钟控制；重启时墙钟回拨会
   依据仍在未来的 UTC deadline 继续暂停，测试明确记录这一固有限制。
5. **重启**：未来 timed deadline 恢复剩余时长；已到期 deadline 启动为 Active；
   Indefinite 重启后仍暂停。
6. **生产同路径 reader 注入**：paused 下提交文本和图片请求，factory 构造与 read
   次数均为 0并返回 `ClipboardReadError::Paused`；active 对照用同一生产函数分别形成
   文本、图片结果并发布，证明测试没有绕过实际 gate 顺序。
7. **在途线性化**：A 已取得许可并阻塞，B 已排队；置 update_pending 后释放 A，断言 A
   先完成发布，B 随后 Paused，B backend/factory 构造和 read 均为 0。
8. **恢复不补录**：Resume 只打开门禁，不主动调用读取闭包；下一次显式捕获消息才读取。
9. **重复命令**：暂停时重新选择 5/30/无限以最新命令为准；Active 上 Resume 幂等。
10. **revision 冲突**：另一客户端先修改 history；暂停保存冲突后刷新完整 snapshot，
    保留新 history 并只覆盖 privacy。
11. **回执未知**：模拟成功发布但丢失回执，controller 通过 snapshot 识别已提交状态；
    snapshot 也不可用时进入 Reconciling/fail-closed，不用旧 revision 重放。
12. **方向事务与回滚**：Active→Pause 先关准入，确定失败按最新权威基线恢复；
    Pause→Active 在权威 Active 前保持关门；冲突刷新会更新回滚基线。
13. **跨 deadline 慢 RPC**：Pause save 阻塞跨过单调 deadline 时 gate 仍按时 Active；
    晚到 Pause 成功只触发 Active 归一化，绝不重新关门。
14. **托盘映射**：所有固定 menu ID 精确映射到 action；取消、未知命令不提交暂停；菜单
    销毁协议保持。
15. **全局 latest-wins**：异类快速点击也只保留最新动作；`try_submit` 只表示接收；
    关闭后拒绝；菜单读取 Active/Paused/Updating/Reconciling 状态不触发 settings IO。
16. **timer/RPC 分离**：阻塞 save 时假时钟、命令和关闭仍能唤醒 controller。
17. **RPC port**：脚本 fake 可阻塞并返回 Conflict、OutcomeUnknown、snapshot 失败；
    controller 完成各方向事务且不持 SettingsClient。生产 adapter 窄测试证明
    snapshot/save 的参数、成功值和错误逐项原样转发。
18. **统一 owner**：fake handles 对每个启动阶段故障执行严格逆序、一次性收敛；
    fake pump 等待 inbox close 才能 join，证明 Hotkey/ClipboardIO/inbox 必须先停；
    controller/helper 各 join 一次，SettingsWorker 最后关闭且无死锁。
19. **日志与错误**：Paused、Reconciling、Debug/Display、诊断和托盘错误不包含剪贴板
    正文或配置 JSON。

建议窄验证：

```powershell
$env:CARGO_TARGET_DIR = `
  'F:\workspace\small-projects\windows-copy-worktrees-targets\privacy-runtime-atom45'
cargo test --lib privacy
cargo test --lib clipboard::io_worker
cargo test --lib platform::windows::tray
cargo test --lib platform::windows::system_window
cargo test --test privacy_pause
cargo clippy --lib --bin clipboard-board --test privacy_pause --all-features -- -D warnings
rustfmt --edition 2021 --config skip_children=true --check <ATOM-45 Rust 文件>
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/check_comments.ps1 `
  -ProjectRoot (Get-Location).Path
git diff --check
```

仓库既有全量测试或格式基线若失败，只记录与当前窄检查的差异，不修改范围外文件。

## 回滚与故障边界

- 本原子的产品回滚是移除 privacy controller、托盘 pause action 和 ClipboardIO gate，
  恢复 ATOM-44 默认配置模型；未知 `privacy` JSON 仍必须由旧版本保留。
- 配置保存成功是暂停持久化线性化点；运行时门禁排他更新保证该点前在途读取已经完成。
- 若保存确定失败，恢复最新权威门禁基线；若结果未知，必须 snapshot 对账后才能决定
  提交或回滚，对账不可用时 fail-closed/Reconciling。
- 进程崩溃后按已原子发布的配置恢复；未发布的运行时状态不作持久化承诺。
- 本原子沿用 ATOM-44 的“进程崩溃下旧版或新版原子可见”声明，不新增突然掉电目录项
  耐久保证。
- timed 暂停到期后，即使 Active 归一化写入失败，当前进程仍按时 Active；正常墙钟下
  过期 deadline 的重启解释为 Active，但墙钟回拨可能再次把该 deadline 视为未来。
- 托盘命令槽满时由四类动作共同 latest-wins 覆盖旧的尚未执行命令，但已进入配置提交
  临界区的命令必须完成对账后再处理最新命令。

## 完成判定

- 5 分钟、30 分钟、无限暂停和恢复均通过确定性测试。
- 暂停期间文本与图片正文读取次数为 0，且没有历史保存。
- timed 到期和重启语义、Indefinite 跨重启、revision 冲突/回执未知/回滚均有证据。
- 托盘消息线程不执行设置 IO，所有 worker 有显式关闭顺序。
- 编码前和提交前 DDD 均为 `PASS`。
- 创建一个仅包含允许文件的本地原子提交，不 push、不设置 upstream。

## 交付给 ATOM-46 的输出

- `RecordingGate` 的正文读取许可点。
- controller 可合并 privacy 配置而不覆盖其他设置的 revision 协议。
- message-only window 到隐私命令 worker 的非阻塞接缝。
- 可注入来源判断的同一门禁位置；ATOM-46 不另建读取后过滤器。

## DDD 门禁

- 编码前 DDD 第一轮：`ddd_atom45_precode`，结果 `REVISE_PLAN`；七组发现已转化为修订 2
  的方向事务、gate、timer/RPC、reader 注入、UTC 限制、托盘状态和 owner 契约。
- 编码前 DDD 第二轮：同一 `ddd_atom45_precode`，结果 `REVISE_PLAN`；三组发现已转化为
  修订 3 的 SettingsRpcPort、唯一关闭链和 L3 风险契约。
- 编码前 DDD 最终结果：`PASS`。
- 提交前 DDD：`PASS`；最终复核覆盖 controller deadline/in_flight、RPC 失败收敛、
  `Reconciling` fail-closed、`RuntimeCleanup`、tray flags 和 Clipboard capture Debug
  脱敏。
- 最终窄验证证据（未运行全量测试）：
  - `cargo test --lib privacy --no-fail-fast`：10 passed。
  - `cargo test --test privacy_pause -- --test-threads=1`：3 passed。
  - `cargo test --lib platform::windows::tray --no-fail-fast`：5 passed。
  - `cargo test --lib clipboard::io_worker --no-fail-fast`：17 passed。
  - `cargo test --lib clipboard::reader --no-fail-fast`：24 passed。
  - `cargo test --lib diagnostics --no-fail-fast`：4 passed；`cargo test --lib settings --no-fail-fast`：13 passed。
  - `cargo test --test settings_storage -- --test-threads=1`：15 passed。
  - `cargo test --bin clipboard-board runtime_cleanup_tests --no-fail-fast`：1 passed。
  - `cargo check --lib --bin clipboard-board`、目标 Rust 文件 `rustfmt --check`、中文注释检查、
    `git diff --check` 和目标 Clippy 均通过。
- 任一评审要求修改计划或代码时，在本 worktree 修复并由同一评审任务复审；未 PASS
  不提交。

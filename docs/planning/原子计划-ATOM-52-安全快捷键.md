# ATOM-52 安全修改全局快捷键计划

## 计划元数据

- 计划 ID：`ATOM-52`
- 类型：`atomic-development`
- 修订版本：`7`
- 状态：`active`
- 依赖：ATOM-44 已集成；ATOM-50P 已集成（完整 ATOM-50 设置页面不再实现）。
- 基线：`6b182ab`
- 分支：`codex/atom52-hotkey`
- Worktree：`F:\workspace\small-projects\windows-copy-worktrees\hotkey-settings`
- 风险等级：`L3`
- 远端约束：Worker 只在本地提交，不设置 upstream、不 push。

## 唯一目标

用户修改全局快捷键时，注册/保存任一步失败都不丢失当前可用热键；只有新热键注册成功且配置耐久保存成功后，才切换运行时热键。

## 当前代码事实

- `HotkeySpec` 和 `DEFAULT_HOTKEY` 目前是固定 Alt+V；`system_window::run` 直接注册一个规格并在窗口过程中过滤固定 ID。
- `SettingsWorker` 已提供带 revision 的 JSON/备份 CAS 保存，但尚无快捷键字段。
- `SettingsClient::snapshot/save` 是同步阻塞 API，不能在 Slint 回调或 message thread 直接调用；主线程需保留一个 clone，交给独立事务 worker。
- Win32 热键注册由拥有 message-only HWND 的消息线程完成，注册/注销必须保持在线程归属。
- ATOM-25R 已撤销自动粘贴方向；本原子只改变唤起面板热键，不改变窗口焦点和粘贴行为。

## 允许修改

- `src/settings/model.rs`、`src/settings/persistence.rs`、`src/settings/worker.rs`：增加可序列化快捷键 DTO、合法组合校验和 CAS 保存接线。
- `src/platform/windows/hotkey.rs`、`src/platform/windows/system_window.rs`：注册事务、运行时热键规格和旧 ID 清理。
- 必要的设置 UI/托盘入口及 `src/main.rs` 最小接线；不做完整设置页面。
- 热键模型/注册抽象测试、当前原子计划文档和中文注释。

## 明确禁止修改

- 不改剪贴板监听、图片、历史分页、窗口置顶、自动粘贴和数据清理。
- 不直接在 UI 线程调用 RegisterHotKey/UnregisterHotKey；不跨线程传递 HWND。
- 不先注销旧热键再注册新热键；不因保存失败覆盖旧配置。
- 不启动真实程序或修改用户真实注册表；Windows 注册表行为使用抽象/注入测试，真实验证由主 Agent 后续单独执行。

## 配置契约

- 配置保存 `hotkey.modifiers` 和 `hotkey.virtual_key`，使用有限的 `u32` 数值；默认值表示 Alt+V。旧 JSON 缺字段时 `#[serde(default)]` 回退 Alt+V，未知顶层/嵌套字段继续保留。
- 只接受非零 `virtual_key <= 0xFF`、至少一个 Ctrl/Alt/Shift/Win 物理修饰键、可选 `MOD_NOREPEAT`，拒绝未知位、保留 VK、系统保留组合和空组合；动态占用由 RegisterHotKey 冲突返回处理。
- 用户展示使用拥有型 `HotkeyLabel(String)`，不再把可配置快捷键表示为 `&'static str`；规范化顺序固定为 Win、Ctrl、Alt、Shift、主键。
- 配置快照 revision 与运行时注册事务绑定；旧 JSON 未包含字段时回退默认值并保持未知字段。

## 注册事务不变量

1. UI/设置层通过非阻塞 mpsc command 将候选规格送入 `HotkeyTransactionOwner`；它持有 `SettingsClient` clone 和当前快照，不让 UI/message thread 等待磁盘。
2. owner 向 message thread 投递 `RegisterCandidate`；线程只在自身 HWND 上以新正 `i32` ID 尝试 `RegisterHotKey`。
3. 注册失败（含冲突）时立即注销候选 ID，旧 active 热键和旧配置继续工作；Saving 阶段只接受旧 active ID，candidate/stale ID 的 WM_HOTKEY 一律忽略。
4. 新注册成功后 owner 用最新完整 `AppSettings`+expected revision 调用保存；`RevisionConflict`/IO 失败注销候选并保留旧热键。`OutcomeUnknown` 必须用 transaction_id、候选完整 DTO 和 expected/actual revision 做 snapshot 对账：确认精确持久化才可继续，否则进入 `ReconcileRequired`，不擅自切换。
5. 配置保存成功后通过 message thread 原子发布 active spec/revision，收到同 transaction_id 的 `Published` ack 后再把旧 ID 标记 stale 并注销；旧 ID 或 candidate 注销失败均登记当前 HWND 生命周期内的 stale ID，过滤且禁止复用，不持久化 HWND/ID。
6. 所有 ID 分配使用 checked 非零正 `i32` 递增，候选不得等于 active/stale，溢出 fail-closed；退出时在 message thread 注销 current+candidate+stale，重复注销幂等。

### 跨线程状态机

`Idle -> CandidateRegistered -> Saving -> Committed`；任意注册/保存/关闭失败进入 `RollbackCandidate`，
旧 active 只有在 `Committed` 后才可失效。message thread 只处理 `RegisterCandidate`、`PublishActive`、
`DropCandidate`、`Shutdown` 命令并通过 `PostThreadMessageW`/消息队列唤醒，不持有 SettingsClient；owner
先停止接收 UI 命令，再等待保存事务完成，最后发送 WM_QUIT 并 join，避免 message thread 阻塞导致无法退出。
每个命令和 ack 都携带单调 transaction_id；owner 同时只允许一个在途事务，后续提交返回 Busy（不覆盖前一事务）。
message thread 命令集合固定为 `RegisterCandidate`、`PublishActive`、`QueryTransaction`、`CancelTransaction`、
`DropCandidate`、`Shutdown`；线程维护 transaction tombstone/generation，已取消或 generation 过期的迟到命令只回
`Cancelled`，不得改变 active。`QueryTransaction` 回执除 transaction 状态外必须携带 `active_state`（Old/Candidate/None/Unknown）、
`active_id`、`candidate_id` 和 generation；`active_state=Unknown` 时 WM_HOTKEY 全部忽略。`PublishActive` 必须按 transaction_id 幂等；ack/窗口关闭时先查询 `Published`、
`CandidateRegistered`、`NotFound` 或 `Cancelled`，禁止仅凭发送错误做 CAS 补偿。只有确认 Published 才采用新 active；确认 CandidateRegistered
才发送 DropCandidate；Query 确认 CandidateRegistered 且 DropCandidate 成功后，使用 candidate revision CAS 写回旧完整 DTO，
成功才恢复旧 active；CAS 冲突/未知或 Drop 失败（登记 stale）统一进入 `ReconcileRequired`/fail-stop，排空并拒绝
该 transaction 的迟到命令，不做未经确认的回滚。ReconcileRequired 只允许 Query/Cancel/Shutdown，托盘仍可打开面板但
不接受新的快捷键修改。Shutdown 先拒绝新命令、排空在途事务，再注销 current/candidate/stale，最后 WM_QUIT。
保存已成功但发布确认未知时，运行时状态标记 `active=Unknown`，不得声称旧 active 仍可用；Query 返回 NotFound/Cancelled
也不能证明保存未发生，必须进入 ReconcileRequired（不做未经确认的 CAS 回滚）；只有 Query 明确返回旧 active 且候选未发布
才恢复旧状态，否则保持无热键 fail-stop，提示重启对账；下一次启动以磁盘已验证配置为准。

## 实现步骤

1. 增加快捷键 DTO、规范化展示和统一校验；补默认/未知位/空修饰/非法键测试。
2. 将 message-only 窗口热键处理从固定规格改为可替换运行时状态，保留旧 `WM_HOTKEY` 过滤安全边界。
3. 实现注册事务 reducer/消息协议，覆盖冲突、保存故障、旧注销失败和重启恢复。
4. 增加最小设置入口（现有设置壳/托盘菜单触发一个轻量快捷键编辑对话框），只投递异步候选命令并显示冲突、保存失败或未知结果；不引入完整设置页。

### 启动策略

- main 首次从已验证 SettingsSnapshot 读取持久化 HotkeySettings 后注册，不再无条件使用 DEFAULT_HOTKEY。
- 缺字段/默认值注册 Alt+V；配置语义非法由设置加载的主/备份校验处理并回退可验证副本。
- 合法配置但首次 RegisterHotKey 冲突或 Win32 失败时不销毁 message-only HWND/托盘线程，也不静默覆盖配置：
  进入 `active=None/HotkeyUnavailable` 状态，保留托盘和面板显式打开能力，用户可重置为默认或重新提交；
  启动不要求管理员权限。fake startup registrar 必须覆盖该可恢复状态。

## 定向验证

- `cargo test --lib settings::model <快捷键校验> -- --test-threads=1`
- `cargo test --lib platform::windows::hotkey <注册事务/ID> -- --test-threads=1`
- `cargo test --lib platform::windows::system_window <消息过滤> -- --test-threads=1`
- `cargo test --lib <注册抽象/保存故障> -- --test-threads=1`
- fake registrar/ID allocator 状态机窄测试：冲突、candidate cleanup、保存失败、RevisionConflict、OutcomeUnknown
  对账、PublishActive ack-loss→Query(Published/CandidateRegistered/NotFound/Cancelled)+active_state/generation、
  CandidateRegistered→Drop→CAS 旧 DTO、Drop 失败/保存后 NotFound/Cancelled 进入 ReconcileRequired、Published→NotFound/Cancel、PostThread failure 后迟到命令被
  Cancel/tombstone 拒绝、旧注销失败 stale、candidate/stale WM_HOTKEY 过滤、Saving 期间旧 active 可用、ID 溢出、
  active=None/HotkeyUnavailable 可恢复、成功切换和重启恢复。
- 校验边界固定为物理修饰 `MOD_ALT|MOD_CONTROL|MOD_SHIFT|MOD_WIN` 加可选 `MOD_NOREPEAT`；virtual key 为
  `1..=0xFE`，排除 `VK_SHIFT/VK_CONTROL/VK_MENU/VK_LWIN/VK_RWIN/VK_APPS/VK_PROCESSKEY/VK_PACKET`；
  静态拒绝明确系统保留的 `Alt+Tab`、`Alt+F4`、`Win+L`、`Ctrl+Alt+Delete`，其他动态占用全部交给
  RegisterHotKey fake/真实返回判定；应用 ID 固定为正 `i32` 的 `1..=0xBFFF`，并覆盖 0、未知修饰、保留 VK、
  上限和溢出测试。
- `cargo check --lib`、目标 Clippy、目标 rustfmt、`git diff --check`。

禁止运行全量测试；不注册真实全局热键、不触碰真实 HKCU。

## DDD 门禁

- 编码前 DDD：审查 RegisterHotKey 事务顺序、跨线程 HWND 约束、CAS 保存、旧热键回退和 stale ID 清理；必须 PASS。
- 提交前 DDD：复核完整 diff、故障注入测试、权限/冲突边界和旧热键不变量；必须 PASS。

## 完成判定

- 冲突、保存失败、旧注销失败、成功切换和重启恢复均有定向证据；旧热键不会在失败路径丢失。
- 只修改允许范围，Worker 创建唯一 ATOM-52 本地提交，不 push。

## 执行记录

- 状态：`completed`
- 完成内容：加入可持久化 `HotkeySettings`、合法组合校验和拥有型标签；运行时热键改为消息线程内候选注册、CAS 保存、发布确认、旧 ID 注销的事务流程；补充 `OutcomeUnknown` 对账、generation tombstone、stale ID、Unknown fail-closed 和启动冲突可恢复状态；托盘提供四个合法快捷键预置入口，异步显示 Busy/冲突/对账状态。
- 关键测试：`settings::model` 8 项、快捷键持久化合并 1 项、`platform::windows::hotkey` 6 项、`platform::windows::system_window` 11 项、`platform::windows::tray` 6 项全部通过；包含 FakeRegistrar 冲突/候选清理/发布信号/迟到取消和保存失败回滚决策测试。
- 工程检查：`cargo check --lib`、`cargo check --bin clipboard-board`、`cargo clippy --lib --no-deps -- -D warnings`、目标文件 `rustfmt`、`git diff --check` 均通过；未运行全量测试，未注册真实全局热键。
- DDD：提交前完整 diff 复核结论为 `PASS`。
- 同步：已变基到 `a459cdb`；仅创建本地提交，不 push 远端。

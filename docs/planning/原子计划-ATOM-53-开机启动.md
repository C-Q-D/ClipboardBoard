# ATOM-53 当前用户开机启动计划

## 计划元数据

- 计划 ID：`ATOM-53`
- 类型：`atomic-development`
- 修订版本：`5`
- 状态：`completed`
- 依赖：ATOM-44、ATOM-50P 已集成；完整 ATOM-50 设置页面不实现。
- 基线：`be3ce23`
- 分支：`codex/atom53-startup`
- Worktree：`F:\workspace\small-projects\windows-copy-worktrees\startup-settings`
- 风险等级：`L2`
- 远端约束：Worker 只在本地提交，不设置 upstream、不 push。

## 完成证据

- 已按 v5 接入稳定启动状态 UI 文案、托盘启用/禁用/重试入口，以及 owner 的可重试关闭流程。
- `cargo test --lib platform::windows::startup -- --test-threads=1`：18 项通过。
- settings model/persistence、tray、UI 状态、`cargo check --lib --bin clipboard-board`、目标 Clippy、目标 rustfmt 和 `git diff --check` 均通过。
- 变基到 `main@be3ce23` 后仅创建本地提交，未 push；真实 HKCU 注册表仍由后续主流程验证。

## 唯一目标

用户可以控制当前账户登录时是否启动 ClipboardBoard；使用 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，不需要管理员权限，开关和重复操作幂等。

## 当前代码事实

- `SettingsWorker` 已提供 CAS JSON/备份保存和快照 revision。
- main 启动与托盘生命周期集中在 `src/main.rs`/Windows platform 模块。
- 当前没有启动项模块，也没有真实注册表抽象；测试不能写用户真实 HKCU。

## 允许修改

- `src/settings/model.rs`、`src/settings/persistence.rs`、`src/settings/worker.rs`：增加 `startup.run_on_login` 字段、默认值、校验和持久化。
- 新增 `src/platform/windows/startup.rs`（或等价模块）：注册表路径和值名、引号构造、幂等 enable/disable、稳定错误分类。
- `src/main.rs` 与最小设置/托盘入口接线；保留已有 settings client 的异步/非阻塞约束。
- `src/command.rs`、`src/app/ui_event.rs` 与 `ui/app-window.slint`：仅用于承载不泄露路径/错误正文的稳定启动状态反馈。
- fake registry 抽象测试、计划文档和中文注释；必要的 `Cargo.toml` Windows Registry feature。

## 明确禁止修改

- 不写 HKCU 以外的启动位置、不创建计划任务、不要求管理员权限、不改变安装器。
- 不把未验证的用户输入直接拼入命令行；不删除其他程序的 Run 值；不静默覆盖用户自定义同名值。
- 不修改剪贴板、图片、历史、快捷键事务和自动粘贴；真实注册表验证由主 Agent 后续单独执行。

## 配置与注册表契约

- 配置字段 `startup.run_on_login: bool` 默认 `false`；旧 JSON 缺字段回退 false，未知字段保留。
- 固定 Run value name `ClipboardBoard`。生产值必须是 `REG_SZ`，内容为当前 exe 绝对路径的
  Windows 单参数命令行表示；路径或参数含空格、Unicode、UNC 或 `\\?\` 前缀时仍按同一算法处理，
  拒绝 NUL、控制字符和非法引号。
- 所有字符串在 Win32 边界以 `OsString`/UTF-16 传递；写入 `REG_SZ` 时明确包含终止 NUL 的字节长度，
  读回时只去除一个终止 NUL，不做 lossy UTF-8 转换。引号算法固定为：外层包双引号，连续反斜杠在
  引号前加倍，参数末尾反斜杠在收尾引号前加倍，路径中的引号直接拒绝。
- 固定值名不等于自动拥有。只有现值与当前 exe 的 canonical 命令行完全相等时才判定为本程序所有；
  现值缺失可由 enable 创建，现值为其他命令行、错误类型或无法解码时返回 `Conflict`/`InvalidValue`，
  禁止覆盖或删除。disable 只删除已确认属于本程序的值，缺失视为成功，其他 Run 值始终保留。
- effective 状态固定为 `Disabled`、`Enabled`、`Missing`、`Mismatch`、`Conflict`、`PermissionDenied`、
  `InvalidValue` 和 `Unknown`，并按配置期望与 Run 观测值映射：`run_on_login=false + 缺失` 为
  `Disabled`；`false + 自有值` 为 `Mismatch`；`true + 自有值` 为 `Enabled`；`true + 缺失` 为
  `Missing`；任一 foreign value/错误类型为 `Conflict`/`InvalidValue`；权限或读回未知为对应错误状态。
  配置与注册表不一致只报告状态，不擅自删除或覆盖。路径迁移不自动搬迁旧值，用户需先解决同名冲突。

## 双资源事务与失败语义

配置 JSON 和 HKCU Run 值是两个独立耐久资源，不能声称存在跨资源原子事务。每次修改保存旧的完整
`AppSettings`、expected revision 和旧的自有 Run 值快照，按下面顺序执行：

1. owner 先通过独立 `SettingsClient` 读取最新完整快照和 revision，仅把 startup 字段作为本事务候选；
   读取并校验当前注册表值，确认 ownership 后通过 expected-state CAS 写入或删除 Run 值并立即读回校验。
2. 注册表操作成功后再次读取最新完整设置快照，以该最新 revision 合并 startup 字段并执行 CAS 保存；不能用
   旧快照覆盖并发的 history/privacy/hotkey 字段。若第二次读取发现另一笔 startup 事务已改变期望，直接
   `SettingsConflict` 并按 ownership guard 补偿，不强行覆盖。
3. CAS 冲突或保存失败时，先重新读取 Run 值；只有值仍与本事务已写入的 canonical 命令完全相等时，
   才允许用 ownership guard 补偿旧快照。若外部进程已改值、ownership 不再成立或补偿结果未知，禁止覆盖，
   直接进入 `ReconcileRequired`；补偿成功才返回 `SaveFailed` 并保留旧有效状态。
4. 写入、读回或补偿的结果未知时进入 `OutcomeUnknown`，再次读取注册表和设置快照；无法证明两者一致则进入
   `ReconcileRequired`，暂停新的启动项修改，只允许 Query/Retry/Shutdown，绝不伪造成功。
5. enable/disable 重复执行必须幂等；关闭 owner 时先拒绝新命令、排空在途事务并完成对账，再停止
   `SettingsWorker`，避免 owner 使用已关闭的 settings client。
- 所有 registry 操作通过 trait/fake backend 注入；生产 backend 只在 Windows 编译，非 Windows 可编译 stub。
- `RegistryBackend` 必须提供带 expected-state 的 `set_if_matches`/`delete_if_matches`（或等价的进程间
  mutation mutex + 写后读回冲突证据）。普通 read→write→read 不得被当作 CAS；每次 enable/disable、补偿
  和删除都必须证明写入前值仍是预期值。fake backend 要能在任意读写间插入外部改值，断言不会覆盖外部值。

## 稳定错误分类

底层 Win32/IO 错误必须映射到固定类别，并同时给出动作路由：

| 类别 | 典型情况 | 路由 |
|---|---|---|
| `InvalidInput` | NUL、控制字符、非法引号、错误 UTF-16 | 终止本次请求 |
| `PermissionDenied` | HKCU 访问被拒绝 | `PendingRetry` |
| `Unavailable` | 注册表/设置 IO 不可用 | `PendingRetry` |
| `ForeignConflict` | 同名不同命令、错误类型或 ownership 失效 | 终止并提示冲突 |
| `SettingsConflict` | Settings CAS revision 冲突 | 重新读取后重试 |
| `OutcomeUnknown` | 写入/读回/补偿结果未知 | `ReconcileRequired` |
| `CompensationFailed` | 旧值补偿失败或外部改值 | `ReconcileRequired` |
| `OwnerClosed` | owner 已停止或队列关闭 | 终止本次请求 |

## 异步 owner 与最小入口

- `StartupSettingsOwner` 独占 registry backend 与 `SettingsClient` clone，命令通道容量固定为 1；UI/托盘
  只投递 `Enable`、`Disable`、`Query`、`Retry`、`Shutdown`，不直接调用 registry 或同步 settings API。
- 每个命令携带正整数 `transaction_id`、generation 和 expected revision；同一时间只允许一个在途事务，
  新提交返回 `Busy`，迟到命令根据 tombstone/generation 丢弃。
- 结果事件固定为 `Applied`、`AlreadyApplied`、`Conflict`、`SaveFailed`、`PendingRetry`、
  `ReconcileRequired`、`Busy` 和 `Stopped`。UI 显示稳定状态文本，不显示注册表原始错误或路径隐私。
- 启动初始化只读取已验证的配置与 Run 值，得到 effective 状态后再启动主程序；不在启动阶段自动修复
  mismatch。运行时设置入口至少从托盘菜单可达，提供启用/禁用/重试和状态反馈，不实现完整设置页。

## 实现步骤

1. 增加启动设置 DTO、serde 默认/未知字段保留、校验和 CAS 保存测试。
2. 新增 RegistryBackend trait 与 Windows HKCU Run 实现；封装值名、命令行转义和幂等读写。
3. 增加 `StartupSettingsOwner` 有界命令和结果协议；按双资源事务顺序执行 CAS 保存与注册表补偿，
   覆盖 Busy、generation/tombstone、OutcomeUnknown 和 ReconcileRequired。
4. 接入启动初始化与托盘最小入口：只读取并报告 effective 状态，不自动修复 mismatch；补 enable/disable/
   repeat/restart、同名冲突、并发保存和 owner 关闭顺序 fake 测试。

## 定向验证

- `cargo test --lib settings::model <startup 字段> -- --test-threads=1`
- `cargo test --lib settings::persistence <startup 持久化> -- --test-threads=1`
- `cargo test --lib platform::windows::startup <fake registry> -- --test-threads=1`
- fake backend 必须覆盖：缺 key/value、缺失值 disable 幂等、其他 Run 值保留、同名不同命令行冲突、错误
  `REG_SZ` 类型、权限拒绝、读/写/删失败、写后读回未知、补偿失败、重启 mismatch 和 non-Windows stub。
- quoting 必须覆盖：空格、连续/尾部反斜杠、Unicode、UNC、extended path、控制字符、NUL、引号和
  UTF-16 读回；断言写入字节长度和 round-trip 字符串完全一致。
- owner 必须覆盖：CAS 冲突不覆盖并发设置、registry 先成功后 CAS 失败的补偿、补偿未知进入
  `ReconcileRequired`、Busy、迟到 generation、Shutdown 先停 owner 后停 SettingsWorker。
- `cargo check --lib`、目标 Clippy、目标 rustfmt、`git diff --check`。

禁止全量测试，不写真实 HKCU、不启动登录项、不操作安装器。

## DDD 门禁

- 编码前 DDD：审查 HKCU 范围、UTF-16 命令行引号、固定值 ownership、双资源事务/补偿、owner 协议、
  mismatch 状态、幂等和 fake backend；必须 PASS。
- 提交前 DDD：复核完整 diff、设置重启与注册表证据、权限/失败回滚；必须 PASS。

## 完成判定

- enable/disable/重复/重启/路径空格/注册表失败均有可重复窄证据；其他 Run 值保留。
- 只修改允许范围，Worker 创建唯一 ATOM-53 本地提交，不 push。

## 修订记录

- v2：按编码前 DDD 补齐双资源事务补偿和 `OutcomeUnknown/ReconcileRequired`，固定 `ClipboardBoard`
  值的 ownership/conflict 判定、UTF-16 `REG_SZ` 和 Windows quoting 算法、StartupSettingsOwner 有界
  命令/结果/generation 协议、启动 mismatch effective 状态及完整 fake backend 边界矩阵；未改变本原子
  的 HKCU-only 范围和“不实现完整设置页”约束。
- v3：根据 DDD 增加补偿前 ownership guard、`set_if_matches/delete_if_matches` expected-state CAS 和
  外部并发改值测试，并固定错误类别到终止/重试/对账路由，关闭 registry read→write 的 TOCTOU 空档。
- v4：补充配置期望与 Run 观测值的 effective 状态映射，并规定 registry 操作前后都读取最新完整设置快照，
  以最新 revision 合并 startup 字段后 CAS，避免旧快照覆盖 history/privacy/hotkey 并发修改。
- v5：将稳定启动状态反馈的最小 UI 接线纳入允许范围；启动查询和托盘命令只传递枚举映射文案，
  不携带路径、命令行或底层错误正文。

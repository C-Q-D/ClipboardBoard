# ClipboardBoard ATOM-40 统一分页协调器原子计划

## 计划元数据

- 计划 ID：ATOM-40
- 类型：atomic-development
- 修订版本：1
- 状态：提交前 DDD 已通过，待本地原子提交
- 所属交付单元：UNIT-12
- 风险等级：L2
- 计划提交：`feat(history): [ATOM-40] 建立统一分页协调器`

## 基线与依赖证明

- `base_main_sha`：`1d41bf8d5526698be4d1fd9a32c5aa1294b5eeeb`
- 依赖原子：ATOM-39
- ATOM-39 主线提交：`29e1b5d`
- 祖先证明命令：
  `git merge-base --is-ancestor 29e1b5d 1d41bf8d5526698be4d1fd9a32c5aa1294b5eeeb`
- 祖先证明结果：退出码 `0`，ATOM-39 是当前基线祖先。
- 本地分支：`codex/atom40-43-history-perf`
- worktree：
  `F:\workspace\small-projects\windows-copy-worktrees\history-perf`
- 远端约束：本分支不设置 upstream、不 push、不创建远端分支。

## 唯一目标

在 ATOM-25R 已有分页协议上，把文本和图片混合列表共同需要的游标、容量、失败重试
和页级性能观测统一收口到现有 `HistoryPageCoordinator`。继续复用首批 30、后续 50、
`generation/token/requested_cursor` 精确身份和复合游标，不创建第二套分页协调器。

## 现状证据

- `src/history_query.rs` 已有唯一 `HistoryPageCoordinator`，负责数据集 generation、
  请求 token、活动请求身份、首页 30 条、续页 50 条和 2,000 条上限。
- `HistoryPageResult` 和 `UiHistoryPage` 已可承载文本与图片 DTO；图片摘要在
  `ui_item_from_summary` 中转换为 `UiClipboardItemKind::Image`。
- `src/app/ui_event.rs` 仍单独保存 `next_history_cursor`、
  `history_retry_required` 和当前已加载数量，并在 reducer 内自行完成续页去重与容量
  截断。这使分页状态有两个所有者，后续图片混合列表性能门禁难以取得单一快照。
- 查询 worker 当前不记录 SQL 执行耗时；本原子只观测从请求签发到 UI 接受结果的
  `request_to_accept_duration`，不得把它描述成“查询耗时”。ATOM-43 仍需独立测量
  首批结果绑定和显示。
- `src/storage/worker.rs` 已在同一参数化查询内支持全部、文本、图片和收藏筛选，并通过
  `(copied_at, id)` 复合游标稳定分页，本原子不修改 SQL。

## 行为契约

### 唯一状态所有者

1. `HistoryPageCoordinator` 继续是唯一分页协调器，并新增当前数据集的统一运行状态：
   下一页游标、已接受的唯一条目数、续页重试状态和只含数值的性能快照。
2. 新数据集、失效和成功首页都必须重置旧数据集的游标、容量、失败及观测状态。
3. `UiState` 不再持有上述分页字段；它只持有可见卡片、选择状态和等待提交的请求。
4. 文本页、图片页和混合页走完全相同的请求、接受、去重、容量与失败状态机；类型只
   用于有限计数，不改变分页分支。

### 请求与结果身份

1. 首页固定 `cursor=None`、`limit=30`；续页固定使用协调器保存的数据库游标，标准
   `limit=50`，接近 2,000 条时按剩余容量收缩。
2. 同一数据集最多一个活动请求。只有
   `generation + token + requested_cursor` 完全匹配且面板仍可见的结果可改变状态。
3. 切换关键词或标签、捕获刷新、隐藏和重新打开均使旧结果失效；迟到页不得覆盖或追加
   当前数据集。
4. 游标只采用数据库成功页返回的 `next_cursor`，不得从 UI 末项重算。

### 去重、容量与失败

1. 协调器提供一次页应用决策：首页替换，续页按稳定记录 ID 保留首见顺序去重。
2. 同一页重复项、与已加载集合重复的项都不增加已加载数量；最多保留 2,000 条唯一
   摘要。
3. 首页失败保留旧卡片并进入固定错误状态；续页失败保留卡片与原数据库游标，并进入
   明确重试状态。
4. 提交失败与 worker 返回失败使用相同身份收口；重试必须分配新 token，不能形成布局
   重绘请求风暴。

### 性能观测

1. 活动请求记录单调时钟起点，UI 接受当前成功结果时计算
   `request_to_accept_duration`；该区间包含请求排队、SQL worker 执行、结果邮箱等待和
   UI 事件调度，但不包含接受后的 Slint 模型绑定、布局或渲染。
2. 耗时使用 `accepted_at.checked_duration_since(requested_at)`；时钟倒退或测试传入早于
   起点的时刻时按零处理，不能 panic 或生成虚假大值。测试通过显式时间接缝推进，不使用
   真实 `sleep`。
3. 内部可变分页状态与公开只读性能快照必须分开。游标、重试和活动请求只能存在于私有
   `HistoryPaginationState`；对外 `HistoryPerformanceSnapshot` 只包含数值指标。
4. 公开性能快照至少包含：已接受页数、已加载唯一条目数、文本条目数、图片条目数、
   丢弃重复数、最近一次和累计 `request_to_accept_duration`。不另设与
   `loaded_items` 重复的 `accepted_items`。
5. `text_items + image_items == loaded_items` 是成功状态不变量；三个字段必须由同一批
   最终去重并截断后的卡片重新计算或在同一状态转移中更新。
6. 被拒绝的迟到结果、错误身份、协议超限页、失败页和重复响应不得计入成功性能指标。
7. 观测对象不包含关键词、来源、预览、路径、哈希或错误正文，不写默认诊断日志，
   避免扩大隐私与磁盘 I/O 范围。ATOM-43 可读取该快照，但必须另测首批模型绑定和显示。

### Worker 响应数量门禁

1. 活动请求保存签发时的 `limit`，不得信任返回页自行声明的数量。
2. `UiHistoryPage.items.len()` 大于签发 `limit` 时属于协议错误，整页拒绝；不得先截断
   再接受 `next_cursor`，否则被截断记录可能因游标前移而永久跳过。
3. 协议超限页按首页失败或续页失败进入既有固定失败状态；保留旧卡片和原续页游标，
   不更新已加载数量、类型计数、重复计数或成功耗时。
4. 恰好等于 `limit` 的页合法；接近 2,000 条时必须用签发时已经收缩的 limit 校验。

## 待编码前 DDD 审查的接口

以下名称是计划接口，不是已承诺实现；DDD 可在不改变行为契约的前提下收窄：

```rust
struct HistoryPaginationState {
    next_cursor: Option<HistoryCursor>,
    loaded_items: usize,
    retry_required: bool,
    metrics: HistoryPerformanceSnapshot,
}

pub struct HistoryPerformanceSnapshot {
    pub accepted_pages: u64,
    pub loaded_items: usize,
    pub text_items: usize,
    pub image_items: usize,
    pub duplicate_items: usize,
    pub last_request_to_accept_duration: Duration,
    pub total_request_to_accept_duration: Duration,
}

pub enum HistoryPageApplication {
    Replace(Vec<UiClipboardItem>),
    Append(Vec<UiClipboardItem>),
    FirstPageFailed,
    NextPageFailed,
}

pub struct UiStateSnapshot {
    // 既有公开字段保持不变。
    pub history_performance: HistoryPerformanceSnapshot,
}
```

- `request_next_page` 应改为消费协调器内部游标和已加载数量，调用方不得重复传入这两项。
- 接受结果的测试接缝应允许传入 `Instant`；生产入口使用 `Instant::now()`，并通过
  `checked_duration_since` 计算 `request_to_accept_duration`。
- 页应用方法接收当前已加载记录的稳定 ID 集合，返回已去重且已按剩余容量截断的决策；
  协调器不拥有 Slint 模型，也不调用 UI。
- 指标计数使用检查加法或饱和加法，禁止回绕；`Duration` 累加饱和。
- `UiState::snapshot()` 必须把协调器的纯性能快照复制到公开 `UiStateSnapshot`；
  `ui_state_snapshot()` 继续作为生产与集成测试的公开读取入口，不暴露内部游标、重试或
  活动 token。
- DDD 重点确认：结果身份与签发 limit 校验必须先于 ID 集合构造；失败响应不记录成功
  耗时；首页替换不把旧数据集卡片计入重复集合；公开快照仅承担数值观测，ATOM-43 另测
  模型绑定与显示。

## 允许修改

- `src/history_query.rs`：统一分页状态、页应用决策、数值性能快照及直接单元测试。
- `src/app/ui_event.rs`：删除重复游标/重试字段，改为消费协调器决策，并补充 reducer
  直接测试。
- `tests/comment_policy.rs`：仅当新增生产 Rust 文件或公共类型需要纳入既有中文注释
  机械门禁时修改。
- `docs/planning/原子计划-ATOM-40-统一分页协调器.md`：记录 DDD、实现和验证证据。

## 明确禁止修改

- 不修改 `src/storage/worker.rs` 的 SQL、复合游标或数据库模型。
- 不修改 `ui/app-window.slint`、ListView 底部回调或加载提示；这些属于 ATOM-41。
- 不修改缩略图加载、LRU 或资产路径；这些属于 ATOM-42。
- 不修改 Release 性能脚本和最终门禁阈值；这些属于 ATOM-43。
- 不修改搜索防抖、图片捕获/复制、收藏、删除、清空、配置或隐私策略。
- 不修改共享状态文档：`AGENTS.md`、`原子开发任务计划.md`、
  `docs/planning/开发计划.md`、`docs/ai-project/项目工作台.md`、
  `docs/ai-project/项目阶段记录.md`。
- 不启动真实应用，不访问默认 `%LOCALAPPDATA%\ClipboardBoard`、真实剪贴板、托盘或
  注册表。

## 实现顺序

1. 先为统一状态快照和结果决策补充失败测试，覆盖图片、文本与混合页。
2. 在现有 `HistoryPageCoordinator` 内收拢游标、唯一条目数量、重试状态和时钟起点。
3. 把精确身份校验、签发 limit 门禁、续页去重、容量截断和指标更新组合为单次原子状态
   转移。
4. 让 `UiState` 删除重复字段，只应用协调器返回的替换/追加/失败决策。
5. 将纯性能快照加入公开 `UiStateSnapshot`，并用真实 reducer 应用文本/图片混合页后
   通过 `UiState::snapshot()` 等价公开构造路径读取断言。
6. 针对搜索/标签切换、捕获刷新、迟到结果、同页重复和失败重试执行定向验证。
7. 完成提交前差异 DDD；若差异实质变化，重新计算 diff 哈希并复审。

## 测试与验证

### 协调器单元测试

- 首页 30、续页 50、2,000 边界和单活动 token 保持不变。
- 文本页、图片页和混合页更新同一快照；类型计数准确。
- 同页重复和跨页重复按 ID 去重，数据库顺序不变，游标仍取数据库结果。
- 关键词或标签切换后的旧 generation、错误 token、错误 cursor 和重复响应不改变卡片、
  游标、重试或指标。
- 首页/续页失败、提交失败和明确重试保持原协议。
- 注入时间验证最近页与累计 `request_to_accept_duration`；时钟倒退按零，迟到和失败
  结果不计入成功指标。
- worker 返回条目数大于签发 limit 时整页拒绝并保留旧游标；等于 limit 时正常接受；
  接近 2,000 条时按收缩后的签发 limit 校验。

### reducer 定向测试

- 首次打开和筛选变化仍生成首页请求。
- 混合 85 条列表仍按 30、50、5 接受且无重复。
- 捕获刷新、搜索输入和面板隐藏继续拒绝旧页。
- 续页失败保留游标与现有卡片，重试分配新 token。
- 图片与文本条目都能跨页保留稳定选择和复制身份。
- 真实 reducer 接受文本/图片混合页后，通过 `UiState::snapshot()` 构造的公开
  `UiStateSnapshot.history_performance` 读取页数、总数、分类数、重复数和耗时；公开
  快照中不存在游标、重试标记或活动 token。

### 计划验证命令

```powershell
cargo test --lib history_query::tests
cargo test --lib app::ui_event::tests
cargo test --lib app::ui_event::tests::滚动续页按三十加五十加五加载八十五条
cargo test --lib app::ui_event::tests::续页失败保持游标并要求明确重试
cargo test --lib app::ui_event::tests::续页去重不从_ui_末项重算游标
cargo test --lib app::ui_event::tests::捕获刷新拒绝旧首页结果
cargo test --lib app::ui_event::tests::搜索输入立即拒绝旧首页结果
cargo test --lib app::ui_event::tests::混合页性能快照通过公开状态读取
cargo check --lib
cargo fmt --all -- --check
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/check_comments.ps1
git diff --check
```

- 测试不得访问默认应用目录；如新增文件系统测试，必须使用包含 worktree 名、
  进程 ID 和单调序号的唯一临时根，并在测试结束清理。
- 本原子不执行全量测试、不启动桌面程序、不执行真实 Windows 全局状态测试。

## 完成判定

- 生产代码中仍只有一个 `HistoryPageCoordinator`。
- 游标、容量、失败重试和页级性能快照只有协调器一个所有者。
- 公开 `UiStateSnapshot` 只复制纯性能数值，不暴露内部游标、重试或活动请求。
- 文本、图片与混合页共用同一状态机；切换查询不混旧页，同页与跨页不重复。
- 超过签发 limit 的 worker 页整页拒绝，避免截断后接受前移游标造成记录遗漏。
- 所有定向验证、中文注释检查、格式检查和 `git diff --check` 通过。
- 编码前与提交前两个隔离 DDD 均完成，任务名、结果和最终 diff 哈希记录在本文档。
- Worker 仅创建一个本地原子提交，不 push、不设置 upstream。

## 风险与回滚

- 主要风险：把 reducer 的游标与失败状态迁移到协调器时改变既有重试边沿；指标更新早于
  身份校验导致迟到页污染；按 ID 去重后类型计数或容量计数偏大；时钟接缝引入真实等待。
- 控制方式：先锁定旧协议回归测试，再增加混合类型、精确身份、去重和确定性时钟测试；
  协调器返回纯决策，保持 Slint 与 SQLite 边界不变。
- 停止并重新规划条件：统一状态要求修改 SQL/存储游标、必须改变 ListView 回调，或
  无法在不复制协调器的前提下提供 ATOM-43 所需观测。
- 回滚方式：主线集成前直接丢弃本地 Worker 提交；集成验证失败时由主 Agent abort
  集成事务，Worker 从最新 `main` 重建替代提交，不在集成分支手工修复。

## DDD 与执行记录

- 编码前 DDD：任务 `ddd_atom40_precode`，初审结论 `REVISE_PLAN`，修订后复审最终
  `PASS`。
- 编码前修订：拆分私有分页状态与公开纯性能快照，并固定
  `UiStateSnapshot.history_performance` 读取路径；把耗时更名并限定为
  `request_to_accept_duration`，使用 `checked_duration_since` 且明确不含 Slint 绑定/
  渲染；移除重复 `accepted_items`，分类计数与 `loaded_items` 同源；补充完整 reducer
  测试命令和签发 limit 超限整页拒绝契约。
- 编码前复审：`ddd_atom40_precode` 已确认全部修订项关闭，可以按当前契约编码。
- 提交前 DDD：任务 `ddd_atom40_final`，初审结论 `REVISE_CODE`；发现同一 generation
  再次成功首页仍累计旧页指标，以及计划中的 limit、失败污染和累计耗时边界测试未全部
  落地。
- 提交前修复：
  - 成功首页在去重和设置新游标前把私有 `HistoryPaginationState` 重置为默认值；同代次
    旧首页、续页的页数、重复数、分类数和累计耗时不会带入刷新首页。
  - 增加同代次“首页→续页→再次首页”回归，确认新首页指标从一页重新计数。
  - 增加签发 limit 等值合法、收缩 limit 超限整页拒绝、迟到/失败不污染成功指标和
    `Duration::MAX` 累计饱和测试。
- 提交前复审：任务 `ddd_atom40_final` 对修复后差异复现审查哈希一致，最终结论
  `PASS`；允许创建 ATOM-40 本地原子提交。
- 最终差异哈希：`42a2003b1912ce59a4f1bfcf207be59ed93ba331dfc69e4c4650bc7f5151fdf6`。
  该值覆盖写入本行之前的完整代码、测试和计划差异；本行只是对审查基线的事后标注。
- 哈希口径：先对唯一未跟踪计划执行
  `git add -N -- docs/planning/原子计划-ATOM-40-统一分页协调器.md`，再使用下列
  PowerShell 直接读取 Git 子进程 `StandardOutput.BaseStream`，对
  `git -c core.autocrlf=false diff --no-ext-diff --binary --` 的原始 stdout 字节计算
  SHA-256；不经过字符串解码、换行替换或手工拼接。

```powershell
$startInfo = [System.Diagnostics.ProcessStartInfo]::new('git')
@('-c', 'core.autocrlf=false', 'diff', '--no-ext-diff', '--binary', '--') |
    ForEach-Object { [void]$startInfo.ArgumentList.Add($_) }
$startInfo.RedirectStandardOutput = $true
$startInfo.UseShellExecute = $false
$process = [System.Diagnostics.Process]::Start($startInfo)
$sha256 = [System.Security.Cryptography.SHA256]::Create()
$hashBytes = $sha256.ComputeHash($process.StandardOutput.BaseStream)
$process.WaitForExit()
if ($process.ExitCode -ne 0) { throw "git diff 失败：$($process.ExitCode)" }
-join ($hashBytes | ForEach-Object { $_.ToString('x2') })
```
- 实际实现：
  - 在唯一 `HistoryPageCoordinator` 内收拢数据库游标、已加载数量、续页重试和活动
    请求签发时刻；`UiState` 不再重复持有游标或重试字段。
  - 增加首页替换、续页追加和两类失败的纯 reducer 决策；文本和图片按稳定 ID 共用同一
    去重、容量和指标状态转移。
  - 增加公开 `HistoryPerformanceSnapshot`，并通过 `UiStateSnapshot` 复制页数、唯一
    条目数、文本/图片分类、重复数和 request-to-accept 耗时，不暴露内部游标或 token。
  - worker 页超过签发 limit 时整页拒绝；原卡片、续页游标和成功指标保持不变。
  - request-to-accept 使用单调 `Instant` 和 `checked_duration_since`，时钟倒退按零；
    指标明确不等同于 SQL 查询耗时，也不包含 Slint 模型绑定或渲染。
- 实际验证：
  - 测试先行红灯：新混合页测试在新接口尚未实现时按预期编译失败。
  - `cargo test --lib history_query::tests`：修复前 14 项通过；提交前修复后 19 项通过。
  - `cargo test --lib app::ui_event::tests`：78 项通过。
  - 上述模块测试覆盖计划列出的五个 reducer 定向场景以及公开混合页性能快照。
  - `cargo check --lib`、修改文件 `rustfmt --edition 2021 --check`、中文注释检查和
    `git diff --check` 通过。
  - `cargo fmt --all -- --check` 被基线中未由本原子修改的多个 Rust 文件格式差异阻止；
    本原子没有越界重排这些文件，改以两份允许文件的直接 rustfmt 检查证明本次差异格式
    正确。
  - 提交前复审修复后，协调器模块 19 项、reducer 模块 78 项再次通过；`cargo check
    --lib`、修改文件 rustfmt、中文注释和差异检查再次通过。
- 本地提交：本计划与实现由同一个 ATOM-40 提交固化；实际 commit/tree 对象在提交后
  回报给主 Agent，作为不会产生自引用的最终完成证据。

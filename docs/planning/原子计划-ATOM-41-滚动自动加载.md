# ClipboardBoard ATOM-41 滚动自动加载原子计划

## 计划元数据

- 计划 ID：ATOM-41
- 类型：atomic-development
- 修订版本：5
- 状态：已完成，待主线集成
- 所属交付单元：UNIT-12
- 风险等级：L2
- 计划提交：`feat(ui): [ATOM-41] 支持历史无限滚动`

## 基线与依赖证明

- `base_main_sha`：`eeb9d07340a4001677885a9ec9a402aeaa901e02`
- 依赖原子：ATOM-40
- ATOM-40 主线集成提交：`eeb9d07340a4001677885a9ec9a402aeaa901e02`
- ATOM-40 Worker 提交：`6f234f1add1f91ff3b87bd301392183b20fb67c0`
- 祖先证明命令：
  `git merge-base --is-ancestor eeb9d07340a4001677885a9ec9a402aeaa901e02 HEAD`
- 祖先证明结果：退出码 `0`，ATOM-40 主线集成提交是当前基线祖先。
- 本地分支：`codex/atom41-history-scroll`
- worktree：
  `F:\workspace\small-projects\windows-copy-worktrees\history-perf`
- 远端约束：本分支不设置 upstream、不 push、不创建远端分支。

## 唯一目标

在 ATOM-40 唯一 `HistoryPageCoordinator` 之上补齐混合文本/图片列表的连续滚动接缝：
用户接近列表底部时自动请求下一页，追加期间保留选择与视口，布局回调抖动不重复请求，
并以固定状态区显示续页加载或失败重试。界面不出现页码、上一页或下一页按钮。

## 现状核对

### 已满足

- `ui/app-window.slint` 已使用 Slint `ListView`，并从 `viewport-y`、
  `visible-height`、`viewport-height` 三个真实几何变化回调上报视口。
- 文本卡片高度为 `106px`，图片卡片高度为 `186px`，同一模型已允许混合类型。
- `UiState::handle_history_viewport` 已按真实内容高度计算到底部的像素距离，而不是按
  列表索引猜测位置。
- ATOM-40 已保证同一数据集只有一个活动请求，并统一处理 30 条首页、50 条续页、
  精确请求身份、游标、去重、2,000 条容量、失败和重试。
- 续页追加保持数据库顺序，`UiState` 已按稳定 `id + content_hash` 保存选择身份。

### 尚未完全满足

- 当前底部阈值仍按两张 `106px` 文本卡片计算；图片卡片为 `186px`，混合列表的提前
  加载距离不足以稳定覆盖两张最高卡片。
- 三个 Slint 几何属性可能在一次布局中先后变化；当前单阈值布尔边沿没有进入/离开
  滞回区，阈值附近的像素抖动可能反复重新武装。
- 成功追加后仍保留“已进入底部”状态。如果首页或续页追加后内容仍不足以离开底部
  区域，后续布局通知可能无法继续自动补页。
- 历史页结果会重建 `VecModel`；目前只有缩略图结果显式保存并恢复
  `history-viewport-y`，续页追加可能造成视口跳回或滚动位置变化。
- 当前只有搜索加载文案和续页失败文案，没有独立的“正在加载更多”状态；失败提示
  又以条件节点插入布局，出现或消失时会改变列表可见高度并产生额外几何回调。
- 既有 reducer 测试覆盖 30+50+5 和失败重试，但没有锁定混合高度、回调乱序/抖动、
  追加后连续补页、视口保持以及加载提示生命周期。

结论：现状只满足基础文本/混合数据续页协议，不足以判定 ATOM-41 完成。

## 行为契约

### 混合卡片底部判定

1. 底部距离继续只使用 Slint 上报的 `viewport-y`、`visible-height` 和
   `content-height` 计算，不遍历卡片、不解码图片、不按列表索引推测。
2. 进入阈值至少覆盖两张当前最高的图片卡片，即以 `IMAGE_CARD_HEIGHT * 2` 为基准；
   当前固定为 `372px`。文本、图片和混合列表共用同一像素规则。
3. 离开阈值固定为 `558px`，形成有限滞回区：`distance <= 372` 为 inside，
   `distance > 558` 为 outside，`373..=558` 保持此前状态。三个几何属性乱序通知或
   阈值附近小幅抖动只能触发一次续页，只有真实离开离开阈值后才能重新进入。
4. 非法或尚未完成布局的几何值（不可见面板、非正可见高度、非正内容高度）不得签发
   请求，也不得破坏当前请求身份。

### 自动续页状态机

1. outside→inside 边沿只调用现有 `HistoryPageCoordinator::request_next_page`；不得
   创建第二套游标、token、容量或数据集 generation。
2. 活动请求期间任意数量的布局回调只能合并到该请求，不能生成第二个 token。
3. 每次成功接受 `Append`（包括去重后为空的追加）必须先进入独立绑定门禁。修订可用
   时登记 `ProbePending(revision)`；修订耗尽时登记 `RevisionExhausted`，仍冻结模型
   绑定期间回调。追加修订只标识一次 UI 绑定接缝，不复制分页 generation、token 或
   游标。
4. 从进入绑定门禁起，到对应 post-bind probe 被消费或耗尽绑定完成前，普通
   `HistoryViewportChanged` 仍可更新缩略图视口，但不得参与分页边沿或签发续页，
   避免模型绑定前后的旧回调误触发。
5. 只有完成追加模型绑定、恢复并夹紧 `viewport-y` 之后，窗口层才读取当时真实的
   `viewport-y + visible-height + content-height`，携带追加修订投递一次 post-bind
   probe。不得在 reducer 接受页时提前使用旧几何。
6. reducer 只接受与当前 `ProbePending` 精确匹配的第一条 probe，并在
   判断前原子消费 pending。旧修订、重复 probe 和无 pending probe 均丢弃。
7. 有效 probe 为 inside 时最多请求一页；为 outside 时恢复普通边沿的 outside 状态，
   等待用户重新进入。位于滞回区时沿用追加前状态，但本次 probe 仍只能产生最多一页。
   数据集已耗尽、达到容量、面板隐藏、身份失效或已有活动请求时不签发请求。
8. 每个成功 Append 最多提供一次绑定后探针机会，普通重绘不能形成请求风暴。若追加
   后内容仍不能填满视口或仍处于进入阈值内，由有效 probe 自动请求下一页；若追加后
   已离开阈值，则等待用户继续滚动。
9. 数据集耗尽、达到 2,000 条上限或面板隐藏时停止自动加载；切换搜索、标签、捕获
   刷新和重新打开继续沿用 ATOM-40 的数据集失效规则。
10. 续页失败保持当前卡片、选择、视口和数据库游标。失败后普通同区重绘不得重试；
   用户点击固定重试入口，或真实离开离开阈值后再次进入，才允许生成新 token。

### Probe 生命周期与失效

1. `begin_history_dataset`、搜索或标签切换、捕获刷新、首页 `Replace`、面板隐藏、退出
   以及协调器身份耗尽/失效必须在同一 reducer 状态转移中清除 Append 绑定门禁，并把
   `near_bottom` 重置为新数据集初态 outside。
2. 上述失效发生后，携带旧追加修订的迟到 probe 只能被丢弃；不得改变新数据集边沿、
   加载态、重试态、选择、视口或签发请求。
3. `next_append_revision` 使用 `checked_add` 分配。耗尽时继续应用 Append、保持选择与
   视口并进入 `RevisionExhausted`；模型绑定与视口恢复的 setter 返回后仍保持门禁，
   直到下一 UI 闭包才只解除门禁，不读取几何、不投递 probe、不自动续页。修订号禁止
   回绕。
4. 窗口弱引用无法升级时，必须按追加修订精确清除对应 pending。清理只取消本次
   post-bind 自动重武装，保留已经接受的卡片、选择和 reducer 已知视口状态。
5. 实现采用显式可失败调度接缝：Append 模型绑定与视口恢复完成后，通过现有
   `post_ui_event` 把带修订 probe 排入下一次 UI 闭包。`post_ui_event` 的
   `Result` 是调度/投递失败返回；失败时立即按修订调用取消接缝，不得留下永久门禁。
6. probe 调度成功但送达前发生数据集失效时，由第 1 条原子清 pending，迟到事件再由
   修订匹配拒绝。probe 正常送达或失败取消都必须解除“普通分页边沿暂停”门禁。
7. 窗口缺失或 probe 调度/投递失败后，当前同区不会自动补页；后续真实几何先到
   outside（`distance > 558`）再进入 inside（`distance <= 372`）时，应通过普通边沿
   恢复续页。失败不得永久禁用该数据集的滚动加载。

### 选择与视口稳定性

1. 续页只能追加，不能替换已显示卡片；跨页重复由 ATOM-40 去重，现有顺序不变。
2. 追加前记录选中项的 `id + content_hash`，追加后必须仍指向同一记录；不得仅依赖
   易被重排复用的索引恢复身份。
3. 续页追加重建 Slint 模型前读取当前 `history-viewport-y`，模型绑定后恢复并按新
   内容范围夹紧；用户视口不得跳到顶部，当前选中卡片不得跳到另一项。恢复过程不得
   调用键盘选择的 `ScrollSelection` 或 `scroll_selection_into_view` 路径。
4. 首页替换、搜索结果替换和新数据集不套用“追加视口保持”，避免把旧数据集滚动位置
   带入新结果。
5. 键盘上下选择和鼠标卡片选择继续使用既有路径；本原子不改变复制、收藏、删除行为。

### 加载与失败提示

1. “搜索中”只表示首页或搜索结果加载；“正在加载更多…”只表示续页请求已成功签发且
   尚未被成功、失败、提交失败、数据集失效或面板隐藏收口。
2. 续页加载态不得从 `search-status` 或“当前卡片非空”推断，避免保留旧搜索结果时
   把首页请求误显示为续页。
3. 续页加载提示与“加载失败，点击重试”互斥；失败收口必须关闭加载提示并显示重试，
   成功、耗尽、搜索切换和隐藏必须同时清除过期提示。
4. 列表下方保留固定高度的分页状态区。加载、失败和空闲只替换区内内容，不增删布局
   行，避免提示自身改变 `visible-height` 后制造滚动边沿抖动。
5. 重试入口继续是失败态唯一的同区主动入口；不新增传统分页按钮。

## 计划实现边界

### 允许修改

- `src/app/ui_event.rs`
  - 将单阈值底部布尔量收窄为有滞回和绑定后单次探针语义的滚动状态。
  - 增加独立 Append 绑定门禁、post-bind probe 修订及精确消费规则。
  - 维护独立续页加载态，并在所有请求收口和数据集失效路径清理。
  - 区分首页替换与续页追加的模型刷新效果，向窗口绑定层传递“追加需保留视口”事实。
  - 增加 reducer 直接单元测试。
- `ui/app-window.slint`
  - 保留现有 ListView 三个几何回调。
  - 把过期的“固定卡片高度”注释改为混合固定高度契约。
  - 增加固定高度分页状态区和互斥的加载/失败内容。
- `src/command.rs`
  - 允许增加携带追加修订和三元几何的私有语义 UI 事件；不得加入游标或请求 token
    的第二所有者。
- `tests/history_scroll.rs`
  - 允许使用 Slint testing backend 与测试组件验证真实混合布局、固定状态区和模型
    追加视口；测试不得 `show()` 真实程序窗口。
- `tests/comment_policy.rs`
  - 仅当新增生产 Rust 文件或公共类型需要纳入中文注释门禁时修改。
- `docs/planning/原子计划-ATOM-41-滚动自动加载.md`
  - 记录 DDD、实现、定向验证和提交证据。

### 明确禁止修改

- 不修改 `src/history_query.rs` 的分页协议、游标、容量、去重、请求身份或性能指标。
- 不修改 `src/storage/worker.rs` 的 SQL 和复合游标。
- 不修改缩略图解码、纹理缓存与 LRU；这些属于 ATOM-42。
- 不修改 `tests/list_performance.rs` 的最终性能阈值；这些属于 ATOM-43。
- 不增加页码、上一页、下一页或一次性加载全部历史。
- 不修改图片捕获/复制、收藏、删除、清空、配置、托盘、快捷键和隐私策略。
- 不修改共享状态文档：`AGENTS.md`、`原子开发任务计划.md`、
  `docs/planning/开发计划.md`、`docs/ai-project/项目工作台.md`、
  `docs/ai-project/项目阶段记录.md`。
- 不启动真实应用，不访问默认 `%LOCALAPPDATA%\ClipboardBoard`、真实剪贴板、托盘或
  注册表。

## 待编码前 DDD 审查的实现形态

以下名称是计划接口，不是已承诺实现；DDD 可在不改变行为契约的前提下进一步收窄：

```rust
struct HistoryScrollState {
    near_bottom: bool,
    next_page_loading: bool,
    next_append_revision: u64,
    append_binding_gate: AppendBindingGate,
}

enum AppendBindingGate {
    Idle,
    ProbePending(u64),
    RevisionExhausted,
}

enum HistoryModelRefresh {
    None,
    Replace,
    AppendPreservingViewport { append_revision: Option<u64> },
}
```

- `near_bottom` 使用 `372px` 进入和 `558px` 离开阈值更新。
- 成功追加只登记带修订的 post-bind probe，不立即重新武装或使用旧几何；probe 到达
  前的普通视口回调不得签发分页请求。
- post-bind probe 必须在 Append 模型绑定、旧视口恢复及合法夹紧全部完成后读取真实
  三元几何并投递；精确修订匹配后只消费一次。
- 追加修订使用 `checked_add`；`None` 表示修订号耗尽，本次 Append 仍进入
  `RevisionExhausted` 绑定门禁。setter 后通过 `invoke_from_event_loop` 安排下一 UI
  闭包只解除门禁，不读取几何、不调度 probe；窗口缺失或调度失败时立即解除。
- 计划提供按修订精确取消 pending 的私有接缝。窗口弱引用缺失或
  `post_ui_event(probe)` 返回错误时调用该接缝；它不得回滚已接受卡片。
- `next_page_loading` 在续页请求生成时开启，在接受结果、失败、提交失败、新数据集、
  隐藏和退出时关闭；不得复制协调器的 active token。
- `apply_history_page_result` 向同一次 UI 事件处理返回最小刷新效果，使窗口层只在
  `AppendPreservingViewport` 前后保存与恢复视口。
- 若 DDD 证明无需新结构，也可使用等价的私有字段与返回值；不得扩大公共 API。

## 实现顺序

1. 先补 reducer 失败测试，锁定 372/558 精确边界、滞回、同区回调合并、追加修订和
   post-bind probe 单次消费。
2. 将底部判定改为图片兼容进入阈值与更大的离开阈值，继续复用 ATOM-40 请求门禁。
3. 成功 Append 分配修订并暂停普通分页边沿；追加模型绑定、视口恢复与夹紧后投递
   唯一 post-bind probe。
4. 补齐 probe 取消矩阵：数据集失效、Replace、隐藏、退出、窗口缺失、投递失败和
   修订号耗尽都不得留下 pending。
5. 增加独立续页加载态，并覆盖成功、失败、提交失败、数据集切换和隐藏的全部收口。
6. 让历史页应用返回首页替换或带可选修订的续页追加效果；追加绑定不得调用选择滚入
   路径。
7. 把 Slint 分页提示改为固定高度状态区，绑定加载与失败互斥状态。
8. 增加 reducer 与 testing backend 交互契约测试，确认真实混合布局、选择身份、卡片
   顺序、视口保持与重试不回归。
9. 只运行本原子相关定向测试、允许文件定向格式和注释门禁；不执行全量测试。
10. 完成提交前差异 DDD；若差异实质变化，重新计算 diff 哈希并复审。

## 定向测试与验证

### reducer 单元测试

- 精确断言 `371/372` 为 inside，`373` 保持原状态，`558` 仍保持，`559` 为 outside。
- 三个几何回调乱序和 `371/372/373/558/559` 附近抖动只签发一个 token；超过
  `558px` 后才重新武装。
- 活动续页请求期间重复进入通知不生成第二请求。
- Append（包括空追加）登记唯一修订；probe 前旧普通回调不请求，匹配 post-bind
  probe 只消费一次，旧修订和重复 probe 丢弃。
- 成功追加后，匹配 probe 的新几何仍接近底部则恰好继续一页；新几何已离开则恢复
  普通边沿但不自动续页；耗尽状态不请求。
- 搜索、标签切换、捕获刷新、隐藏重开和首页 Replace 分别发生在 probe 投递前时，
  旧 probe 不请求且不污染新数据集的 near/loading/retry 状态。
- 注入窗口弱引用缺失与 `post_ui_event` 调度失败，断言对应 pending 被精确清除、卡片/
  选择/视口不回滚，并可在后续真实 outside→inside 后恢复普通续页。
- 将 `next_append_revision` 置于耗尽边界，断言使用 `checked_add`、不回绕、不遗留
  pending，Append 卡片仍被接受。
- 30+50+5 混合卡片连续加载后无重复、无遗漏，耗尽后不再请求。
- 续页失败关闭加载态并保持游标；同区重绘不重试，点击或离开再进入可重试。
- 续页提交失败、搜索或标签切换、捕获刷新、面板隐藏均关闭旧加载态。
- 追加前后选中 `id + content_hash` 不变；首页替换仍使用既有首项和身份恢复规则。

### UI 与模型绑定测试

- `tests/history_scroll.rs` 使用 Slint testing backend/测试组件，不调用真实程序
  `show()`，不访问剪贴板、托盘、默认应用目录或注册表。
- 真实混合卡片 `106px + 186px` 产生预期 `content-height`；三个真实几何回调仍存在且
  参数顺序不变。
- 固定分页状态区空闲、加载和失败切换时 `history-visible-height` 恒定；续页在途只
  显示“正在加载更多…”，失败只显示“加载失败，点击重试”。
- 续页追加绑定前后的 `history-viewport-y` 相同或仅按新合法范围夹紧，不跳回顶部，
  且测试探针证明没有调用选择滚入路径。
- 追加前后选中项的 `id + content_hash` 以及复制按钮解析出的稳定身份不变。
- 鼠标选择、键盘上下选择、复制按钮和 Esc 行为不受影响。

### 计划验证命令

```powershell
cargo test --lib app::ui_event::tests::混合卡片底部阈值提前加载
cargo test --lib app::ui_event::tests::几何抖动只签发一次续页
cargo test --lib app::ui_event::tests::追加后绑定探针按新几何继续补页
cargo test --lib app::ui_event::tests::旧普通回调和重复探针不得误触发
cargo test --lib app::ui_event::tests::数据集失效清除旧追加探针
cargo test --lib app::ui_event::tests::探针投递失败解除门禁并可恢复
cargo test --lib app::ui_event::tests::追加修订耗尽不回绕
cargo test --lib app::ui_event::tests::续页加载态在全部收口路径关闭
cargo test --lib app::ui_event::tests::滚动续页按三十加五十加五加载八十五条
cargo test --lib app::ui_event::tests::续页失败保持游标并要求明确重试
cargo test --lib app::ui_event::tests::跨页卡片保持完整选择和复制能力
cargo test --lib app::ui_event::tests::混合卡片选择边界累加图片高度
cargo test --test history_scroll
cargo check --lib
rustfmt --edition 2021 --check src/app/ui_event.rs src/command.rs
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/check_comments.ps1
git diff --check
```

- 测试名称可在编码前 DDD 后按实际最小实现调整，但必须保留上述行为覆盖。
- `src/command.rs` 未修改时从定向 `rustfmt` 命令移除。全仓
  `cargo fmt --all -- --check` 只记录基线诊断，不作为本原子对范围外文件的整改入口；
  实际门禁仅对允许修改的 Rust 文件执行定向 `rustfmt --check`。
- 本原子不执行全量测试、不启动桌面程序、不执行真实 Windows 全局状态测试。

## 完成判定

- 文本、图片和混合列表在真实底部几何下连续自动续页，无传统分页按钮。
- 三个布局回调和 372/558 边界抖动不产生重复请求；Append 只有匹配修订的单次
  post-bind probe 可按绑定后新几何继续补页。
- 搜索、标签、捕获刷新、Replace、隐藏、退出和协调器失效均清除旧 pending；窗口
  缺失或投递失败不会永久封锁普通 outside→inside 边沿。
- 追加修订号检查递增且永不回绕；耗尽仍冻结绑定期旧回调，绑定完成后只关闭本次自动
  probe，不丢弃已接受卡片；后续真实 outside→inside 可以恢复普通续页。
- 30+50+5 混合结果无重复、无遗漏，耗尽或达到容量后停止。
- 续页追加不改变已选记录身份，视口不跳回顶部。
- 固定分页状态区准确区分加载、失败和空闲，提示变化不改变列表布局高度。
- ATOM-40 仍是唯一分页协调器；本原子没有复制游标、token、容量或 generation。
- 所有定向验证、中文注释检查、格式检查和 `git diff --check` 通过。
- 编码前与提交前两个隔离 DDD 均完成，任务名、结论和最终 diff 哈希记录在本文档。
- Worker 只创建一个本地原子提交，不 push、不设置 upstream。

## 风险与回滚

- 主要风险：加载提示改变布局反向触发续页；模型绑定前旧回调抢先触发；重复/迟到
  probe 形成自动请求风暴；失效或调度失败遗留 pending 永久封锁分页；修订号回绕接受
  旧 probe；恢复旧视口覆盖新数据集顶部；把首页加载误标为续页；失败后普通重绘绕过
  明确重试。
- 控制方式：固定状态区高度、372/558 双阈值、活动请求门禁、追加修订精确匹配、
  checked_add、pending 原子消费和取消矩阵、probe 前暂停普通分页边沿、刷新效果显式
  区分 Replace/Append，并用稳定身份断言选择与复制。
- 停止并重新规划条件：必须修改 SQL/存储游标、必须复制分页协调器、Slint 无法在模型
  追加后恢复合法视口，或固定状态区导致现有窗口无法容纳核心操作。
- 回滚方式：主线集成前直接丢弃本地 Worker 提交；集成验证失败时由主 Agent abort
  集成事务，Worker 从最新 `main` 重建替代提交，不在集成分支手工修复。

## DDD 与执行记录

- 编码前 DDD：任务 `ddd_atom41_precode`，初审结论 `REVISE_PLAN`。
- 初审要求：增加带追加修订的单次 post-bind probe；锁定 372/558 精确滞回边界；
  使用 Slint testing backend 验证真实混合高度、固定状态区、Append 视口和稳定身份；
  全仓格式仅记基线诊断，允许文件执行定向格式门禁。
- 第一次修订结果：修订版本 2 已逐项纳入上述要求。
- 编码前 DDD 复审：任务 `ddd_atom41_precode_v2`，结论 `REVISE_PLAN`；仅剩 probe
  生命周期未覆盖全部数据集失效、窗口缺失、投递失败和修订号耗尽。
- 第二次修订结果：修订版本 3 已增加原子取消矩阵、显式可失败 `post_ui_event`
  调度接缝、outside→inside 恢复、迟到 probe 隔离及 `checked_add` 耗尽规则，待再次
  复审。
- 编码前 DDD 最终复审：任务 `ddd_atom41_precode`（v3 复审），结论 `PASS`；允许按
  修订版本 3 测试先行实施。
- 实现结果：
  - 普通滚动采用 `372px` 进入、`558px` 离开滞回；绑定等待期间产生的回调在 Slint
    回调边界冻结为 `HistoryViewportChangedDuringAppend`，迟到后也不参与分页。
  - Append 使用独立三态绑定门禁、检查递增修订和下一 UI 闭包 post-bind probe；
    修订耗尽仍冻结绑定期回调，窗口缺失、调度/投递失败和数据集失效均按计划解除门禁。
  - 续页追加保存并恢复合法视口，不调用选择滚入；续页加载态与搜索加载态分离。
  - Slint 分页状态区固定为 `32px`，空闲、加载和失败只替换区内内容。
- 定向验证：
  - `cargo test --lib app::ui_event::tests`：通过，87 项通过。
  - `cargo test --test history_scroll -- --test-threads=1`：通过，2 项通过；测试后端未
    `show()` 应用窗口。
  - `cargo check --lib`：通过。
  - `rustfmt --edition 2021 --check src/app/ui_event.rs src/command.rs
    tests/history_scroll.rs`：通过；未把全仓格式作为范围外整改门禁。
  - `& .\scripts\check_comments.ps1`：通过，中文文件级注释检查通过。
  - `git diff --check`：通过。
- 提交前 DDD：任务 `ddd_atom41_final`，初审结论 `REVISE_CODE`；发现修订耗尽时
  `None` 同时取消了可投递 probe 与模型绑定门禁，旧几何可能在绑定期间误触发。
- 提交前第一次修复：修订版本 4 将绑定门禁改为 `Idle / ProbePending(revision) /
  RevisionExhausted` 三态。耗尽 Append 仍冻结绑定期乱序回调，模型与视口恢复后解除
  门禁且不自动续页；只有后续真实 outside→inside 才恢复普通加载。待同一 DDD 复审。
- 提交前 DDD 第二次复审：任务 `ddd_atom41_final`，结论仍为 `REVISE_CODE`；指出
  Slint setter 返回后仍可能产生延迟布局回调，不能同步解除 `RevisionExhausted`。
- 提交前第二次修复：修订版本 5 把耗尽门禁解除安排到下一 UI 闭包；该闭包只解除
  gate，不读取几何、不发送 probe。窗口 weak 缺失或调度失败可立即解除；setter 后及
  下一闭包前产生的回调继续冻结，迟到事件在解除后仍保持旧布局语义。
- 提交前 DDD 最终复审：任务 `ddd_atom41_final`，结论 `PASS`；主 Agent 现场计算的
  raw diff 哈希与复审输入一致，允许创建 ATOM-41 本地原子提交。
- 定向验证：已完成。
- 本地提交：待创建。

# ATOM-43R 混合高度几何窗口模型修复计划

## 计划元数据

- 计划 ID：`ATOM-43R`
- 类型：`atomic-development`
- 修订版本：`11`
- 状态：`completed-local-commit-pending-integration`
- 父级 ID：`WCB-PLAN-001 / ATOM-43`
- 创建基线：`05e98b6`
- 风险等级：`L3`
- 依赖：ATOM-42 已集成；ATOM-43 性能门禁已有失败证据。
- 本地分支：`codex/atom43-geometry-repair`
- Worktree：`F:\workspace\small-projects\windows-copy-worktrees\geometry-repair`
- 远端约束：本分支只在本地提交，不设置 upstream、不 push、不创建远端分支。

## 背景证据

ATOM-43 在 Windows x64 Release 的 20,000 条交错文本/图片列表上失败：文本卡片高度
106px、图片卡片高度 186px，真实内容高度 `2,653,333.5`，而精确前缀和应为
`2,920,000`；200 次滚动有 81 个视口边界/方向不匹配，无法到达真实底部。

根因已由 Slint 1.17.1 源码核对：compiler-magic `ListView` 只缓存当前已实例化
delegate 的平均高度，再用平均高度乘模型行数推算 `viewport-height` 和 anchor。交错
首屏的 `[106, 186, 106]` 平均值 `132.6667` 乘 20,000 正好得到失败观测值。不能通过
放宽阈值、调换数据顺序或统一伪造 delegate 高度来掩盖该缺陷。

## 唯一目标

用应用侧显式几何协议替代混合可变高度 compiler `ListView` 的平均高度推算，使文本
106px、图片 186px 的真实前缀和、视口边界、窗口化行范围和卡片操作身份始终一致，同时
保留最多 100 行附近的窗口化渲染和 ATOM-43 原有性能阈值。

## 支持的验收场景

1. 20,000 条交错文本/图片摘要的精确内容高度为 `2,920,000`，误差不超过 1px。
2. 顶部、中部和底部视口均被 clamp 到合法范围；最大负偏移精确为
   `-max(content_height - visible_height, 0)`，不会反向或越界。
3. 非空数据集的可见窗口使用 prefix-sum + 二分查找计算，窗口行数含 overscan 但始终不超过 100；
   空数据集窗口为 `[0,0)`、无 overscan 且不触发续页；首行和末行滚动样本均能访问到，
   首行和末行滚动样本均能访问到，模型不把完整 20,000 行交给 UI 重复器。
4. 点击、复制、收藏、删除使用绝对快照索引或稳定身份，不因窗口本地索引重排而误操作。
5. 原 ATOM-43 Release 探针改为消费显式几何/窗口协议后，保留 20,000 条、200 次滚动、
   呼出、首批、工作集、P95 和 ATOM-42 LRU 门禁；geometry、滚动边界和窗口化证据全部通过。

## 当前行为与目标行为

- 当前：`ui/app-window.slint` 的 `ListView` 通过 compiler-magic 平均已实例化 delegate
  高度推算混合总高，Rust 只收到不可信的 viewport-height。
- 目标：Rust 几何 reducer 计算 `prefix[i]`、精确 `content_height`、clamp 后 viewport
  和 `[start,end)` 窗口；Slint 只渲染受控 `window_cards`/`window_offsets`，内容画布的
  高度来自 Rust 的精确协议，不再让 ListView 推算混合总高。

## 代码定位依据

- `ui/app-window.slint`：`ListView` 组件、`history-list`、卡片循环和 viewport 回调。
- `src/app/ui_event.rs`：`handle_history_viewport`、`selection_item_bounds`、选中/追加
  事件以及窗口卡片模型同步。
- 可新增 `src/app/history_geometry.rs`：纯 Rust prefix-sum、二分窗口、clamp 和本地到
  绝对索引映射；只通过 `src/app/mod.rs` 注册。
- `tests/history_scroll.rs`：已有滚动/续页边界回归。
- `tests/list_performance.rs`：ATOM-43 Release 混合性能探针和窗口化访问证据。
- `docs/planning/原子计划-ATOM-43-混合列表性能门禁.md`：原门禁失败证据；不得改低原有
  P95、工作集、20,000 行和 200 次滚动阈值。

## 允许修改

- `ui/app-window.slint` 中历史视口、卡片窗口模型和相应绝对定位/回调接缝。
- `src/app/history_geometry.rs`（新增）及 `src/app/mod.rs` 的最小模块注册。
- `src/command.rs` 中历史 viewport/window 事件 DTO 及其 Slint 接线所需的 revision/origin 字段。
- `src/app/ui_event.rs` 中消费几何协议、viewport clamp、窗口卡片和绝对身份映射的必要代码。
- `Cargo.toml` 中为 Windows session nonce 启用已有 `windows-sys` 的 `Win32_Security_Cryptography` feature；
  不新增运行时依赖或改变非 Windows 构建。
- `tests/history_scroll.rs`、`tests/list_performance.rs`、`scripts/measure_mixed_list_performance.ps1` 及几何纯函数窄测试。
- 本计划文档及必要中文注释。

## 明确禁止修改

- 不修改 SQLite、剪贴板读取/写回、图片解码、缩略图 loader、ATOM-42 LRU、分页查询或
  StorageExecutor。
- 不修改 ATOM-14R 旧固定高度探针及其阈值。
- 不修改 ATOM-43 原有呼出、首批、工作集、滚动 P95 和 LRU 门禁阈值；只替换其几何/窗口
  证据来源为显式协议。
- 不把完整 20,000 行复制成 UI 卡片；不使用平均高度、固定伪高度或放宽门禁掩盖错误。
- 不启动真实程序、剪贴板、托盘、注册表或默认用户数据目录。

## 实现步骤

1. 新增纯几何协议：使用整数或可证明无损的高精度数值保存每行高度和 prefix；覆盖空、
   单项、全文本、全图片、交错和随机混合输入。
2. 用二分查找从 clamped viewport 求可见起止行并加有限 overscan；若窗口超过 100 行则
   逐步收缩 overscan，仍超过时 fail-closed，不发布半成品窗口。
3. 将 UI 历史区域从 compiler-magic `ListView` 改为普通 `Flickable` + 精确 content
   canvas，窗口卡片绝对定位到 canvas；保留文本/图片各自原始 106/186 外层高度和现有
   操作按钮视觉尺寸。
4. 将 viewport 事件改为消费协议的精确 content_height，并在 Rust 侧 clamp；卡片回调
   发送 dataset/window revision 加稳定绝对身份，窗口刷新不改变选中对象；追加/替换先
   发布新 prefix 快照，再接收新的 viewport 事件。
5. 将 selection bounds、缩略图驻留范围、续页底部判断和性能探针统一到同一个几何 reducer，
   避免重复的高度公式；新增拥有型 `set_history_geometry_metadata(Vec<HistoryGeometryItem>)`
   接缝提供逻辑数量、类型和高度，不触发 UI `row_data`；保留既有 `set_cards` 兼容适配（包括旧
   ATOM-14R 固定高度探针）。调用 `set_history_geometry_metadata` 后进入显式几何模式，生产看板和
   新 ATOM-43R 探针只通过 `set_window_commit(WindowCommit)` 传递最多 100 行；未提供 metadata 的旧
   `set_cards` 调用继续走 legacy fixed-height ListView 兼容模式，不能被当作 ATOM-43R 混合窗口证据。
6. 更新 ATOM-43 探针，明确使用 Release 配置运行；逻辑数据集仍绑定完整 20,000 条拥有型摘要和
   每项类型/高度元数据，但 UI 只接收已提交的有界窗口模型，断言精确 content_height、首中末窗口和
   200 次真实视口更新。旧 `set_cards` API 仅作为兼容接缝，不能让 CountingModel 全量 `row_data`
   访问冒充窗口证据。

## 接口与不变量

- 几何 DTO 至少包含 `dataset_revision`、`window_revision`、`total_count`、`total_height`、
  `visible_height`、`viewport_y`、窗口起止绝对索引以及每项绝对索引/top/height；内部字段
  不可携带剪贴板正文。所有 viewport、window、卡片回调都必须携带或闭包捕获这两个 revision，
  旧 revision 事件直接丢弃。
- 视口最大偏移统一定义为 `max_offset = max(total_height - visible_height, 0)`，因此短数据集也满足
  `0 <= -viewport_y <= max_offset`；任何坐标转换都先使用该值 clamp。
- 当 `total_count = 0` 时，窗口长度必须为 `0`、窗口为 `[0,0)` 且 `total_height = 0`；仅当数据集非空时，
  窗口长度才必须落在 `1..=100`，空数据集不得伪造一行占位卡片或触发续页。
- `prefix[0] = 0`，`prefix[n] = total_height`，每项 `height > 0` 且
  `prefix[i+1] - prefix[i]` 等于该卡片的 106/186 高度。
- 非空窗口包含可见行加 overscan，计算后必须 `1 <= len <= 100`；空窗口固定为 `[0,0)`。
  若非空输入导致超过 100 行、
  revision 耗尽、prefix/坐标溢出或 f64→Slint length 转换非有限，则 fail-closed，保持上一
  个已验证窗口且不触发续页。overscan 只能逐步缩小，不能放宽上限。
- 任一窗口本地 index 必须先映射到当前窗口 DTO 的绝对索引和稳定身份；旧窗口事件不能作用
  于新 dataset 或新 window revision。
- 数据集 replace/append 必须先在内存中完成新 prefix 与窗口快照，再一次性发布新的
  `dataset_revision/window_revision`；发布前保存当前 viewport 的绝对锚点或稳定选中身份，
  发布后 clamp 到新总高，禁止在旧 prefix 上继续分页或选择。revision 水位属于当前进程的
  `HistoryWindowState` 生命周期，初始空快照为 `dataset_revision=1/window_revision=1`，每次数据集
  变化均使用 `checked_add`；session nonce 在进程启动时由 Windows `BCryptGenRandom` 生成非零 `u128`，
  失败则不启用窗口协议；它只在本进程内 immutable、不得持久化或重置，revision/token 溢出均 fail-closed。
  测试可注入非零 deterministic nonce，但每个新 session 必须不同；重开进程不得接受带旧进程 nonce 的事件。
- `WindowCommit` 是唯一发布单元，字段包含 dataset/window revision、session nonce、cards、offsets、
  start/length、content-height、visible-height、clamped viewport 和 `commit_checksum: [u8; 32]`。
  checksum 使用 BLAKE3 对固定小端序 canonical descriptor 计算：`session_nonce` 为 16 字节 `u128`，
  所有 revision、索引、数量和 token 为 8 字节 `u64`，total/visible/clamped/top/height 为 8 字节有符号
  `i64`，`origin_token=None` 编码为 1 字节 `0`（有值编码为 `1` 后跟 8 字节 token），descriptor 依次包含
  `session_nonce/dataset_revision/window_revision/commit_revision/start/length/total_count/total_height/
  visible_height/clamped_viewport_y/origin_token` 以及每项 `absolute_index/id/content_hash/top/height`；
  几何源值全部以整数像素进入 descriptor；若接缝收到 f64/Slint length，先拒绝 NaN/Infinity，
  将 `-0` 规范化为 `+0`，再以 checked round-to-nearest 转为 i64，无法无损或越界时 fail-closed，
  不直接 hash 平台相关 f32 bits。不得把 Slint Model、图片句柄或指针写入摘要。状态机固定为
  `Building -> Ready -> Published`：进入
  Building 入口先把 `commit_ready=false`；之后所有 cards/offsets/start/length/total-count/total-height/
  visible-height/clamped-viewport/origin-token setter 和模型安装都不可接受事件；Ready 阶段
  校验 prefix、窗口上限并计算 descriptor/checksum；先按 cards/offsets/start/length/content-height 顺序
  写入 UI 暂存属性（同时写入 visible-height/clamped-viewport/origin-token），再由单一
  `publish_commit_stamp` 接缝一次性安装 `commit_revision + checksum`，最后把 `commit_ready=true` 标记为
  Published。任何中间 setter 回调因未 Published、revision 或 checksum
  不匹配而丢弃，不能进入 reducer；只有 Published 且 `commit_revision == window_revision` 的事件可被接受。
- 事件 DTO 必须包含 `session_nonce`、`dataset_revision`、`window_revision`、`commit_revision`、
  `commit_checksum`、`origin_token`（用户手势为 `None`）、viewport/visible 尺寸、窗口起止绝对索引、
  稳定卡片 ID 和 content hash；接受谓词必须同时满足 session、dataset、window、commit revision/checksum
  全部匹配，任一缺失或不匹配都只丢弃事件。
- 窗口因 clamp 被程序主动修正时，生成进程内唯一且 checked 递增的 origin token，并绑定
  `(session_nonce, dataset_revision, window_revision, target_viewport_y, commit_revision)`；回调必须完整匹配
  该绑定才确认并清除一次，用户先滚动或目标偏移改变时只丢弃迟到 token，不得清除新 token 或抑制用户事件。
- 每次 dataset 内容、窗口起止、卡片身份或高度发生变化都必须产生新的 window revision；append/replace
  的新快照发布必须原子地替换旧快照，禁止 Slint 同时观察新 cards 与旧 offsets。
- 卡片回调使用 `UiClipboardItem.id + content_hash` 作为稳定身份，同时携带当前窗口绝对索引和两种
  revision；local index 只能用于查找当前窗口 DTO。选中项不在窗口时必须按该身份重新计算窗口并再提交，
  禁止将窗口外选择直接当作 local index；ID 复用或 hash 变化都必须提升 dataset/window revision。
- 显式几何模式只把精确 `content_height` 写入单一 canvas `Rectangle.height`；该模式的 Flickable
  不包含 compiler-magic `ListView`，禁止第二个高度来源。legacy fixed-height adapter 可以保留独立
  `ListView` 分支，仅服务未提供 metadata 的旧 `set_cards`/ATOM-14R 探针：完整逻辑数量仍由模型提供，
  重复器每次只访问有界窗口（`row_data <= 100`、不创建 20,000 个子组件），旧属性和固定 106px 阈值
  原样保留；它不参与混合高度 geometry/window 证据。Rust 侧使用 checked `u64`/`f64` 计算，
  转换为 Slint `length` 前确认有限且不超过 `f32::MAX`；回传事件的 i32 坐标使用同样的
  checked 边界。
- geometry 计算失败或数值溢出时 fail-closed，不更新 UI 窗口、不触发续页，不伪造高度。
- `total_height`、`visible_height`、每项 `top/height` 和数量的整数值必须非负；有符号
  `clamped_viewport_y` 必须满足 `-max_offset <= clamped_viewport_y <= 0`（viewport 负向坐标单独按
  `max_offset` 约束）。负尺寸、负数量或 checked 转换失败直接拒绝 WindowCommit，避免
  `max_offset` 被异常输入放大。
- `history-model-length` 继续表示完整逻辑数据集数量，不能改成窗口长度；逻辑类型/高度元数据与 UI
  窗口模型分离，窗口协议另行暴露
  `window-start`、`window-length` 和每项绝对索引，分页/续页判断只消费完整数据集计数与同一快照。
- prefix 区间统一为半开区间 `[prefix[i], prefix[i+1])`；viewport 命中恰好位于边界时，二分规则必须
  固定选择右侧新行，底部命中 `total_height` 时只允许返回最后一行。输出到 Slint 前执行有限性、`f32::MAX`
  和 i32 事件坐标的 checked 量化，无法无损表示时 fail-closed。

## 测试要求与验证命令

### 纯几何窄测试

- 空、单项、全文本、全图片、交错、随机混合 prefix 总高和每项边界。
- 20,000 条交错输入总高 `2_920_000`；窗口长度、overscan、首中末行和短数据 `max_offset=0` clamp。
- 半开 prefix 边界、空窗口、checked 量化失败、本地索引到绝对索引映射、dataset/window revision 和
  origin token 迟到/重复事件隔离。
- WindowCommit canonical BLAKE3 checksum 的逐字段篡改、旧 checksum、session nonce 隔离、Building/Ready
  中间 setter 丢弃和 Published 接受谓词；`id + content_hash` 变化必须提升 revision。
- canonical descriptor 每个固定宽度字段、`origin_token=None` 编码、checksum 跨进程重放和任一字段篡改
  都必须有断言；legacy `set_cards` 模式与 metadata/WindowCommit 模式分别验证，旧 ATOM-14R 探针继续
  保留其完整模型与 row_data 证据，新 ATOM-43R 探针断言 metadata 路径的 `row_data_access_count=0`。

### UI/性能窄测试

- `cargo test --lib app::history_geometry -- --test-threads=1`
- `cargo test --test history_scroll -- --test-threads=1`
- `cargo test --test list_performance geometry_window_contract -- --test-threads=1`（必须实际运行且
  解析 `1 passed`；不得仅靠 ignored 测试命令返回 0）
- `cargo test --test list_performance set_cards_window_separation -- --test-threads=1`（必须实际运行，
  断言逻辑 metadata 可证明 20,000 条但 CountingModel 的 `row_data_access_count` 始终为 0，窗口模型
  `max_batch_rows <= 100`）
- `cargo test --test list_performance --no-fail-fast -- --test-threads=1`（只执行其余非 ignored
  几何/窗口用例；Release ignored 探针由脚本单独运行）
- `cargo test --release --locked --test list_performance '测量一万文本与一万图片混合列表' -- --ignored --nocapture`
- `pwsh -NoProfile -File scripts/measure_mixed_list_performance.ps1`
- `cargo check --lib --tests`、目标 Clippy、目标 rustfmt、`git diff --check`。
- Release 脚本必须消费窗口协议输出的 `logical_item_count`、`window_start`、`window_length`、
  `window_first_absolute`、`window_last_absolute`、`dataset_revision` 和 `window_revision`，并断言逻辑总数
  仍为 20,000、窗口长度在 1..=100（空集另测为 0）、首中末绝对索引正确；旧 ATOM-14R 字段和阈值保持原样。
- 旧 ATOM-14R Release 固定高度探针命令 `cargo test --release --locked --test list_performance
  '测量两万条固定高度摘要列表' -- --ignored --nocapture` 保持原样运行并断言完整 `set_cards` 模型长度、
  旧属性、源码既有 106px/阈值和 `row_data` 单次访问不超过 100；默认无 metadata 才走该 legacy 分支，
  metadata 一旦设置绝不启用 ListView。该回归证据与 ATOM-43R 混合证据分开记录。

禁止运行全量测试；若基线格式差异导致全仓格式检查失败，只记录并保持本原子 diff 最小。

## DDD 门禁

- 编码前 DDD：由独立隔离上下文审查 Slint 平均高度根因、显式几何协议、窗口上限、绝对
  身份和禁止范围；必须 `PASS` 后编码。
- 提交前 DDD：复核完整 diff、Release 门禁证据、内存/窗口化约束、迟到事件和旧 ATOM-14R
  不变；必须 `PASS` 后提交。

### 修订记录

- v2：补齐窗口超过 100 行时的 fail-closed、dataset/window revision 传输、append/replace
  的 prefix 快照发布顺序、单一 canvas 高度来源和 Slint length checked 转换；补充
  `set_cards` 兼容接缝与 Release ignored 探针的显式验证口径。
- v3：补齐空数据集窗口长度、revision 起始与单调性、程序 clamp 的 origin token 及事件回路抑制，
  并明确 `history-model-length` 保持完整数据集计数、窗口长度使用独立字段。等待编码前隔离 DDD 结论。
- v4：根据编码前 DDD 纳入 `src/command.rs` 和 Release 测量脚本；定义短数据 `max_offset`、
  WindowCommit gate、checked revision、稳定 ID/绝对索引回调、半开 prefix 边界和 f32/i32 量化失败；
  新增必须实际运行的 `geometry_window_contract` 非 ignored 测试及脚本窗口字段门禁。
- v5：统一所有短数据和底部坐标为 `max_offset`；闭合空窗口规则、WindowCommit 状态机、session nonce、
  origin token 目标绑定、id+hash 稳定身份和逻辑元数据/有界 UI 模型分离，并明确旧 set_cards 不能贡献
  20,000 行窗口化证据。
- v6：根据再次编码前 DDD 固化 BLAKE3 canonical descriptor、单一 `publish_commit_stamp` 发布时序、
  BCryptGenRandom 进程 nonce 生命周期、`set_history_geometry_metadata`/`set_window_commit` 输入接缝，
  以及 checksum/gate/session/set_cards 分离窄测试与 row_data 访问硬断言。
- v7：纳入 `Cargo.toml` 的 Cryptography feature；明确既有 `set_cards`/ATOM-14R 保持兼容，
  但 ATOM-43R 的 20,000 条逻辑证明只来自拥有型 metadata，bounded window 只来自 WindowCommit，
  避免旧探针与新窗口化证据互相污染。
- v8：固定 canonical descriptor 的字节宽度与 `None` 编码；明确 metadata 触发显式几何模式、旧
  `set_cards` 保留 legacy fixed-height 兼容模式，新旧探针各自的 row_data 证据边界。
- v9：补充 f64/length 的 NaN、Infinity、`-0` 规范化和 checked i64 量化；闭合 legacy fixed-height
  adapter 的独立 ListView 兼容路径、逻辑全量计数与单次 `row_data <= 100` 回归，明确它不参与混合几何证据。
- v10：补齐 WindowCommit 所有 setter（含 visible/clamp/origin）的 Building 门禁和单一 stamp 发布，
  拒绝负尺寸/数量，明确 ATOM-14R 固定高度 Release 回归命令及 metadata 一旦设置不再启用 ListView。
- v11：根据提交前 DDD 修订显式 viewport 事件接线、一次性 origin-token 回路和窗口卡片操作身份；
  `WindowCommit.validate` 现在拒绝窗口内部 gap/越界，revision 在发布前 checked 预留，Slint
  `Flickable.viewport-height` 绑定精确 canvas 高度；Release 探针读取实际发布后的 window start/length/model
  与 commit revision，legacy 分离测试绑定完整 20,000 行模型并断言 row_data 为零。

## 完成判定

- 纯几何、UI/滚动和 Release ATOM-43 门禁全部通过；无阈值放宽或伪造证据。
- 只修改允许范围，中文注释/差异检查通过，worker worktree 干净。
- 生成唯一含 `ATOM-43R` 的本地提交；主 Agent 集成前复跑同一窄验证。

## 执行记录（编码阶段）

- 新增 `HistoryGeometry` 整数 prefix-sum、二分窗口、clamp 和最多 100 行 overscan；空集保持 `[0,0)`。
- 新增 `WindowCommit` canonical BLAKE3 checksum、Building/Ready/Published 门禁、session nonce、origin token 和稳定 ID/哈希身份校验。
- UI 增加 metadata + bounded Flickable 精确画布，显式模式绑定 `viewport-height`，legacy `set_cards` 固定高度 ListView 仍独立保留；显式卡片回调统一生成带 WindowEventIdentity 的选择/复制/收藏/删除事件。
- ATOM-43 Release 探针改为消费 metadata/WindowCommit 的真实 `window-start`、`window-length`、bounded cards model 和发布 revision；脚本新增逻辑数量、窗口起止和 revision 门禁。
- 窄验证：纯几何 4 passed；WindowCommit 3 passed（含连续区间/总高度边界）；显式 viewport token/迟到隔离 1 passed；显式卡片身份 1 passed；`history_scroll` 2 passed；`geometry_window_contract` 1 passed；`set_cards_window_separation` 1 passed（完整 20,000 行 legacy 模型 row_data=0）；Release 混合探针和 PowerShell 门禁通过（20,000 条、总高 2,920,000、窗口最长 23、200 次滚动、工作集 31.457 MiB、长滚动 P95 0.196 ms、LRU 窄测通过）。
- 提交前 DDD：PASS。根据 DDD 建议修复 Slint length 的范围判断 Clippy 告警，并将
  `WindowCommitBuilder::set_window` 收敛为带中文契约注释的 `WindowCommitPayload` DTO；
  之后已 rebase 到最新 `main`（`6b182ab`）并重跑窄验证。
- rebase 后验证：`cargo check --lib`、lib/目标 integration Clippy（`-D warnings`）、5 个几何/UI
  unit、`history_scroll` 3 项、`geometry_window_contract`、`set_cards_window_separation`、
  `git diff --check` 全部通过；Release/PowerShell 门禁通过（20,000 条、总高 2,920,000、窗口最长
  23、200 次滚动、呼出 P95 0.145 ms、首批 0.116 ms、工作集 31.516 MiB、长滚动 P95 0.166 ms、
  ATOM-42 LRU 窄测通过）。当前只待创建本地唯一提交，不推送远端。

## 交付给下一原子

输出显式 `HistoryGeometry`/窗口协议和可重复的混合 Release 性能证据，供后续 ATOM-57
直接消费；不改变分页、图片资产和存储契约。

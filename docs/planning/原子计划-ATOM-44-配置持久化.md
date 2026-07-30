# ATOM-44 配置持久化原子计划

## 计划元数据

- 计划 ID：ATOM-44
- 类型：atomic-development
- 修订版本：4
- 状态：completed
- 父级 ID：WCB-PLAN-001 / UNIT-13
- 创建基线：`1d41bf8d5526698be4d1fd9a32c5aa1294b5eeeb`

### 修订记录

- 修订 2：按编码前 DDD 补齐主/备份状态、Windows 失败后置状态、乐观并发 revision、
  统一语义验证、schema 与未知字段保留、进程崩溃耐久边界、临时根隔离和满队列关闭契约。
- 修订 3：按第二轮编码前 DDD 固定 Win32 成功线性化与回执丢失语义、未来 schema
  禁止降级写入，以及 ReplaceFileW 全部文档错误和未知后置状态分类。
- 修订 4：按首次提交前 DDD 的 `REVISE_CODE` 结论，改为完整 DTO 驱动的通用已知字段
  合并，补齐 Win32 失败真实路径布局与主备份重验、关闭和 panic、完整数值矩阵、碰撞
  上限、错误脱敏及保存重启回归，并固定可复现 diff 哈希口径。

## 执行声明与依赖证明

- 原子状态：done。
- 分支：`codex/atom44-settings-core`。
- worktree：`F:\workspace\small-projects\windows-copy-worktrees\settings-core`。
- `base_main_sha`：`1d41bf8d5526698be4d1fd9a32c5aa1294b5eeeb`。
- 硬依赖：无新增原子依赖；ATOM-44 不消费 ATOM-40～43 的分页、视口或性能接口。
- 基线祖先证明：已执行
  `git merge-base --is-ancestor 29e1b5d 1d41bf8d5526698be4d1fd9a32c5aa1294b5eeeb`，
  退出码为 `0`；因此已完成并进入主线的 ATOM-39 提交是本基线祖先。
- 基线一致性：`git rev-parse HEAD` 精确等于 `base_main_sha`；计划创建前工作树为空。
- 远端约束：本分支只创建本地原子提交，不设置 upstream、不 push、不创建远端分支。

## 唯一目标

建立一个拥有配置文件 IO 的专用 `SettingsWorker`，使配置可从默认路径或测试注入路径
加载，以同目录 staging 文件和 Windows 原子替换协议可靠保存，并在主文件损坏时从备份
恢复。

本原子完成后只提供可复用的配置存储接缝。暂停记录、排除程序、设置窗口、快捷键切换、
开机启动和固定窗口均由后续原子实现。

## 当前行为与目标行为

### 当前行为

- 仓库没有 `settings` 模块、配置模型或配置工作线程。
- `Cargo.toml` 没有直接声明 `serde` 与 `serde_json`。
- 默认用户数据路径已有统一惯例：
  `%LOCALAPPDATA%\ClipboardBoard\<子目录>`；数据库与图片模块都提供显式路径入口供隔离测试。
- 现有 `StorageExecutor` 已证明“单一 worker 拥有 IO、调用方只持有有界命令入口、所有者
  负责关闭和 join”的线程模型可用。

### 目标行为

- 默认配置路径严格为
  `%LOCALAPPDATA%\ClipboardBoard\config\settings.json`。
- 配置目录缺失时由 worker 创建；缺失 `LOCALAPPDATA` 返回明确错误，禁止回退当前目录。
- 缺少主文件和备份时返回默认快照，不把“首次启动”视为损坏。
- 主文件合法时加载主文件；worker 同时记录主文件经过当前统一验证器验证为有效。
- 主文件缺失或损坏时尝试同目录 `settings.json.bak`；备份合法则返回备份快照，并报告
  `Backup` 加载来源，供后续启动接线决定是否提示。
- 主文件与备份都不可用时：
  - 两者都缺失则返回默认快照；
  - 任一文件存在但所有现存副本均损坏或不可读时返回明确恢复失败，不静默覆盖证据。
- 保存只在 `SettingsWorker` 内执行：先校验 `expected_revision` 和配置语义，再序列化到
  同目录唯一 staging 文件，`flush` 和
  `sync_all` 成功后才发布。
- 已有主文件时使用 Windows `ReplaceFileW` 原子替换，并把上一份主文件保存为
  `settings.json.bak`；但只有本次保存前主文件已由统一验证器确认有效时，才允许把它
  写成备份。主文件缺失或无效、当前快照来自 `Backup` 时，发布不得覆盖现有有效备份。
  首次保存使用同目录 `MoveFileExW` 的 write-through 移动。
- staging 发布提供“进程崩溃下旧版或新版原子可见”的能力；由于本原子没有可靠的目录
  元数据 flush 协议，突然掉电后的目录项耐久性不作保证。
- 发布失败时只承诺内存中的 `settings`、`source` 和 `revision` 不变。磁盘状态必须按
  Win32 错误分类并重新验证，不再笼统承诺主文件和备份字节完全不变。
- Win32 发布返回成功是配置事务唯一线性化点。worker 必须先把内存中的 settings、
  保留未知字段、`source=Primary` 和 `revision+1` 一次性更新，再尝试发送成功回执；
  回执发送失败不得回滚磁盘或内存事务。
- UI 线程不打开、读取、写入或替换配置文件。同步客户端接口必须注明可能等待 worker
  回执，后续 UI 接线不得直接从 Slint 回调调用它。

## 配置数据契约

本原子只定义当前清理与历史设置所需的稳定模型，不提前加入后续隐私和窗口行为：

```text
schema_version = 1
AppSettings
└── history: HistorySettings
    ├── max_items = 2_000
    ├── retention_days = 30
    ├── image_quota_mib = 500
    ├── capture_images = true
    └── capture_source_app = true
```

- `schema_version` 是持久化文档顶层整数，当前唯一支持值为 `1`；字段缺失视为兼容的
  version 1。值为 `0`、负数或分数属于 `Corrupt`；大于 `1` 属于独立的
  `UnsupportedFutureSchema`，不得降级成普通损坏。
- 已知 DTO 使用 `serde` 派生，并以 `#[serde(default)]` 保证已知缺失字段采用默认值。
- 默认值必须由 Rust `Default` 实现集中定义，测试不得复制另一套魔法数字。
- 数值 JSON 类型固定为无符号整数，不接受字符串、浮点数、负数或 `null`。统一语义验证器
  同时用于 load 和 save，合法范围固定为：
  - `max_items`：`1..=100_000`；
  - `retention_days`：`1..=3_650`；
  - `image_quota_mib`：`16..=10_240`。
- 不变量：上述三个限制必须同时合法；`capture_images=false` 时配额仍需合法，以保证
  用户重新启用图片记录时无需修复配置。
- 顶层和 `history` 对象中的未知字段必须在 worker 的内部原始文档中保留。保存时只替换
  `schema_version` 和已知字段，再把未知键原样写回；不得用普通 typed round-trip
  静默丢弃未来版本数据。更深层的未知值作为完整 `serde_json::Value` 原样保留。
- 若顶层或 `history` 不是 JSON object，或未知字段与已知字段发生同名冲突，以已知字段
  规则为准并拒绝非法已知值；不得让未知字段覆盖已知字段。
- 本原子不加入暂停状态、排除列表、窗口标题开关、快捷键、开机启动或窗口尺寸字段；
  对应原子在同一模型上做兼容扩展。
- `SettingsSnapshot` 包含进程内单调 `revision: u64`。首次加载 revision 为 `0`；每次
  保存必须携带 `expected_revision`，不匹配时返回明确 `RevisionConflict`，且不写磁盘。
- 保存成功后 revision 使用 `checked_add(1)`，返回包含新 revision 的完整快照，且
  `source` 固定变为 `Primary`；耗尽时在写文件前失败。
- 只有发布前失败或 Win32 发布返回失败时，内存中的 `settings`、`source`、`revision`
  和保留的未知字段保持不变。Win32 已成功但回执接收失败时，调用方得到
  `OutcomeUnknown`，不得假设失败或用旧 revision 重试；必须调用 `snapshot()` 对账。
- 主文件为 `UnsupportedFutureSchema` 时立即返回 `UnsupportedSchema`，不得读取旧备份
  伪装成可写 version 1，也不得启动普通可保存 worker；未来版本主文件字节必须保留。

## 工作线程与公共接口契约

计划在 `src/settings/` 下建立深模块，外部只通过 `src/settings/mod.rs` 使用：

- `AppSettings`、`HistorySettings`：可克隆、可比较的配置 DTO。
- `SettingsLoadSource`：`Primary`、`Backup`、`Defaults`，不携带文件正文。
- `SettingsSnapshot`：当前配置、加载来源与进程内单调 revision 的只读返回值。
- `SettingsError`：区分缺少本地数据根、目录/文件 IO、序列化、主备份均不可恢复、
  schema 不支持、语义非法、revision 冲突/耗尽、命令通道关闭、关闭状态和 worker panic；
  显示文本不得包含 JSON 正文。
- `SettingsWorker::start()`：解析默认路径、启动 worker、等待首次加载完成。
- `SettingsWorker::start_at(config_directory)`：仅供注入明确目录的应用接线和隔离测试，
  不读取或修改进程环境。
- `SettingsWorker::client()`：返回不拥有 join 权限的可克隆 `SettingsClient`。
- `SettingsClient::snapshot()`：从 worker 内存读取当前已提交快照，不触发磁盘读取。
- `SettingsClient::save(expected_revision, AppSettings)`：在 worker 上执行 compare-and-save，
  成功返回新 `SettingsSnapshot`；revision 冲突或持久化失败不改变当前快照。
- `SettingsWorker::begin_closing()` 与 `finish_shutdown()`：建立关闭线性化点并回收线程；
  `Drop` 只作尽力关闭，显式关闭结果仍由调用方负责处理。

命令队列必须有固定小容量；配置文件只能由该 worker 串行读取和写入。准入协议精确复用
`StorageExecutor`：共享 `closing_intent: AtomicBool`，再在同一生命周期互斥锁内复核
`Open` 并执行有界队列 `send`；入队后释放锁，等待业务回执时不得持锁。队列已满时，
已经取得锁的提交先完成入队，关闭线程看到 closing intent 后等待该已准入提交，再建立
`Closing`；之后所有克隆客户端稳定拒绝。客户端不得获得文件句柄、staging 路径、发送端
原始类型或 worker 线程句柄。

成功回执通道是事务结果通知，不是提交线性化点。命令已入队后若客户端/响应接收端消失，
worker 仍必须完成保存；发布成功后先提交内存状态，再发送回执。客户端收到回执通道断开
时返回 `OutcomeUnknown`，随后以 `snapshot()` 取得权威 revision；若错误地用旧 revision
再次保存，必须得到 `RevisionConflict`。

## 允许修改

- `docs/planning/原子计划-ATOM-44-配置持久化.md`
- `Cargo.toml`
- `Cargo.lock`
- `src/lib.rs`
- `src/settings/mod.rs`
- `src/settings/model.rs`
- `src/settings/persistence.rs`
- `src/settings/worker.rs`
- `src/settings/windows_replace.rs`（仅 Windows 原子发布适配）
- `tests/settings_storage.rs`（仅当公共接口集成测试比模块内测试更清晰时创建）

## 明确不修改

- `AGENTS.md`
- `原子开发任务计划.md`
- `docs/planning/开发计划.md`
- `docs/planning/并行开发执行计划.md`
- `docs/ai-project/项目工作台.md`
- `docs/ai-project/项目阶段记录.md`
- `src/main.rs`、Slint UI、托盘、热键和窗口代码
- 剪贴板读取、图片流水线、SQLite schema 与历史清理
- 默认 `%LOCALAPPDATA%\ClipboardBoard` 中的任何真实文件
- 真实剪贴板、托盘、注册表、单实例状态或正在运行的应用

若实现必须越出上述允许文件，先停止并重新评估原子边界，不得自行扩大范围。

## 实现步骤

1. 在 `Cargo.toml` 直接声明兼容当前工具链的 `serde`（derive）与 `serde_json`，更新锁文件。
2. 建立配置 DTO、schema version、集中默认值、统一语义验证器和无正文错误类型；在
   `src/lib.rs` 只公开深模块入口。
3. 建立默认配置目录解析纯函数和显式目录布局，固定主文件、备份和唯一 staging 命名；
   worker 内部保留顶层与 `history` 未知 JSON 字段。
4. 实现主文件、备份、默认值的确定性加载状态机；读取设定合理字节上限，避免损坏文件
   造成无界内存分配，并把副本分类为 `Missing`、`Valid`、`Corrupt` 或
   `UnsupportedFutureSchema`。主文件为未来 schema 时立即停止，只有 Missing/Corrupt
   才允许检查备份。
5. 实现 revision compare-and-save；保存前重新用同一验证器检查磁盘主文件，只有有效
   主文件可以轮换为备份，Backup 来源不得用损坏主文件覆盖有效备份。
6. 实现 staging 独占创建、JSON 写入、`flush`、`sync_all`、Windows 原子发布和 RAII
   staging 清理；Windows 适配层返回精确失败分类，测试替身可模拟每种后置状态。
7. 实现有界命令队列、首次加载握手、快照读取、保存回执、回执丢失对账、
   `closing_intent` 与生命周期锁内入队、关闭线性化及 join。
8. 增加缺失、损坏、未知字段、语义非法、revision 冲突、写入失败、备份恢复和满队列
   关闭测试；所有测试使用该 worktree 唯一临时根，不修改环境变量或默认用户目录。
9. 运行窄测试、格式化、Clippy 与中文注释检查；检查完整 diff 后进入提交前 DDD。

## 边界与异常

- 配置文件读取必须有固定上限；超限按损坏副本处理，不回显内容。
- UTF-8、JSON 类型或已知字段格式错误均使该副本 `Corrupt`；不得部分采用同一损坏副本。
- `UnsupportedFutureSchema` 优先于备份回退：主文件 schema 2 且备份 schema 1 时，
  `start_at` 必须返回 `UnsupportedSchema`，保留两份文件，且不存在可调用普通 save 的
  client。该规则防止旧版本把未来主配置静默降级并覆盖。
- load 与 save 必须调用同一个语义验证函数；未知字段不能导致失败，缺失已知字段使用
  默认值，非法已知字段不能通过 save 写入。
- staging 使用进程级实例 nonce、PID 与单调 token 命名，并以 `create_new` 独占创建；
  碰撞按固定上限重试，不能误删非本次调用拥有的文件。
- staging 创建/写入、`flush`、`sync_all` 或 Win32 发布返回失败时，不能把新快照标记为
  已提交；Win32 发布成功后的回执失败不得撤销已经提交的新快照。
- 首次发布和替换发布必须处于同一目录/同一卷，不提供跨目录降级复制。
- 备份恢复只加载、不在 load 路径自动修复主文件。Backup 来源后的第一次 save 必须先
  重新验证主文件；无效主文件以不指定 backup 路径的替换方式发布，现有有效 `.bak`
  保持为恢复点。成功后主文件状态变为已验证有效。
- Windows 发布适配层不得把 `ReplaceFileW` 的零返回值简化成“磁盘未改变”。适配层
  必须保留 Win32 原始错误码并返回以下互斥结果，后置状态固定为：
  - `ERROR_UNABLE_TO_REMOVE_REPLACED`（1175）：主文件和 staging 保留原名；
  - `ERROR_UNABLE_TO_MOVE_REPLACEMENT`（1176）：指定 backup 时主文件和 staging
    保留原名；未指定 backup 时主文件不存在、staging 保留原名；
  - `ERROR_UNABLE_TO_MOVE_REPLACEMENT_2`（1177）：staging 仍在原名且已继承主文件
    streams/attributes，旧主文件被移到另一名称；指定 backup 时该名称就是 `.bak`；
  - ReplaceFileW 文档覆盖的其他错误：主文件和 staging 保留原名、backup 不存在，
    但不保证 staging 是否继承主文件 streams/attributes；
  - `UnknownPostState`：仅用于适配层无法取得可信 Win32 错误码或发现结果不符合上述
    文档状态时；不得推断任何路径内容或身份，只能保留证据并重新验证。
- 上述任一发布失败后，worker 立即用统一验证器重新检查主文件和备份，刷新内部
  `PrimaryVerifiedValid/PrimaryUnverified` 磁盘判断，但不得更新对外内存快照。只删除
  经路径和独占所有权确认仍属于本次调用的 staging；1177 等无法证明身份时保留证据。
  下一次进程 load 按“主文件→备份→默认/恢复失败”状态机处理；同一进程下一次 save
  再次重新验证主文件，只有有效主文件可写入备份。
- 测试适配层必须能返回 1175、带 backup 的 1176、无 backup 的 1176、1177、文档其他
  错误和 `UnknownPostState`，并按上述确定后置状态改变隔离 fixture；断言错误后内存
  快照不变，且下一次 load/save 使用重新验证后的安全路径。
- 本原子的耐久声明只覆盖正常返回和进程崩溃时的原子可见性；不声明突然掉电后目录项
  一定持久，因为没有实现可证明的 Windows 目录元数据 flush 协议。
- worker 初始化失败必须 join 已启动线程；worker panic 时返回稳定错误，不把 panic
  伪装成通道关闭。
- 发布成功后即使 success reply receiver 已被丢弃，也必须提交新内存快照；worker 不得
  因 `send` 失败 panic 或撤销 revision。只有调用方观察为 `OutcomeUnknown`，事务本身
  已完成。
- 关闭准入复用 StorageExecutor 的 `closing_intent` 和生命周期锁内阻塞入队规则；满队列
  时已取得门禁的提交必须在关闭前完成准入，尚未取得门禁的提交观察 Closing 并被拒绝。
- 测试临时根命名必须包含 `clipboard-board-atom44-settings-core`、进程级实例 nonce、
  PID 和单调序号，并用 `create_dir` 独占创建、按固定上限处理碰撞；测试自行回收，不得
  调用默认 `start()`。

## 测试要求

1. **缺失配置**：唯一临时根内没有主文件和备份，`start_at` 返回全部默认值与
   `SettingsLoadSource::Defaults`。
2. **schema 与未知字段**：缺失 `schema_version` 按 version 1 加载；明确 version 1
   正常；0、负数、分数按 Corrupt 拒绝。主 JSON 含顶层、`history` 和未知字段内的嵌套
   值，保存已知字段后这些未知值逐值保持；缺失已知字段回落默认。
3. **未来 schema 禁止降级**：主文件为 schema 2、备份为合法 schema 1；启动返回
   `UnsupportedSchema`，不返回 client、不加载备份进入可写状态，两份文件字节不变。
4. **主文件损坏、备份有效**：主文件写入无效 JSON，备份写入合法配置；加载返回备份值
   与 `Backup` 来源，revision 为 0，worker 记录主文件未验证有效。
5. **主备份都损坏**：返回明确不可恢复错误；两个证据文件保持不变。
6. **保存与重启**：保存非默认合法快照，显式关闭 worker，再从同一临时根重启；加载值
   与保存值一致；保存返回 revision 递增且 source 为 `Primary`。
7. **备份轮换**：先成功保存 A，再保存 B；主文件为 B，备份为 A；重启加载 B。
8. **恢复后保存不破坏备份**：备份存放 A、主文件损坏，加载得到 A/Backup；保存 B 后
   损坏新主文件 B，再次启动仍从未被覆盖的备份恢复 A。
9. **统一语义验证**：对三个数值逐一覆盖边界最小、最大、0、超过最大、负数、分数、
   字符串和 `null`；非法 load 将副本视为损坏，非法 save 在创建 staging 前失败。
10. **双客户端 stale-write**：两个客户端取得相同 revision；第一个保存成功并获得
   revision+1/Primary，第二个用旧 revision 保存得到 `RevisionConflict`，磁盘和快照
   保持第一个结果；用新 revision 再保存才成功。
11. **成功发布但回执丢失**：丢弃保存响应接收端，允许 worker 完成 Win32 成功发布；
    随后 snapshot 必须为新 settings/未知字段/Primary/revision+1，用旧 revision 保存
    得到 `RevisionConflict`，证明响应通道不是线性化点。
12. **发布适配层失败结果**：分别模拟 1175、带 backup 的 1176、无 backup 的 1176、
    1177、文档其他错误和 `UnknownPostState`；fixture 严格形成各自确定后置状态。每次
    都断言 settings/source/revision 不变、磁盘有效性被重新判定，并验证下一次
    load/save 不会用无效主文件覆盖有效备份。
13. **普通写入失败**：在创建、写入、flush、`sync_all` 和发布前分别注入错误；内存
    快照不变，仅删除可证明由本次调用拥有的 staging。
14. **生命周期和满队列**：关闭线性化点后客户端保存和读取快照均被拒绝；
    `finish_shutdown` 回收线程；worker 被测试栅栏阻塞且队列填满时，已取得生命周期锁
    的额外提交先入队，closing intent 阻止后续提交，释放栅栏后无死锁地排空并关闭。
15. **revision 耗尽**：测试把 revision 置为 `u64::MAX`，保存必须在文件 IO 前失败。
16. **默认路径纯函数**：显式传入缺失或空 `LOCALAPPDATA` 返回明确错误；合法值只构造
   `ClipboardBoard\config`，不创建真实目录。
17. **隔离临时根**：并发创建的临时根均含实例 nonce/PID/token 且路径互异；预建碰撞
    触发有界重试，不访问默认 LOCALAPPDATA。
18. **中文注释**：新增 Rust 文件具有中文文件级说明；公共类型、字段、方法、错误、
    并发与原子替换分支均有符合项目规范的中文注释。

## 验证命令

编码完成后只运行与 ATOM-44 有关的针对性验证：

```powershell
$env:CARGO_TARGET_DIR = 'F:\workspace\small-projects\windows-copy-worktrees\settings-core\target-atom44'
cargo test settings
cargo test --test settings_storage
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/check_comments.ps1
```

若未创建 `tests/settings_storage.rs`，省略对应不存在的测试目标并在执行记录说明。不得为
本原子启动真实应用或执行全量人工 Windows 验收。

## 完成判定

- 缺失、损坏、未来 schema 禁止降级、未知字段保留、语义验证、revision 冲突、成功回执
  丢失、Windows 精确失败后置状态、满队列关闭和备份恢复测试全部通过。
- 所有文件 IO 仅发生在 `SettingsWorker`；公共 DTO 与客户端不泄漏文件所有权。
- 保存遵守“校验 expected revision 与语义、同步 staging、原子发布、成功后才更新并
  返回 revision+1/Primary 快照”的事务顺序。
- 默认值与根计划一致，且未提前实现 ATOM-45 以后行为。
- 针对性测试、`cargo fmt --check`、Clippy 和中文注释检查通过。
- 编码前与提交前两个独立 DDD 均为 `PASS`；每次复审前由主 Agent 现场按 raw stdout
  口径复算受审 diff 哈希，避免把哈希写回受审 diff 形成自引用。
- 创建一个包含 ATOM-44 的本地原子提交；没有 push 或 upstream。

## 交付给下一原子的输出

- ATOM-45 可在 `AppSettings` 上兼容增加持久化暂停策略。
- ATOM-47/48 可读取 `HistorySettings` 中的数量、天数与图片配额。
- ATOM-50 可通过后台桥调用 `SettingsClient`，但不得在 Slint 回调同步等待保存回执。

## 停止或重新规划条件

- Windows 原子替换无法在现有 `windows-sys` 特性范围内可靠实现。
- 现有 Rust/Windows 工具链与所需 serde 版本不兼容。
- 为完成目标必须修改 SQLite schema、UI、真实系统状态或共享状态文档。
- DDD 判定配置模型、备份恢复或关闭语义需要拆成多个原子。
- 发现同一 worktree 有不属于本原子的修改。

## 风险与 DDD 门禁

- 风险等级：L3。
- 风险理由：持久化写入具有不可逆副作用，涉及 Windows 原子文件替换、损坏恢复、
  并发关闭线性化和后续多个原子共享的公共配置接口。
- 编码前 DDD：主 Agent 已创建独立只读评审任务；必须审查模型边界、主备份状态机、
  原子替换、故障保持性、worker 生命周期和测试可证明性。
- 提交前 DDD：编码和针对性验证完成后，由不同的独立只读评审任务审查完整 diff、
  调用方、测试证据与 diff 内容哈希；结果必须为 `PASS`。
- 编码前 DDD 第一轮任务与结果：`ddd_atom44_precode`，`REVISE_PLAN`；九项发现已在
  修订 2 转化为明确行为与测试契约。
- 编码前 DDD 第二轮任务与结果：`ddd_atom44_precode`，`REVISE_PLAN`；三项发现已在
  修订 3 转化为明确行为与测试契约。
- 编码前 DDD 最终结果：`ddd_atom44_precode`，`PASS`；允许严格按修订 3 进入测试先行
  实现。
- 提交前 DDD 最终结果：`ddd_atom44_final`，`PASS`；主 Agent 已在复审前现场按 raw
  stdout 口径复算并确认受审 diff 一致，哈希不写入本计划，避免形成自引用。
- 提交前 DDD 首轮任务与结果：`ddd_atom44_final`，`REVISE_CODE`；已逐项修复完整 DTO
  合并、Win32 后置布局、缺失验收、阻塞接口注释、审查 target 清理和哈希口径问题，
  等待同一任务复审。

## 计划提交信息

`feat(settings): [ATOM-44] 建立可靠配置存储`

## 执行记录（提交前）

- 测试先行：首个 `settings_storage` 用例先因 `clipboard_board::settings` 不存在而 RED；
  建立最小 worker/default 快照后转为 GREEN，随后逐步加入持久化、恢复和并发用例。
- 针对性测试：首次实现时 `cargo test --lib settings` 通过（8 项）、`cargo test --test
  settings_storage` 通过（11 项）；首次提交前 DDD 修复后重新执行，分别通过 13 项与
  15 项。
- 格式检查：ATOM-44 文件使用
  `rustfmt --edition 2021 --config skip_children=true --check ...` 通过。
- Clippy：`cargo clippy --lib --bin clipboard-board --test settings_storage --all-features --
  -D warnings` 通过。
- 中文注释：项目现有 `scripts/check_comments.ps1` 通过；额外检查 6 个 ATOM-44 新文件首行
  中文文件级注释通过，并已人工复核公共类型、字段、方法和关键并发/持久化分支。
- 计划命令偏差：仓库基线的 `cargo fmt --check` 会要求格式化多个原子范围外既有文件；
  `cargo clippy --all-targets --all-features -- -D warnings` 会因既有 UI 测试夹具缺少新增
  `ClipboardCard`/`UiClipboardItem` 字段及既有 Clippy 告警失败。按文件所有权没有修改这些
  基线问题，改用上述 ATOM-44 窄检查证明当前差异。
- 提交前 DDD：`ddd_atom44_final` 经两轮 `REVISE_CODE` 修复和同一任务复审后最终
  `PASS`；允许创建本地原子提交。
- 首次提交前 DDD 修复：完整 typed `AppSettings` 先序列化为对象，再让所有当前已知
  顶层字段覆盖 raw，同名旧值不能胜出；`history` 继续递归保留未知键。测试接缝模拟
  后续新增顶层已知字段并验证序列化重启一致。
- Win32 失败测试 publisher 已按 1175、带/不带 backup 的 1176、1177、其他错误和未知
  状态真实改变主/备份/staging 路径；失败后显式重验主备份，逐类验证 staging 清理或
  保留、下一次 save 及 restart load 安全。
- 补充验收覆盖保存关闭重启、三个数值字段完整 JSON 类型/边界矩阵、关闭后的
  snapshot/save、worker panic 的 join 优先级、staging 与测试临时根有界碰撞以及
  SettingsError 的 Display/Debug 正文脱敏；公共同步接口已注明不得由 Slint 回调直调。
- 审查遗留 `target-atom44` 已先解析为
  `F:\workspace\small-projects\windows-copy-worktrees\settings-core\target-atom44`，确认
  精确名称且严格位于当前 worktree 后删除；后续构建继续使用仓库外目标目录。

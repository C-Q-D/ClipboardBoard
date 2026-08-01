# ATOM-46 排除程序原子计划

## 计划元数据

- 计划 ID：ATOM-46
- 类型：atomic-development
- 修订版本：4
- 状态：complete
- 父级 ID：WCB-PLAN-001 / UNIT-14
- 风险等级：L3（隐私边界、来源快照和读取前并发门禁）
- 创建基线：`821966e6c03deb11ce1d5e54b7f7e809114ae668`
- 计划提交：`feat(privacy): [ATOM-46] 支持排除指定程序`

## 执行声明与依赖证明

- 工作分支：`codex/atom46-excluded-apps`
- worktree：`F:\workspace\small-projects\windows-copy-worktrees\excluded-apps`
- `base_main_sha`：`821966e6c03deb11ce1d5e54b7f7e809114ae668`
- 硬依赖：ATOM-45 暂停记录运行时。
- 依赖提交：`821966e6c03deb11ce1d5e54b7f7e809114ae668`。
- 已执行 `git merge-base --is-ancestor 821966e6c03deb11ce1d5e54b7f7e809114ae668 821966e6c03deb11ce1d5e54b7f7e809114ae668`，退出码为 `0`；当前基线精确包含 ATOM-45。
- 远端约束：只创建本地原子提交，不设置 upstream、不 push、不创建远端分支。

## 唯一目标

用户配置的排除程序复制事件不得进入 ClipboardIO 的正文读取、捕获结果或历史；排除判断必须在已有 ATOM-45 `RecordingGate` 读取许可和 backend/factory 之前完成。规则只依据来源程序快照，不读取窗口标题、进程内存、剪贴板正文或配置之外的数据。

本原子不实现设置页面、托盘菜单、历史清理、图片格式解析、云同步或新的日志正文；设置页面后续只需要复用本原子提供的持久化字段和规则快照接缝。

## 现有接缝与边界

### 配置与快照

- `AppSettings.privacy` 已由 ATOM-45 提供，新增 `excluded_apps` 作为隐私配置的稳定字段。
- 配置使用 ATOM-44 的 `SettingsClient`/`SettingsSnapshot` 和递归未知字段保留协议；缺少该字段时默认空规则，旧配置无需迁移。
- `AppSettings`、`PrivacySettings` 和 `SettingsSnapshot` 移除会递归输出排除数组/路径的派生 Debug，改为只输出数值/模式、规则计数、来源和 revision；JSON 序列化仍保持完整字段用于持久化，诊断不得直接格式化 JSON。
- ClipboardIO 启动时只取得一次拥有型排除规则快照；本原子不让消息线程同步读取配置文件，也不在 UI 回调中执行设置 IO。
- 规则快照应在构造 `RecordingGate` 时注入，并与暂停状态共用同一个读取前门禁；后续设置原子可以通过现有 gate 更新接缝替换快照，不复制第二套正文门禁。

### 来源与事件路由

- `WM_CLIPBOARDUPDATE` 仍在消息线程捕获 sequence 和 `ProcessSource`，随后把不可变来源快照随 `ClipboardCaptureRequest` 投递给 ClipboardIO worker。
- 当前 `ProcessSource` 继续只保留 exe 文件名、显示名和 PID，避免完整路径进入历史结果；为完整路径规则新增仅供 ClipboardIO 请求使用的 `ProcessSourceSnapshot`，其中路径是受限查询得到的拥有型映像路径，结果桥和历史 DTO 不携带该字段。生产来源查询不得增加窗口标题或正文读取。`ClipboardCaptureRequest`/`ReadResponse` 只携带请求级快照；成功 `ClipboardCaptureResult` 仍只带脱敏 `ProcessSource`。
- worker 只能使用请求携带的来源快照做排除判断；不能在 worker 中重新查询前台窗口，否则来源会与复制事件错配。
- 来源快照为 `None` 时不命中排除规则，事件按原有策略继续交给暂停门禁；无法安全转换的非 UTF-8 映像文件名则保留有界 basename 标记并在正文门禁前 fail-closed，不把未知来源错误地当作某个被排除程序。
- `history_bridge::run_clipboard_pump_with_source_policy` 接收启动时 `capture_source_app` 快照；为 `false` 时在文本和图片共用的分派点把 `ClipboardCaptureResult.source` 映射为 `None`，因此 `source_exe/source_app` 都不会写入 SQLite/UI。该静态脱敏不影响 worker 在正文读取前使用请求级 `ProcessSourceSnapshot` 做排除匹配；运行中切换留给后续设置原子。

## 排除规则模型与规范化

- 持久化字段为 `privacy.excluded_apps`，以字符串数组表示，每项是 exe 文件名或完整 Windows 路径；默认 `[]`。模型上限固定为最多 64 项、单项 UTF-8 字节数不超过 512、不得含 NUL、去除首尾空白后不得为空；违反任一项使该设置副本进入既有 `Corrupt`/备份恢复流程，并以 `privacy.excluded_apps` 作为稳定错误字段。
- 规则和来源均按 Windows `CompareStringOrdinal(..., ignore_case=true)` 不区分大小写匹配；正斜杠统一为反斜杠，去除首尾空白。非 Windows 单元测试使用明确标注的 Unicode fallback，不把 ASCII-only lower 当作产品契约。
- 不含目录分隔符的规则只与来源映像的最终文件名精确匹配（例如 `KeePass.exe`），禁止子串命中。
- 含盘符、UNC 前缀或目录分隔符的规则按规范化完整路径精确匹配；不进行文件系统访问、符号链接解析或 `canonicalize`，避免配置匹配产生外部副作用。
- 规则路径只接受两类形式：无分隔符且无冒号的 basename，或绝对 DOS `X:\...`/UNC `\\server\share\...`；拒绝 `C:foo`、`./`、`../`、根相对 `\foo`、越根 `C:\..`、内部重复分隔符、空 server/share 和 `\\?\` 扩展前缀。正斜杠可作为输入分隔符并在验证后统一为反斜杠；`.`/`..` 段一律拒绝，不做文件系统访问或隐式消解。完整路径匹配使用规范化后精确相等，目录前缀、相似文件名和不同目录均不命中。
- 来源映像路径按同一绝对 DOS/UNC 规范化；系统路径若无法安全转换为 UTF-8 或违反边界则仅保留 basename，完整路径规则不命中。
- 规则快照构造时按规范化值去重并保留首次出现顺序；模型验证和快照构造都执行长度/NUL/空值边界，加载损坏副本走既有主/备份/默认恢复，不部分采用。
- 规则快照去重并保持首次出现顺序；更新或匹配过程不携带正文。`ExcludedAppsSnapshot` 的 Debug 只输出规则数量，不输出规则内容或路径。

## 读取前门禁契约

1. worker 收到请求后，在同一 `RecordingGate` 锁内通过 `try_read_for_snapshot(Option<&ProcessSourceSnapshot>)` 先检查 `update_pending`/暂停状态，再使用请求中的来源快照对排除快照做纯内存匹配；因此“暂停优先返回 `Paused`”与排除判断顺序一致。旧 `try_read_for_source(Option<&ProcessSource>, Option<&str>)` 仅作为兼容包装接缝，内部构造来源快照后转调新接口。
2. 命中排除规则时立即返回不含正文的 `ClipboardReadError::ExcludedApp`，不得构造 Win32 backend、调用 reader/factory、生成摘要或发布 inbox。
3. 未命中时同一调用原子增加 `active_readers` 并返回 ATOM-45 `RecordingReadPermit`；排除判断不绕过已有 `active_readers` 线性化。
4. 只有同时通过暂停、规则和来源快照判断，才允许进入 `factory/backend -> read -> capture result -> inbox publish` 链路；RAII permit 生命周期保持不变。
5. 排除判断、错误、Debug/Display 和诊断日志只允许包含规则命中状态、类型、来源 exe 摘要或计数，不得包含剪贴板正文、HTML、图片字节、窗口标题、完整路径、规则内容或完整配置 JSON。`ProcessSourceSnapshot`、`ClipboardCaptureRequest`、含隐私设置的 `AppSettings`/`PrivacySettings`/`SettingsSnapshot` 均使用自定义脱敏 Debug，只输出存在性、计数、类型和 revision。
6. 被排除事件不会写历史；恢复或修改规则后不主动重读当前剪贴板，只有下一次真实 `WM_CLIPBOARDUPDATE` 才捕获。

## 暂停与排除组合顺序

- 暂停门禁和排除门禁在同一个 `RecordingGate.try_read_for_snapshot` 读取前操作中组合，均为 fail-closed 的读取拒绝；不得在 backend 构造后再过滤。
- 暂停状态优先返回 `Paused`，活动状态命中规则才返回 `ExcludedApp`；两种错误都不含正文且不发布结果。
- `RecordingGate::new_with_excluded_apps` 用于初始配置；`GateUpdate::finish_with_excluded_apps` 和 `RecordingGate::replace_excluded_apps` 是唯一替换接缝，在同一 mutex 内同时提交模式和不可变规则快照。更新前的 `begin_update` 阻断新 reader 并等待已取得许可的 reader，规则构造失败时不提交更新，旧快照和门禁状态保持不变。暂停 controller 的既有 `finish(mode)` 保留旧规则；后续设置原子必须先 CAS 保存成功再调用替换接缝。
- 关闭阶段沿用 ATOM-45 的唯一生命周期链；排除规则不新增独立 worker、线程或关闭顺序。
- 关闭线性化时，队列中尚未取出的请求连同其请求级来源路径一起丢弃；已取出的请求必须先完成同一 gate 的判断/permit 生命周期，worker join 后才允许 inbox close。关闭之后不得发布包含来源快照的结果；该顺序与 ATOM-45 的 `ClipboardCaptureInbox::close` 契约保持一致。

## 允许修改

- `docs/planning/原子计划-ATOM-46-排除程序.md`
- `src/settings/model.rs`
- `src/settings/persistence.rs`
- `src/settings/mod.rs`
- `src/privacy/mod.rs`
- `src/privacy/pause.rs`
- `src/privacy/controller.rs`
- `src/clipboard/io_worker.rs`
- `src/clipboard/reader.rs`
- `src/clipboard/mod.rs`
- `src/platform/windows/source.rs`
- `src/platform/windows/system_window.rs`
- `src/main.rs`（仅配置快照注入和现有生命周期接线）
- `src/history_bridge.rs`（仅静态来源字段脱敏接线）
- `Cargo.toml`（仅启用 `Win32_Globalization` 以调用 `CompareStringOrdinal`）
- `tests/privacy_excluded_apps.rs`
- 上述模块内与排除规则直接相关的窄测试

## 明确禁止修改

- `AGENTS.md`、`原子开发任务计划.md`、`docs/planning/并行开发执行计划.md`、项目状态文档等共享文档。
- Slint 页面、卡片、搜索、分页、收藏、删除、清空和主题。
- SQLite schema、历史泵、图片写盘和图片解码。
- 托盘菜单、全局快捷键和自动粘贴行为。
- 真实剪贴板、真实托盘、真实应用、默认 `%LOCALAPPDATA%` 文件、注册表和网络。
- 远端分支、push、upstream 或 PR。

## 实现步骤

1. 先在本计划记录编码前 DDD 结论，确认规则边界、来源脱敏、读取许可顺序、配置未知字段保留和关闭协议可证。
2. 为规则规范化/精确匹配、大小写和路径边界、空/超限配置建立窄 RED 测试。
3. 扩展 `PrivacySettings` 与校验/递归持久化，构造不可变 `ExcludedAppsSnapshot`；证明旧配置缺少字段仍可加载。
4. 增加不含完整路径的历史 `ProcessSource` 和仅请求级携带路径的 `ProcessSourceSnapshot`，把事件来源以拥有型快照传入已有 ClipboardIO 请求路径；在同一 `RecordingGate` 的读取许可和 backend/factory 之前按“暂停优先、排除次之”执行判断，不复制第二套正文门禁。
5. 为文本、图片、暂停、排除、无来源、前台变化和迟到事件建立同一路径 fake factory 测试，断言正文读取次数、inbox 发布次数、错误摘要和路径不会进入结果/日志。
6. 将 `HistorySettings.capture_source_app` 作为启动时静态快照传入结果泵；`false` 时文本和图片的 `source_exe/source_app` 均被清空，排除判断仍使用请求级最小来源快照。运行中切换留给后续设置原子，本原子不声称动态热更新。
7. 运行 ATOM-46 窄测试、目标格式/Clippy/中文注释/diff 检查，完成提交前 DDD；复核只改允许文件后创建本地提交。

## 验收与定向验证

所有测试使用显式临时目录、假来源、假时钟和 fake reader/factory；不得调用真实剪贴板或默认配置路径。

1. 空排除配置与旧 ATOM-44/45 配置兼容；保存后顶层、history、privacy 未知字段逐值保留。
2. exe 文件名匹配不区分大小写、只做完整文件名精确匹配；同名子串不命中。
3. 完整路径匹配规范化分隔符、盘符和大小写；不同目录、`..` 或相似前缀不误命中。
4. 空规则、NUL、超长项、超多项被拒绝；规则去重保持首次顺序。
5. 被排除来源的文本请求：factory/read 次数均为 0，inbox 不发布，返回 `ExcludedApp`。
6. 被排除来源的图片请求：同样不得构造图片 backend 或读取字节。
7. 同一路径下暂停优先仍返回 `Paused`；更新中和关闭不允许迟到排除事件进入正文。
8. 未知来源不命中排除规则；普通来源仍可读取并发布，证明未误伤既有行为。
9. 排除判断只使用事件携带来源快照；前台窗口随后变化不会改变该事件结果。
10. 错误、Debug/Display、诊断和配置快照不含剪贴板正文、图片字节、窗口标题、完整路径、规则内容或完整 JSON；Request/各快照 Debug 只输出脱敏摘要。
11. 规则快照替换与 reader 并发时遵守 `update_pending` 屏障，已在途 reader 完成后新请求才使用新规则；暂停更新沿用旧规则且关闭顺序无变化。
12. `capture_source_app=false` 是启动时静态策略，文本/图片历史的 `source_exe/source_app` 均为 `None`，但读取前仍使用最小 exe/path 请求快照；无来源请求按 fail-open 对照验证。
13. Windows 大小写使用 `CompareStringOrdinal`，非 ASCII 文件名/目录测试覆盖等价与不等价样例。
14. 规则更新与 active reader、关闭前已取请求/关闭后发布、迟到事件均有线性化测试；关闭不泄露 source snapshot。
15. 只执行目标模块测试、目标 `cargo check`/Clippy、目标 Rust `rustfmt --check`、中文注释检查和 `git diff --check`，不运行全量测试。

## DDD 门禁

- 编码前 DDD：首轮结果 `REVISE_PLAN`；第二轮结果 `REVISE_PLAN`；第三轮（v3 只读复核）结果 `PASS`。评审确认静态 capture_source_app 文本/图片共用脱敏、basename/绝对 DOS/UNC 与相对/越根/重复/`\\?\` 边界、`CompareStringOrdinal`、所有设置/来源/请求/规则快照 Debug 脱敏、Gate 初始/替换/暂停保留规则/CAS 后替换和关闭/迟到/并发测试契约均已闭合。
- 提交前 DDD：已完成并通过 PASS；复核确认排除判断位于 factory/backend 之前、规则不泄露正文、配置未知字段未丢失、暂停/关闭生命周期未改变。
- 任一评审要求修改计划或代码时，先在本 worktree 修复并重新复核；未 PASS 不提交。

### 实现与定向验证记录

- 已实现配置字段、严格规则规范化、请求级 `ProcessSourceSnapshot`、`CompareStringOrdinal` 匹配、暂停优先的 `RecordingGate` 读取前门禁、规则替换接缝、ClipboardIO 文本/图片共用脱敏桥和启动静态 `capture_source_app` 策略。
- 已增加 `tests/privacy_excluded_apps.rs` 六项边界测试，并补充历史来源脱敏、来源快照和读取前 factory 门禁测试；未调用真实剪贴板、托盘、默认配置路径或网络。
- 定向测试通过：`cargo test -j1 --lib privacy -- --test-threads=1`（11）、`cargo test -j1 --test privacy_excluded_apps -- --test-threads=1`（6）、`cargo test -j1 --lib "privacy::pause" -- --test-threads=1`（3）、`cargo test -j1 --lib "clipboard::io_worker" -- --test-threads=1`（18）、`cargo test -j1 --lib history_bridge -- --test-threads=1`（19）、`cargo test -j1 --lib settings -- --test-threads=1`（16）、`cargo test -j1 --test settings_storage -- --test-threads=1`（15）、`cargo test -j1 --lib "platform::windows::source" -- --test-threads=1`（7）。
- `cargo check -j1 --lib --bin clipboard-board`、目标 `rustfmt --check`、`cargo clippy -j1 --lib --bin clipboard-board -- -D warnings` 和 `git diff --check` 通过；未执行全量测试。首次并行检查曾因机器资源不足 OOM，随后使用单并发重跑通过。
- 提交前 DDD：首轮发现并修复了并发更新令牌未串行化、快照构造未限制 64 条和非 UTF-8 basename 可能绕过门禁三项问题；修复后重新复核 PASS。最终确认排除判断位于 factory/backend 之前、暂停优先、规则仅使用来源快照、完整路径不进入结果/UI、文本与图片共用来源脱敏、未知配置字段和 ATOM-45 关闭顺序保持不变。

## 回滚与完成判定

- 回滚仅移除 ATOM-46 的 `excluded_apps` 字段、规则快照和读取前判断，保留 ATOM-45 暂停门禁及旧配置未知字段。
- 规则更新失败保留旧快照；排除命中只丢弃当前事件，不清理既有历史。
- 完成条件：规则/配置/事件路由/文本图片门禁测试通过，窄检查通过，双 DDD PASS，只包含允许文件的本地提交已创建，不 push。

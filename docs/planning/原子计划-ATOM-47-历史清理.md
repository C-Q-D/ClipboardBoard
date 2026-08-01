# ATOM-47 历史数量与保留天数清理计划

> 契约修订：编码前 L3 DDD 首轮判定 `NEEDS_REVISION`；修订 2 经本隔离复核后 `PASS`。
> 本次修订补齐 upsert 与清理的原子事务、零删除修订号、策略校验/失败安装、首屏自定义
> 策略接缝和关闭生命周期边界；仅修改本计划文档，不改变生产代码。

## 原子元数据

- 原子编号：`ATOM-47`
- 所属交付单元：`UNIT-15`
- 工作分支：`codex/atom47-history-cleanup`
- Worktree：`F:\workspace\small-projects\windows-copy-worktrees\history-cleanup`
- `base_main_sha`：`821966e`
- 硬依赖：`ATOM-44`，其提交 `821966e` 已包含在当前基线祖先中。
- 计划提交：`feat(cleanup): 清理过量和过期历史`
- 风险等级：`L3`（批量删除、线程内事务、配置触发语义）。

## 目标与边界

本原子只负责普通文本历史按数量和保留天数自动收敛，保持当前 SQLite 单线程所有权和事务边界。

### 必须实现

1. 在存储线程启动并完成迁移后，使用已校验的初始策略清理一次，并在向调用方报告
   `ready` 之前完成该事务；`open_at` 使用默认策略，另提供供 ATOM-50 注入已加载设置的
   `open_at_with_policy`（或等价公开接缝）。首屏历史查询不得早于该清理完成。
2. 每次文本 upsert 都必须在**同一个 SQLite 事务**中执行 upsert、年龄清理和数量清理；
   upsert 与清理共享**一个** `mutation_revision`，不能先提交 upsert 再发另一条清理命令。
   图片 upsert 不触发本原子的文本清理。
3. 暴露显式策略更新命令，供后续设置保存流程在配置变更后立即触发清理；该命令在
   worker 内串行化，并返回本次清理删除数、有效策略和单调修订号。
4. 删除最旧的未收藏文本；数量淘汰严格按 `copied_at ASC, id ASC`，同一时间戳使用
   较小 ID 优先且结果可复现。
5. 收藏文本永不被自动清理；达到上限的收藏数量不强行删除收藏项。图片行（无论收藏与否）
   均不属于本原子的候选集，必须保留，图片配额由后续原子负责。
6. 年龄清理使用严格条件 `copied_at < cutoff`；先完成年龄门槛，再对剩余未收藏文本执行
   数量门槛，最终最多保留 `max_items` 条未收藏文本。任一步 SQL、时间换算或 revision
   预留失败，整个事务回滚，数据库、策略和已安装 revision 均不改变。
7. 即使成功事务删除 0 行，也必须安装并返回新的单调 `mutation_revision`（启动清理、
   upsert 后清理、策略更新清理均适用），不得因为“幂等”跳过线性化修订。
8. 返回结果只包含删除数量、有效策略、稳定身份和单调存储修订号，不返回剪贴板正文、
   原始字节或包含正文的诊断信息。

### 明确不包含

- 图片历史记录的数量/磁盘配额清理及图片资产文件回收；由 `ATOM-48`、`ATOM-49` 负责。
- SQLite schema、迁移文件和索引变更。
- SettingsWorker 或设置 UI 的接线；后续 `ATOM-50` 通过本原子暴露的策略更新 API 接入。
- 主窗口、托盘、剪贴板监听和真实 Windows 全局状态测试。

## 候选接口与不变量

### 策略

`HistoryCleanupPolicy` 只携带普通历史上限和保留天数，默认值与配置模型一致：

- `max_items = 2_000`
- `retention_days = 30`

策略字段使用无符号整数并在进入 SQL 前执行**包含端点**的验证：

- `1 <= max_items <= 100_000`；
- `1 <= retention_days <= 3_650`。

零值、超上限值以及无法表示的输入都返回不含正文的 `InvalidCleanupPolicy`，不得进入
worker SQL。策略更新必须先以候选策略计算并完成清理事务，事务成功后才安装新策略；
策略校验、时间换算、SQL、事务提交或 revision 预留失败时，旧策略、数据库和 revision
全部保持不变。不能先安装策略再清理，也不能在失败后把候选策略留在内存中。

公开结果契约必须保持单一线性化点：

- 为保持既有 `history_bridge`/命令调用方的结构体兼容，`TextUpsertResult` 字段不变；新增
  `TextUpsertCleanupResult { result: TextUpsertResult, deleted_count }`（或等价受限 DTO）。
  worker 内部回执必须使用该合并结果并只携带一个 `mutation_revision`；既有 `upsert_text`
  适配器可映射回 `TextUpsertResult`，但不得把 upsert 和清理拆成两个事务或两个 revision。
- 策略更新返回 `CleanupPolicyResult { policy, deleted_count, mutation_revision }`；
  `policy` 是事务成功后实际安装的候选策略。
- 内部命令显式携带清理参考时钟 `now_millis`，不得使用 `TextUpsertInput::copied_at`
  代替当前时间；生产入口取 Unix 毫秒，测试入口注入固定值。

### 清理事务

清理候选严格满足：

```sql
item_type = 'text' AND is_pinned = 0
```

事务顺序（upsert、启动清理和策略更新都遵守；upsert 只是多出第一步）：

1. 在任何 SQL 前使用 `checked_add` 预留一个 revision；耗尽时立即失败。
2. （仅文本 upsert）写入或更新文本行；候选字段不改变收藏状态和既有稳定身份。
3. 使用 `checked_mul(retention_days, 86_400_000)` 与 `checked_sub(now_millis, age_millis)`
   计算 cutoff；任一算术溢出在开启事务写入前失败，不能使用饱和算术或放宽边界。
4. 删除严格满足 `item_type = 'text' AND is_pinned = 0 AND copied_at < cutoff` 的行。
   `copied_at = cutoff` 必须保留（除非随后因数量门槛被淘汰）。
5. 统计年龄删除后的剩余未收藏文本；若数量超过 `max_items`，仅按
   `ORDER BY copied_at ASC, id ASC` 删除恰好 `count - max_items` 行。不得用不稳定的
   `OFFSET` 或没有第二排序键的 `ORDER BY`。
6. 提交事务后同时安装预留 revision；策略更新还要在此时安装候选策略。upsert 结果和
   清理结果共享同一个 revision。任一步骤失败均回滚且修订号不得前移。

`deleted_count` 必须是本事务年龄删除数与数量删除数之和；0 也视为成功事务并安装 revision。

`now_millis` 由命令携带，以便单元测试注入固定时间；生产入口使用当前 Unix 毫秒时间。时间换算必须使用 checked arithmetic，不能因异常值溢出。

### 触发顺序

- 启动清理必须发生在存储线程对外报告 ready 之前，避免启动快照读取到未收敛数据；
  启动零删除也安装 revision，后续第一条成功 mutation 的 revision 必须继续递增。
- 文本 upsert 与清理是同一 worker 命令、同一 SQLite 事务和同一 revision；调用方收到成功
  回执时，文本写入和两个清理门槛都已提交。图片 upsert 不触发本原子文本清理。
- 策略更新命令先校验候选策略，再用候选策略完成清理事务；只有提交成功后才安装策略并
  回执。调用方看到成功回执时清理已完成，失败回执时旧策略仍然有效。
- 启动失败、策略更新失败和任意 SQL/算术/revision 错误都不得向 ready 或调用方暴露半成功
  快照；失败的事务必须由 SQLite 回滚，worker 仍由唯一 `StorageExecutor` 所有者收口。

### ATOM-50 首屏和运行时接缝

- `HistoryCleanupPolicy`、默认值、边界验证器和 `StorageExecutor::open_at_with_policy`（或
  等价 API）是本原子唯一公开接缝；ATOM-50 读取已成功加载的设置后，在创建首屏查询入口
  前传入自定义策略，不能直接访问 `StorageState` 或另开 SQLite 连接。
- `open_at` 保留默认策略兼容性；自定义策略启动失败时返回明确错误且不报告 ready，不能
  先以默认策略显示首屏再异步替换为自定义策略。
- 为保持既有存储单元测试中使用的小整数历史时间夹具和 revision 断言，`cfg(test)` 下的
  `open_at` 保留旧的无启动清理夹具入口；启动清理语义由新增的
  `open_at_with_policy` 定向覆盖。生产构建的 `open_at` 始终使用默认策略并在 ready 前清理，
  该测试兼容分支不改变生产行为，也不允许业务代码依赖它。
- 运行时设置保存调用显式策略更新命令；设置保存成功与清理成功的先后由调用方按回执处理，
  但任何失败都保留旧策略和旧历史，禁止把策略保存和清理失败混成“已生效”。ATOM-50
  不得在 storage 更新失败时向用户报告设置已生效；若持久化设置已经先写入而运行时更新
  失败，必须按 ATOM-50 的 CAS/补偿协议恢复旧设置或进入明确的对账状态，不能静默留下
  “磁盘新策略、内存旧策略”的首屏状态。

## 允许修改的文件

- `src/storage/worker.rs`：策略 DTO、校验器、命令、worker 触发和同事务实现、兼容适配器、
  针对性单元测试。
- `src/storage/mod.rs`：只增加对外导出或错误边界所必需的最小定义。
- `docs/planning/原子计划-ATOM-47-历史清理.md`：本原子契约、DDD 证据和验证回写。

禁止修改共享计划、项目状态、设置模块、图片模块和主线接线文件。

## 针对性验收

1. 默认策略删除超过 30 天的未收藏文本，保留边界等于 cutoff 的记录；用一个不触发数量
   超限的夹具证明严格 `<`，并单独用双门槛夹具证明第二阶段可淘汰等于 cutoff 的行。
2. 文本数量超过上限时只删除最旧未收藏文本，排序在相同时间戳下按较小 ID 优先；断言
   最终未收藏文本不超过上限且收藏文本不计入上限。
3. 收藏文本永不被删除；至少插入一条收藏图片和一条未收藏图片，断言两者都不受本原子
   清理影响，避免把图片策略误并入文本 SQL。
4. 启动清理发生在 `ready` 之前；预置过期行后打开执行器，第一次 `status/list` 只能看到
   已收敛数据；零删除启动也使后续 mutation revision 从下一值开始。
5. 文本 upsert、年龄清理和数量清理在同一事务；`upsert_text_with_cleanup_at` 的合并回执
   只有一个 revision 和总删除数，既有 `upsert_text` 只映射快照且仍共享该事务；图片 upsert
   不会误触发文本清理。
6. 策略边界 `1/100000`、`1/3650` 合法，0 和超上限非法；策略更新在成功后立即清理，
   重复调用是幂等的但仍返回新的单调 revision。
7. 策略校验失败、SQL 中途故障、revision 耗尽和 cutoff 乘法/减法溢出均不改变数据库、
   策略或 revision；使用测试专用故障注入验证 upsert 与年龄删除一起回滚。
8. 关闭线性化点建立后拒绝新清理/upsert；已入队的同事务命令完成后才处理 Shutdown，
   worker 只由 `StorageExecutor` join 一次，Drop 与显式关闭不重复执行清理。
9. ATOM-50 可通过公开初始策略/运行时更新 API 在首屏查询前安装自定义策略；生产执行路径
   不得绕过唯一 worker 创建第二个可写 SQLite 连接。测试可在 worker 已关闭后使用临时连接
   建立夹具或只读断言，但不得并发写库、从结果/日志读取或打印正文。
10. 所有清理结果、错误、Debug 和日志断言均不包含剪贴板正文或原始字节。

验证命令使用独立目标目录 `F:\workspace\small-projects\windows-copy-worktrees-targets\history-cleanup-atom47`，只运行存储模块相关测试、`cargo check`、`cargo clippy -- -D warnings` 和 `cargo fmt --check`；禁止启动真实应用。

## DDD 门禁记录

- 编码前 DDD：首轮隔离 L3 反证发现 upsert 与清理原本被契约拆成两个事务、策略更新
  先安装后清理、范围边界和首屏自定义策略接缝不充分；修订 2 已补齐同事务/单 revision、
  零删除 revision、policy 边界与 checked cutoff、失败不安装候选、图片保护、关闭和
  ATOM-50 初始策略接缝，二轮反证未发现新的实质缺口，结论 `PASS`。
- 提交前 DDD：新的隔离评审上下文已按最终 diff 复核，结论 `PASS`；未发现违反本契约的
  实质问题。复核覆盖启动 ready 前清理、同事务文本 upsert/年龄/数量清理、候选策略提交
  后安装、严格 cutoff、稳定淘汰、图片保护、revision 预留/失败回滚、关闭生命周期和
  `cfg(test)` 兼容接缝。
- 实现回写：`src/storage/worker.rs` 增加默认/自定义策略、启动和运行时清理、合并 upsert
  回执、checked cutoff、候选策略更新及测试故障注入；`src/storage/mod.rs` 仅增加 DTO 导出
  和错误边界。既有 `TextUpsertResult` 和调用方适配保持不变。
- 定向验证：`cargo test --lib storage::worker::tests:: --no-fail-fast -- --test-threads=1`
  通过 57/57；新增清理、策略失败、revision 耗尽测试均通过；`cargo check --lib --bin
  clipboard-board`、`cargo clippy --lib --bin clipboard-board -- -D warnings` 和
  `git diff --check` 均通过。`rustfmt --check` 仍报告基线中未由本原子修改的既有格式差异，
  已恢复这些无关格式改动以保持 diff 最小。
- 提交前代码差异哈希（仅 `src/storage/worker.rs` 与 `src/storage/mod.rs`，不含本段文档
  回写）：`a1ef3b22a4b26ee97b402d93970ce68226be6376`；本地提交后状态和提交哈希由执行记录
  补写。

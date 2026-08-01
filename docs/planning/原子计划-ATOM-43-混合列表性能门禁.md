# ClipboardBoard ATOM-43 混合列表硬性能门禁原子计划

## 计划元数据

- 计划 ID：ATOM-43
- 类型：atomic-development
- 修订版本：3
- 状态：[!] blocked（硬性能门禁失败，按停止条件等待重新规划）
- 所属交付单元：UNIT-12
- 风险等级：L2
- 创建基线：`821966e`（包含 ATOM-42 与 ATOM-45 主线集成结果）
- 依赖原子：ATOM-42；依赖祖先证明由主 Agent 在集成时复核。
- 本地分支：`codex/atom43-perf-gate`
- worktree：`F:\workspace\small-projects\windows-copy-worktrees\perf-gate`
- 远端约束：本分支不设置 upstream、不 push、不创建远端分支。

## 唯一目标

在最终的混合卡片、Slint ListView 窗口化模型和 ATOM-42 缩略图生命周期语义上，建立
可重复的 Windows Release 性能硬门禁。门禁只测性能，不通过放宽阈值掩盖模型或图片
驻留退化。

## 输入场景与边界

1. 生成恰好 20,000 条摘要：10,000 条文本、10,000 条图片，交错排列以覆盖两种固定
   卡片高度（文本 106px、图片 186px）。
2. 每条图片摘要携带独立拥有的代表性 16×16 RGBA 缩略图 `slint::Image`，并逐项断言
   `to_rgba8`、尺寸和像素长度；不把“有图片字段”误写成“已验证 active set”。
3. 通过最终 `AppWindow` 和真实 `ListView` 模型绑定测量完整 20,000 条模型的呼出与
   首批：首批耗时必须覆盖 `set_cards`、`show` 和 testing backend 更新，并断言模型长度
   仍为 20,000，不能另绑 30 条模型冒充首批。
4. 在同一完整模型上执行 200 次连续视口循环；每次先清空 `row_data` 日志，再只设置
   视口（禁止重绑模型和固定行高公式），要求每个样本访问 1～100 行且首尾样本命中。
5. ATOM-42 的 active set、500 项 LRU、范围外释放和迟到结果身份校验继续由其已有
   UI reducer 定向测试证明；本原子不复制缓存实现，也不修改生产 LRU。

## 硬门禁

| 指标 | 阈值/要求 | 证据字段 |
|---|---:|---|
| 摘要数量 | 文本 10,000、图片 10,000、合计 20,000 | `text_summary_count`、`image_summary_count`、`item_count` |
| 呼出 P95 | ≤ 100ms | `open_p95_ms` |
| 首批显示 | ≤ 50ms，完整模型长度仍为 20,000 且有窗口化行访问 | `first_batch_ms`、`first_batch_item_count`、`first_batch_rows` |
| 峰值工作集 | ≤ 60MiB | `working_set_mib` |
| 滚动采样 | 恰好 200 次，访问窗口始终 ≤100 行并到达首尾 | `long_scroll_*` |
| 滚动 P95 | ≤ 50ms，必须包含实际 ListView 更新 | `long_scroll_p95_ms` |
| 缩略图字段 | 10,000 个图片卡片逐项含非空 16×16 RGBA 缩略图 | `thumbnail_summary_count`、`thumbnail_loaded_count`、`thumbnail_width`、`thumbnail_height` |
| 混合几何 | 内容高度等于 `10,000×106 + 10,000×186`，误差≤1px | `expected_content_height`、`observed_content_height`、`geometry_matches` |
| 资源清理 | 隐藏后 testing backend tick 已执行且有工作集采样 | `post_cleanup_mock_tick`、`post_cleanup_working_set_mib` |
| LRU 约束 | 脚本显式运行 ATOM-42 三组窄测试且每组 passed>0，最终机器结果收口为 passed | `lru_contract_tests=passed` |

工作集、时间或滚动证据不可用时必须失败；不能把 `NA` 或零值当作通过。任一硬门禁
失败都按 ATOM-14 的停止条件报告证据，并停止本原子，不扩展到后续原子。

## 允许修改

- `tests/list_performance.rs`：新增独立、默认忽略的混合 Windows Release 性能场景。
- `scripts/measure_mixed_list_performance.ps1`：解析 `ATOM43_RESULT` 并执行硬门禁。
- 本原子计划文档和必要的中文注释清单。

## 明确禁止修改

- 不修改生产 UI、分页协调器、SQLite 查询、缩略图 loader 或 ATOM-42 LRU 实现。
- 不修改旧 ATOM-14R 探针和其阈值，避免改变历史证据口径。
- 不启动真实桌面程序、不访问系统剪贴板、托盘、默认应用目录或注册表。
- 不执行全量测试；只运行本探针、允许文件编译/格式/注释/差异检查及 ATOM-42
  LRU 相关窄单测。

## 计划验证命令

```powershell
cargo test --release --locked --test list_performance '测量一万文本与一万图片混合列表' -- --ignored --nocapture
pwsh -NoProfile -File scripts/measure_mixed_list_performance.ps1
cargo test --lib app::ui_event::tests::缩略图 -- --test-threads=1
cargo test --lib app::ui_event::tests::混合卡片 -- --test-threads=1
cargo test --lib app::ui_event::tests::滚动五百张图片缓存容量保持有界 -- --test-threads=1
cargo check --lib --tests
rustfmt --edition 2021 --check tests/list_performance.rs
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/check_comments.ps1
git diff --check
```

不得将全仓 `cargo fmt --check` 或全量测试失败归因于本原子；若命中基线差异，记录为
非阻塞诊断并保持本原子修改最小。

## 编码前 DDD

- 任务：`ddd_atom43_precode`（初审 `NEEDS_REVISION`，修订版本 2 待复审）。
- 初审缺口：首批误绑 30 条模型、滚动逐样本/首尾证据不足、缩略图非空尺寸未断言、
  LRU 未由脚本显式执行、非 Windows/非 Release/工作集不可用未 fail-closed、峰值未覆盖
  隐藏清理。
- 修订关闭方式：首批改为完整 20,000 条模型并断言首行 0、末行<100；滚动只改视口且
  每样本 1～100 行、首尾真实 offset 命中；逐项调用 `to_rgba8` 断言 10,000 个非空
  16×16 RGBA 缩略图；脚本先运行 ATOM-42 窄测试并解析每组 passed>0，再把字段收口为
  `lru_contract_tests=passed`；测试在非 Windows 或 debug 模式直接停止，工作集 0/缺失
  和解析异常均失败；混合内容高度必须匹配 106/186 几何，峰值在 200 次滚动后经隐藏
  与 testing backend tick 清理采样。
- 通过后才允许创建性能代码差异；若复审继续发现硬门禁证据缺口，先修订本文档。

## 执行结果与停止证据

- 编码前 DDD：`ddd_atom43_precode`，修订版本 2 复审结论 `PASS`；确认完整模型、逐项
  RGBA 图片、混合几何、首批/滚动边界、LRU 脚本证据和 fail-closed 字段解析。
- Windows x64 Release 探针已执行：`cargo test --release --locked --test list_performance
  '测量一万文本与一万图片混合列表' -- --ignored --nocapture`。
- 资源和时间初步证据：呼出 P95 `0.301ms`、首批 `0.173ms`、峰值工作集 `31.801MiB`、
  图片摘要/缩略图各 `10,000`、首批模型 `20,000` 条。
- 硬门禁失败证据：`observed_content_height=2,653,333.5`，预期
  `2,920,000`（`geometry_matches=false`）；200 次滚动中视口越界/反向样本 `81`，
  最终视口 `-2,208,900` 未到预期底部 `-2,653,019.5`，因此
  `long_scroll_supported=false` 且 P95 为 `NA`。
- `scripts/measure_mixed_list_performance.ps1` 已先通过 ATOM-42 三组窄测试（2、3、1
  项），随后按 `long_scroll_p95_ms=NA` fail-closed 返回非零；没有把失败结果改写成通过。
- 结论：现有 Slint 混合可变高度 ListView 的 viewport 总高/底部边界不能满足本原子的
  固定几何契约。按停止条件暂停本原子，不放宽阈值、不伪造 LRU 或 Release 通过证据；
  后续必须另立“混合高度窗口模型/几何协议”重规划原子。
- 其它窄门禁：`cargo check --locked --test list_performance`、目标 Clippy、目标
  rustfmt、中文注释检查和 `git diff --check` 通过。完整测试未执行。

## 提交前 DDD 与完成判定

- 提交前需由独立 DDD Agent 复核完整 diff、脚本硬门禁、旧 ATOM-14R 不变和测试证据。
- 最终差异哈希、DDD 任务名和结论由主 Agent 集成前补写。
- 只有在文本/图片数量、工作集、呼出、首批、200 次真实滚动和窗口化访问证据全部
  达标，且 ATOM-42 LRU 窄测试通过时，状态才可标记为 completed。
- Worker 只创建一个本地原子提交，不 push、不设置 upstream。

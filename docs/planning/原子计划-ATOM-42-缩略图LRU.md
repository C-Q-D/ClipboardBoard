# ClipboardBoard ATOM-42 缩略图 LRU 原子计划

## 计划元数据

- 计划 ID：ATOM-42
- 类型：atomic-development
- 修订版本：1
- 状态：completed，待主线集成
- 所属交付单元：UNIT-12
- 创建基线：`12184b0e62fd6bb6419441ddcbaac7bb24e45855`
- 风险等级：L3
- 本地分支：`codex/atom42-thumbnail-lru`
- worktree：`F:\workspace\small-projects\windows-copy-worktrees\thumbnail-lru`
- 远端约束：本分支不设置 upstream、不 push、不创建远端分支。

## 唯一目标

限制历史列表中已解码缩略图和 Slint UI 纹理的生命周期：只保留当前真实视口及上下各十条卡片范围内的图片，离开范围立即释放 UI 图片和解码缓冲；再次进入时重新排队加载；缓存容量最多 500 张图片并采用可验证的 LRU 顺序。

## 现状与调用关系

- `src/thumbnail_loader.rs` 在后台线程读取有界 WebP 并返回拥有型 RGBA 像素，不持有 Slint 对象。
- `src/app/ui_event.rs` 的 `schedule_thumbnail_requests` 按固定文本 `106px`、图片 `186px` 计算当前加载区域，维护 `UI_THUMBNAIL_CACHE`、失败集合、请求集合和淘汰顺序。
- `set_window_snapshot` 将缓存中的 `slint::Image` 写入 `ClipboardCard` 模型；因此仅清 Rust 缓存不足以释放模型仍持有的纹理，范围变化时必须以空缩略图重建模型并保存合法视口。
- ATOM-41 已固定分页、追加绑定门禁、视口恢复和混合卡片高度；本原子不得复制或修改这些分页协议。

## 行为契约

1. 视口范围由 `viewport-y`、`visible-height` 和固定混合卡片高度计算；与视口相交的卡片为可视区，再向前后各扩展 10 条卡片。只对该范围内的图片建立保留身份集合。
2. 视口范围变化时，集合外的 `slint::Image`、失败占位和缓存顺序均被移除；模型重建后的范围外卡片使用默认空图片，避免 `ClipboardCard` 克隆继续持有纹理。
3. 缓存命中时将身份移动到 LRU 队尾；新结果按队尾插入。容量超过 500 时从队首淘汰，并同步移除对应成功或失败状态。
4. 迟到结果必须继续通过面板代次、记录 ID、内容哈希和当前保留集合校验；范围外或旧代次结果不得进入缓存。重新进入时没有缓存命中就重新提交后台请求。
5. 面板隐藏、退出、搜索/标签/捕获刷新和首页替换后沿用同一调度入口清理旧范围；不修改分页、存储、图片捕获/复制、隐私或配置行为。

## 允许修改

- `src/app/ui_event.rs`：缩略图视口范围、LRU 顺序、模型重建和对应私有单元测试。
- `docs/planning/原子计划-ATOM-42-缩略图LRU.md`：本原子执行记录。

## 明确禁止修改

- `src/history_query.rs`、`src/storage/`、`src/app/ui_event.rs` 中 ATOM-41 分页/追加状态机以外的逻辑。
- `src/thumbnail_loader.rs` 的后台读取协议，除非定向编译暴露出本原子无法避免的类型问题。
- `ui/app-window.slint`、图片捕获与复制、设置/隐私、托盘、快捷键和性能门禁 ATOM-43。
- 根共享计划、阶段记录和项目工作台；由主 Agent 集成时统一回写，避免并行冲突。

## 实现与验证要求

1. 先锁定混合高度下“可视区 ±10 条”的纯范围计算测试。
2. 实现范围外清理、缓存命中触摸、500 容量 LRU 和模型视口保持。
3. 增加范围外释放、重新进入重新请求、迟到结果隔离、滚动 500 张图片不超过 500 的定向测试。
4. 仅运行本原子相关测试、`cargo check --lib`、允许文件定向 rustfmt、中文注释检查和 `git diff --check`；不运行全量测试，不启动真实应用。

## 完成判定

- 真实混合列表只为视口及上下 10 条图片保持纹理；滚出后模型与 Rust 缓存均不再持有该纹理。
- 滚回后身份未命中缓存会重新加载，旧代次/迟到结果无法污染当前模型。
- LRU 顺序可由测试证明，缓存最多 500 条，500 张图片滚动场景保持有界。
- 定向验证及中文注释、格式、差异检查通过；提交前需由主 Agent 安排独立 DDD。

## 执行记录

- 编码前 DDD：`ddd_atom42_precode` 与重试任务在限定时间内未回报；Worker 依据同一
  ARTIFACT/CONTRACT/EVIDENCE 完成只读反证，重点检查了 `ClipboardCard` 的
  `slint::Image` 克隆、视口模型重建递归、范围外释放、LRU 与迟到结果隔离，未改变
  既定行为契约。
- 实际实现：按混合卡片真实高度计算可视区及前后十条；范围变更同步清理 Rust 缓存、
  失败状态和 LRU 顺序，并重建卡片模型释放范围外 `Image` 克隆；缓存命中触摸队尾、
  容量上限固定为 500；隐藏、退出和旧代次结果沿用同一清理/身份门禁。
- 定向验证：
  - `cargo test --lib app::ui_event::tests::缩略图 -- --test-threads=1`：2 项通过。
  - `cargo test --lib app::ui_event::tests::混合卡片 -- --test-threads=1`：3 项通过。
  - `cargo test --lib app::ui_event::tests::滚动五百张图片缓存容量保持有界 -- --test-threads=1`：1 项通过。
  - `cargo test --lib app::ui_event::tests::迟到缩略图结果释放在途身份 -- --test-threads=1`：1 项通过。
  - `cargo test --lib app::ui_event::tests::隐藏面板不调度缩略图 -- --test-threads=1`：1 项通过。
  - `cargo check --lib`、`cargo clippy --lib -- -D warnings`、
    `rustfmt --edition 2021 --check src/app/ui_event.rs`、中文注释检查和
    `git diff --check`：通过。
- 提交前 DDD：任务 `ddd_atom42_final`，结论 `PASS`，无修订；复核了可视区±10条、
  模型 `Image` 克隆释放、500 容量 LRU、范围外按需重载、迟到结果身份校验及与
  ATOM-41 分页状态机的边界。提交前源代码 raw diff SHA256：
  `3b34011166283e032492ac6ccfc1ed27997a9b0245825d6f05039214f067680a`。
- Worker 仅创建一个本地原子提交，不 push、不设置 upstream。

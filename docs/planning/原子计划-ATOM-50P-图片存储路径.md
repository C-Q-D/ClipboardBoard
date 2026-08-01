# ATOM-50P 图片存储路径配置计划

## 计划元数据

- 计划 ID：`ATOM-50P`
- 类型：`atomic-development`
- 修订版本：`2`
- 状态：`active`
- 依赖：ATOM-44 已集成；不依赖已取消的 ATOM-48。
- 基线：`b54ea87`
- 分支：`codex/atom50p-image-path`
- Worktree：`F:\workspace\small-projects\windows-copy-worktrees\image-path`
- 风险等级：`L2`
- 远端约束：Worker 只在本地提交，不设置 upstream、不 push。

## 唯一目标

允许用户持久化一个图片资产根目录；默认仍为 `%LOCALAPPDATA%\ClipboardBoard\images`。
新捕获使用新路径，已有 SQLite 图片根和文件保持可读，禁止因修改路径而静默搬迁或删除旧资产。

## 当前代码事实

- `ImageStoragePreference::{Default, Custom(PathBuf)}`、绝对路径和专用子目录校验已存在。
- `prepare_image_storage` 已能创建受管目录、写 owner marker 并在自定义根失败时返回明确回退信息。
- `SettingsWorker` 已有原子 JSON/备份保存和未知字段保留能力，但 `AppSettings` 尚未保存图片根路径。
- `main.rs` 当前总是使用 `ImageStoragePreference::Default`，需要从已验证设置快照选择偏好。
- SQLite 的 `image_asset_roots` 记录每个历史图片根；切换路径后必须继续读取历史根，不覆盖旧根记录。

## 允许修改

- `src/settings/model.rs`、`src/settings/mod.rs`、`src/settings/worker.rs`、`src/settings/persistence.rs`：增加可选图片路径字段、持久化和语义校验。
- `src/image_storage/mod.rs`、`src/image_storage/prepare.rs`：提供从配置值转换为 `ImageStoragePreference` 的纯函数及错误映射；不改变资产布局和安全边界。
- `src/main.rs`、`src/history_bridge.rs`：启动时消费设置快照，初始化新的图片 capability；只做最小接线。
- `tests/` 中设置、路径和图片桥窄测试；本计划文档与中文注释。

## 明确禁止修改

- 不实现图片空间配额、自动淘汰、资产对账、历史保留设置或隐私设置页面。
- 不迁移、重命名、删除已有图片根或 SQLite 资产；旧根只读能力必须保留。
- 不修改剪贴板格式、图片编码、缩略图 LRU、数据库 schema 或清理事务。
- 不启动真实程序、真实用户目录、系统托盘或默认图片目录；测试使用唯一临时目录和注入设置目录。

## 配置契约

- 配置字段使用 `history.image_storage_root: Option<String>`：`None` 表示默认根；非空值必须是绝对、专用、无 `.`/`..` 的目录。
- 由 `image_storage` 提供唯一的无副作用 `parse_image_storage_preference(Option<&str>)` 解析器；设置保存校验和启动转换都必须调用它，禁止在 `settings::model` 复制一套路径规则。
- 解析器统一拒绝相对路径、盘符根、UNC share 根、保留恢复目录、空字符串、控制字符和 `.`/`..`，错误映射为稳定 `ImageStorageRoot` 字段错误；保存前拒绝的值不能落盘后再由启动回退。
- 重启加载后路径值保持不变；未知 JSON 字段继续原样保留。
- 配置路径只决定下一次 `prepare_image_storage` 使用的根；切换过程中已有 `ImageStorageRootId` 记录和图片文件不受影响。
- 自定义目录创建失败时不静默写入失败目录；沿用既有明确回退结果，调用方必须将实际生效根用于后续捕获。
- 当自定义目录创建失败时，启动接线必须记录不含完整路径的 fallback 原因，并把 `PreparedImageStorage::layout().asset_root()` 作为实际生效根；窄测试同时断言 fallback 原因和实际根，禁止继续使用失败请求路径。

## 实现步骤

1. 在设置 DTO 增加可选路径字段，并调用唯一解析器完成校验；补齐默认、合法、非法、重启和未知字段保留测试。
2. 在图片存储模块实现无副作用的唯一解析器，设置保存和启动转换共享同一错误分类与规范化结果。
3. 启动初始化先读取设置快照，再创建图片存储 capability；历史恢复仍通过数据库中的根路径读取。
4. 增加切换后新捕获写入新根、旧图片根保持可读的窄集成测试；测试不得触碰默认用户目录。

## 验收与定向验证

- `cargo test --lib settings::model <图片路径校验用例> -- --test-threads=1`
- `cargo test --lib settings::persistence <图片路径持久化用例> -- --test-threads=1`
- `cargo test --lib image_storage <路径转换用例> -- --test-threads=1`
- `cargo test --lib history_bridge <切换根/旧根可读用例> -- --test-threads=1`
- `cargo test --lib image_storage <fallback 实际生效根/原因用例> -- --test-threads=1`
- `cargo check --lib --tests`、目标 Clippy、目标 rustfmt、`git diff --check`。

禁止运行全量测试；不启动真实 ClipboardBoard，不读写 `%LOCALAPPDATA%\ClipboardBoard`。

## DDD 门禁

- 编码前 DDD：审查配置字段兼容、路径安全、旧资产可读、无迁移/配额范围；必须 PASS 后编码。
- 提交前 DDD：复核完整 diff、测试证据和启动接线；必须 PASS 后提交。

## 完成判定

- 配置重启保留、非法路径拒绝、合法路径生效、旧根可读均有定向证据。
- 只修改允许范围，中文注释与差异检查通过；Worker 分支创建一个唯一 ATOM-50P 本地提交，不 push。

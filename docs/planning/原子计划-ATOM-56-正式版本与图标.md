# ATOM-56 正式版本与图标资源计划

## 计划元数据

- 计划 ID：`ATOM-56`
- 类型：`atomic-development`
- 修订版本：`1`
- 状态：`completed`
- 基线：`92c15f5`
- 分支：`codex/atom56-release`
- Worktree：`F:\workspace\small-projects\windows-copy-worktrees\release-assets`
- 风险等级：`L1`
- 远端约束：Worker 只在本地提交；主 Agent 集成后才推送 `origin/main`。

## 唯一目标

让 Release 程序拥有稳定的 ClipboardBoard 产品身份、可审计的版本元数据、统一的 Windows 应用/托盘图标，并且 Release 入口不显示控制台窗口。

## 范围与不变量

1. 版本唯一来源使用 Cargo package version；构建资源中的 FileVersion/ProductVersion 必须来自 `CARGO_PKG_VERSION`，不得手写第二份版本号。
2. 产品名固定为 `ClipboardBoard`，描述固定为“轻量 Windows 剪贴板工作台”，原始文件名固定为 `clipboard-board.exe`。
3. Windows 资源使用仓库内的图标源和可重复的 build script 生成/嵌入；资源编译工具不可用时，开发机仍必须可 `cargo check`，但构建脚本要明确输出警告，不得伪造资源已嵌入。
4. Release 目标使用 `windows_subsystem = "windows"`，不改变现有 UI、托盘消息、剪贴板、设置、热键和清理语义。
5. 托盘优先加载本程序资源图标；资源不可用时仅允许回退系统图标并保留可诊断的构建/运行状态，不得改变托盘生命周期。
6. 不新增安装器、自动更新、发布性能脚本、签名、云同步或新的设置页面；这些属于 ATOM-57～60 或未来范围。
7. 图标源必须不依赖网络；提交中保留源文件/生成规则，不能只提交无法追溯的构建产物。

## 允许修改

- `Cargo.toml`：版本元数据和必要的 Windows 资源构建依赖。
- `build.rs`：版本资源、图标资源和 Windows 子系统构建接线。
- `src/main.rs`：仅增加 Release 子系统声明或等价的编译属性。
- `src/platform/windows/tray.rs`：仅把托盘图标加载顺序切换为本程序资源并保留安全回退。
- `assets/`：应用图标源文件及其中文说明。
- 本计划、根任务台账和项目阶段记录。

## 禁止修改

- 不改变窗口布局、搜索、历史、图片、隐私、快捷键协议和启动项业务。
- 不写真实注册表、数据库或用户数据；不启动安装器和登录启动验证。
- 不把资源编译失败静默吞掉；不能为了通过本机检查删除资源接线。
- 不提交 `target/`、临时 `.res/.rc` 输出或本地用户图标缓存。

## 实现契约

### 版本与资源

- `Cargo.toml` 的版本保持一个可发布的 SemVer（当前为 `0.1.0`），构建脚本从环境变量读取并写入版本资源。
- Windows 资源至少包含主图标、文件描述、产品名、产品版本、文件版本和原始文件名。
- 资源 ID 固定且由代码常量引用，避免托盘代码与资源脚本出现隐式编号漂移。
- 图标源采用深色背景、白色剪贴板轮廓的 16/32/48/256 像素多尺寸 ICO，颜色和形状与现有黑底白字界面保持一致。

### Release 子系统

- Windows 构建的入口必须链接 GUI subsystem；Debug/Release 都不得因为资源接线重新引入控制台窗口。
- 非 Windows 编译路径继续可检查，不引用 Windows 资源 API。

### 托盘加载

- `TrayGuard` 创建图标时先从当前模块按固定资源 ID加载；加载失败才复制 `IDI_APPLICATION` 作为兜底。
- 现有 `CopyIcon`/`DestroyIcon` 所有权、`NIM_DELETE` 清理和错误传播保持不变。
- 单元测试只验证资源 ID、加载策略和回退分支的纯逻辑，不访问真实 Shell 托盘。

## 定向验证

1. `cargo check --lib --bin clipboard-board`。
2. `cargo test --lib platform::windows::tray`，覆盖资源 ID/回退策略和既有托盘消息边界。
3. `cargo clippy --lib --bin clipboard-board --all-features -- -D warnings`。
4. 目标文件 `rustfmt --edition 2021 --config skip_children=true --check` 与 `git diff --check`。
5. 用脚本检查 Cargo version、资源字段、图标源尺寸/格式和 Release 子系统声明；资源编译器可用时额外验证 PE 资源存在，工具不可用时只记录环境限制。

禁止全量 Rust 测试；不启动真实登录项或安装器。

## 完成判定

- 版本只从 Cargo package version 产生，资源/托盘/Release 接线均有窄证据。
- Release 编译不显示控制台；Windows 资源编译可用时 PE 中能找到 ClipboardBoard 产品字段和主图标。
- 本机没有资源编译器时，窄检查仍通过并明确输出“资源编译器不可用”的警告，不把回退误报为成功。
- Worker 只有一个本地提交，主线集成后在 ATOM-57～60 前停止。

## DDD 门禁

- 编码前复核资源工具缺失时的可验证降级、版本单一来源、图标资源 ID 和托盘所有权，必须 PASS。
- 提交前复核完整 diff、Release subsystem、资源失败语义和托盘回退，不扩大到发布/安装器范围。

## 完成记录

- 已完成版本元数据、Release GUI subsystem、应用/托盘图标资源和资源编译器缺失警告接线。
- ICO 源已固定为 16、32、48、256 四个尺寸，并由 `build.rs` 校验目录项。
- 已通过 `cargo check --lib --bin clipboard-board`、托盘定向测试（8 项）和 ICO 契约脚本；本机未安装 `rc.exe`，因此仅验证开发构建警告路径。

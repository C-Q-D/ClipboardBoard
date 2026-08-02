# ATOM-61 历史列表与图标回归修复

## 原子元数据

- 原子编号：ATOM-61
- 类型：回归缺陷修复
- 风险级别：L2（Slint 列表布局、Windows 资源构建与运行时托盘共享边界）
- 基线：`42d2906`
- 依赖：ATOM-56（版本与图标资源基线）
- 状态：completed

## 问题契约

用户复制文本后打开面板时，历史查询已经返回记录，但显式几何列表的卡片不可见；同时，
在未把 Windows SDK 资源编译器目录加入 PATH 的开发环境中，构建会跳过图标资源，导致
窗口和托盘图标无法使用应用图标。

## 根因与目标

1. `geometry-history-list` 使用绝对定位容器，`HistoryCard` 的 `preferred-height` 不会
   参与实际布局，卡片实际高度为零。
2. `build.rs` 只通过 PATH 查找 `rc.exe`，而常见 Windows SDK 安装目录未必在 PATH 中。

目标是让首次打开时已有文本记录立即可见，并让构建脚本自动发现常见 Windows SDK 的
`rc.exe`，继续嵌入应用图标和版本资源；不改变用户已确认的交互、历史数据或图标源。

## 实现边界

- `ui/app-window.slint`：显式几何列表为文本/图片卡片设置固定行高。
- `src/app/ui_event.rs`：提交几何快照时先发布 offsets，再发布 cards，避免首次 repeater
  绑定到空偏移模型。
- `build.rs`：PATH 优先，随后探测 `WindowsSdkDir`、`ProgramFiles(x86)` 和
  `ProgramFiles` 下的 Windows SDK 版本目录；把已解析目录注入当前构建进程，并显式传给
  `winres::WindowsResource::set_toolkit_path`，确保 MSVC 资源编译器实际使用该目录。
- 不引入诊断正文日志，不修改数据库 schema，不改变图片、筛选和粘贴行为。

## 验证契约

- 定向单元测试：历史几何模块 4 项全部通过。
- 定向回归测试：首次打开首页查询测试通过。
- 增量 Windows 构建：不手动修改 PATH，`cargo build --bin clipboard-board` 成功且无
  “资源编译器不可用”警告。
- 真实 Windows 冒烟：启动最新构建，复制唯一标记，Alt+V 呼出后面板可见且标记显示。

## DDD 门禁

- 编码前计划门禁：`/root/atom53_plan_ddd`，结果 PASS；确认显式行高、模型顺序、
  `winres` toolkit_path 和回退边界。
- 提交前最终差异门禁：`/root/atom53_plan_ddd`，结果 PASS；无阻断项。

## 执行记录

- 2026-08-02：定位为显式几何卡片零高度；增加显式行高并调整模型发布顺序。
- 2026-08-02：增加 Windows SDK `rc.exe` 自动发现与当前进程 PATH 注入。
- 2026-08-02：定向测试 4+1 通过；真实 Windows 复制→呼出冒烟通过。
- 2026-08-02：补充 `winres::WindowsResource::set_toolkit_path`，提交前 DDD PASS。

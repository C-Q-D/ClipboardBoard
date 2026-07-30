# CF_BITMAP 回退原子开发计划

## 计划元数据

- 计划 ID：ATOMIC-CF-BITMAP-FALLBACK-001
- 类型：atomic-development
- 修订版本：2
- 状态：completed
- 父级 ID：ATOM-34
- 创建基线：55d8389

## 目标

确认 `CF_BITMAP` 来源是否已经由 ATOM-33 的 `CF_DIBV5`/`CF_DIB` 路径完整覆盖；只有
Windows 不保证合成格式时，才增加 `GetDIBits`、HDC 和 GDI 资源管理代码。

## 当前证据

- Microsoft Learn 的 Clipboard Formats 明确列出 `CF_BITMAP → CF_DIB` 和
  `CF_BITMAP → CF_DIBV5` 的系统合成转换。
- 同一文档说明：应用只放置 `CF_BITMAP` 时，系统会在剪贴板关闭时生成 DIB/DIBV5，
  以固定当时的调色板。
- ClipboardBoard 只在 `OpenClipboard` 成功后读取，并以 200ms 有界重试等待写入方释放；
  因此不把 `WM_CLIPBOARDUPDATE` 的精确投递时刻作为唯一前提。
- ATOM-33 已在单次 ClipboardIO 打开中优先请求 `CF_DIBV5`，缺失时请求 `CF_DIB`；
  `IsClipboardFormatAvailable` 和 `GetClipboardData` 会触达系统合成格式。

参考：

- https://learn.microsoft.com/en-us/windows/win32/dataxchg/clipboard-formats
- https://learn.microsoft.com/en-us/windows/win32/dataxchg/standard-clipboard-formats

## 决策候选

DDD 已确认上述系统保证与读取边界成立，ATOM-34 标为 `[~]`：

- 不新增 `CF_BITMAP` 直接读取。
- 不新增 `GetDIBits`、HDC、临时 HBITMAP 或 GDI 资源生命周期。
- ATOM-35 继续使用 PNG、DIBV5、DIB 三种拥有型输入。

## 验收

1. 权威文档能证明 `CF_BITMAP` 自动合成为 DIB/DIBV5。
2. 当前读取发生在写入方关闭剪贴板之后。
3. ATOM-33 已按 DIBV5、DIB 顺序请求系统格式。
4. DDD 结论为 `PASS`，确认无需额外 GDI 回退代码。

## 完成记录

- 结论：`[~]`，仓库现状已满足，不产生功能代码。
- 系统在关闭剪贴板时合成 DIB；ClipboardBoard 以 `OpenClipboard` 成功作为可读取边界。
- `IsClipboardFormatAvailable` 与 `GetClipboardData` 可访问系统合成格式，ATOM-33 已按
  DIBV5、DIB 顺序复制拥有型字节。
- 直接增加 `GetDIBits`/HDC 不会改善 owner 不履行延迟呈现的场景，反而引入颜色差异和
  GDI 句柄生命周期。
- DDD 最终结论：`PASS`。若真实应用出现 DIBV5/DIB 均不可用的可复现 `CF_BITMAP`，
  再按停止条件重规划。

## 停止或重规划条件

- 发现 Windows 目标版本不保证上述合成。
- ClipboardBoard 在写入方关闭剪贴板前读取。
- 真实用户验证出现仅 `CF_BITMAP` 且 DIBV5/DIB 均不可用的可复现应用。

## 验证与 Git

- 本原子只验证现有接缝和文档证据，不运行全量测试。
- 若满足，更新根计划、工作台和阶段记录，创建独立文档提交并推送 `origin/main`。
- 若不满足，再拆分直接 GDI 提取的代码原子，不在本计划中临时扩大实现。

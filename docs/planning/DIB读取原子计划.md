# DIB 与 DIBV5 读取原子开发计划

## 计划元数据

- 计划 ID：ATOMIC-DIB-READ-001
- 类型：atomic-development
- 修订版本：3
- 状态：active
- 父级 ID：ATOM-33
- 创建基线：936891d

## 总体计划

### 产品交付单元

本计划实现 `UNIT-10 / ATOM-33`，把 Windows 剪贴板常见 `CF_DIBV5` 和 `CF_DIB`
内容安全转换为 ATOM-31 的顶向下规范 RGBA8 像素。

### 目标结果

- 纯解析器支持 24/32 位 `BI_RGB` 和 32 位 `BI_BITFIELDS`。
- 正确处理 bottom-up、top-down、四字节行对齐和 RGB(A) 位掩码。
- 所有头部、偏移、行跨度、绝对高度和像素长度均使用 checked arithmetic。
- ClipboardIO 在剪贴板关闭前复制有界拥有型字节，不把 HGLOBAL 带出后端。

### 验收场景

1. 24 位 bottom-up BGR 与行填充转换为正确的顶向下 RGBA8。
2. 32 位 top-down `BI_RGB` 转换为不透明 RGBA8。
3. 32 位 `BI_BITFIELDS` 的内嵌或外置连续掩码正确缩放通道并处理可选 alpha。
4. 零尺寸、负宽、`i32::MIN` 高度、错误 planes/位深/压缩、重叠或非连续掩码、
   非零 `biClrUsed`、V5 profile、未知头尺寸、截断、偏移和乘法溢出均稳定失败。
5. ClipboardIO 优先读取 `CF_DIBV5`，缺失时读取 `CF_DIB`，返回关闭后仍有效的字节。
6. sequence 失配、格式缺失、HGLOBAL 超限、剪贴板忙碌和关闭失败均明确失败。

### 明确排除

- 不支持 1/4/8/16 位、调色板、RLE、JPEG、PNG 压缩 DIB 或 `BI_ALPHABITFIELDS`。
- 不读取 `CF_BITMAP`；该格式属于 ATOM-34。
- 不接入 worker 格式优先级、图片文件、SQLite、缩略图或 UI。
- 不修改或自动测试用户真实剪贴板。

### 已确认格式契约

- 输入从 DIB 头开始，不包含 `BITMAPFILEHEADER`。
- 只接受已知头尺寸 40、52、56、108、124 字节；其他扩展尺寸稳定拒绝。
- 24/32 位输入必须满足 `biClrUsed == 0`，本计划不猜测最优颜色表长度。
- 宽度必须为正；高度非零，负值表示 top-down，正值表示 bottom-up。
- 行跨度为 `((width × bit_count + 31) / 32) × 4`，逐行仅消费有效像素。
- 24 位 `BI_RGB` 按 BGR 解码并补 alpha 255。
- 32 位 `BI_RGB` 的高字节按保留位处理，输出 alpha 255，避免传统剪贴板 DIB
  因高字节全零而完全透明。
- 32 位 `BI_BITFIELDS` 要求 RGB 掩码非零、连续且互不重叠；alpha 掩码可为零，
  非零时也必须连续且不与 RGB 重叠。通道按掩码有效位线性缩放到 0 至 255。
- 40 字节头的 `BI_BITFIELDS` 从头后读取三个外置 RGB DWORD；52/56 字节及更长头
  从头内读取 RGB 掩码；56、108、124 字节头从偏移 52 读取可选 alpha 掩码。
- 像素偏移固定为：40 字节 `BI_RGB` 为 40；40 字节 `BI_BITFIELDS` 为 52；
  52/56/108/124 字节头为 `biSize`。
- 124 字节 V5 头的 `bV5ProfileData` 或 `bV5ProfileSize` 任一非零即拒绝；本原子不做
  ICC profile 定位或颜色管理。
- 不信任 `biSizeImage`。所需像素范围只由 checked 行跨度乘绝对高度得到，必须满足
  `pixel_offset + required_bytes <= input.len()`；允许并忽略尾随字节。

### 跨原子不变量

- 固定 `MAX_DIB_ENCODED_BYTES = 72 MiB`，规范结果继续受 64 MiB RGBA8 上限和
  16384 单维上限约束。
- 解析错误为稳定枚举，不包含原始字节、系统错误文本或本地路径。
- 先验证完整头、已知尺寸、颜色表/profile 和格式，再计算像素偏移、行跨度和总范围，
  最后分配规范像素。
- ClipboardIO 打开期间只复制字节，不解析 DIB。
- HGLOBAL 锁定成功后必须在返回前解锁；成功打开剪贴板后必须尝试关闭。
- 关闭失败优先于读取错误和 sequence 失配。

### 原子依赖顺序

1. `DIB-01`：有界解析 DIB/DIBV5。
2. `DIB-02`：从 Win32 剪贴板复制 DIBV5/DIB 字节。

### 整体验证

- 仅运行 DIB 解析和 Clipboard reader 定向测试。
- 运行库 check、库 Clippy、中文注释门禁和 diff-check。
- 不运行全量测试，不修改真实剪贴板。

### 执行与 Git 策略

- 执行模式：连续执行。
- 每个原子独立验证、L3 DDD 复核、提交并推送 `origin/main`。
- 计划文档先独立提交推送。

## DIB-01 有界解析 DIB/DIBV5

- 状态：done
- 支持的验收场景：场景 1 至 4。
- 唯一目标：把借用的 DIB/DIBV5 字节安全转换为规范 RGBA8。
- 当前行为与目标行为：当前仅有 PNG 解码；完成后纯函数支持常见内存 DIB。
- 前置条件与依赖：ATOM-31 已提交。
- 代码定位依据：`src/domain/image_content.rs`、`src/image_decode/`。
- 允许修改：DIB 解析模块、模块导出、定向测试、注释门禁和计划状态。
- 明确不修改：ClipboardBackend、Win32、worker、持久化和 UI。
- 实现步骤：
  1. 读取并验证已知头尺寸、宽高、planes、位深、压缩、`biClrUsed` 和 V5 profile。
  2. 解析外置或内嵌位掩码，验证连续性和不重叠。
  3. checked 计算规范长度、行跨度、像素偏移和输入总范围。
  4. 按方向逐行解码 BGR/BGRA 或 bitfields，并构造规范像素。
- 接口、数据与错误契约：纯函数借用字节切片；稳定区分截断、格式不支持、尺寸超限、
  掩码无效、算术/范围超限和规范像素失败。
- 边界与异常：不从损坏头部猜测像素偏移；多余尾部数据不被解释。
- 测试要求：颜色、方向、行填充、内外掩码、alpha 缺失/存在、未知头、非零颜色表、
  V5 profile、错误 `biSizeImage` 和全部算术/截断边界。
- 验证命令：DIB 模块测试；库 check/Clippy；中文注释门禁；diff-check。
- 预期结果：任何分配和像素读取前完成完整范围证明。
- 完成判定：定向验证通过，完整 diff 经 L3 DDD `PASS`。
- 交付给下一原子的输出：可消费拥有型 DIB 字节的纯解析函数。
- 停止或重新规划条件：目标应用的常见 DIB 使用本计划排除的压缩或位深。
- 风险等级：L3
- DDD 门禁：提交前复核算术、行方向、掩码和 alpha 语义。
- 计划提交信息：`feat(image): [DIB-01] 有界解析 DIB 与 DIBV5`

### DIB-01 执行记录

- 已实现 24/32 位 `BI_RGB`、32 位 `BI_BITFIELDS`、上下方向和 DWORD 行对齐。
- 已锁定已知头尺寸、颜色表/profile 排除、内外掩码位置和不透明 `BI_RGB` alpha。
- 所有输出长度、行跨度、像素偏移和输入范围均在分配前完成 checked 验证。
- DIB 定向测试 8 项、库 check、库 Clippy、中文注释门禁和 diff-check 均通过。
- 最终 DDD 结论：`PASS`。

## DIB-02 从剪贴板复制 DIBV5/DIB 字节

- 状态：in_progress
- 支持的验收场景：场景 5、6。
- 唯一目标：在 ClipboardIO 协议内优先复制 DIBV5，否则复制 DIB 的有界拥有型字节。
- 当前行为与目标行为：当前可读取文本和注册 PNG；完成后可独立取得 DIB 字节。
- 前置条件与依赖：DIB-01 已提交。
- 代码定位依据：`src/clipboard/reader.rs` 的注册 PNG 有界复制与 sequence 协议。
- 允许修改：reader backend、DIB Win32 适配、reader 定向测试和计划状态。
- 明确不修改：worker DTO、跨格式总优先级、DIB 解析调用、持久化和 UI。
- 实现步骤：
  1. 增加 DIB 读取结果，携带 `DibV5` 或 `Dib` 来源类型和拥有型字节。
  2. 生产实现按 `CF_DIBV5`、`CF_DIB` 顺序检查可用性并取 HGLOBAL。
  3. 复用有界二进制 HGLOBAL 复制，应用 72 MiB 上限。
  4. 增加独立读取函数，沿用 expected sequence、打开重试、关闭和最终 sequence 语义。
- 接口、数据与错误契约：成功返回来源类型与 `Vec<u8>`；格式均缺失返回稳定错误。
- 边界与异常：单次打开内完成格式选择和复制；关闭失败优先。
- 测试要求：V5 优先、DIB 回退、拥有型字节、sequence 两处失配、忙碌、缺失、
  超限和关闭失败。
- 验证命令：reader DIB 定向测试；库 check/Clippy；中文注释门禁；diff-check。
- 预期结果：ATOM-35 可按已选择的 DIB 类型调用纯解析器。
- 完成判定：定向验证通过，完整 diff 经 L3 DDD `PASS`。
- 交付给下一原子的输出：ClipboardIO 的 DIBV5/DIB 有界读取接缝。
- 停止或重新规划条件：目标 Windows 格式不是可锁定 HGLOBAL。
- 风险等级：L3
- DDD 门禁：提交前复核格式优先、锁释放、关闭和 sequence 顺序。
- 计划提交信息：`feat(clipboard): [DIB-02] 复制 DIBV5 与 DIB 字节`

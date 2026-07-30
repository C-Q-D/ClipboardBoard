# 注册 PNG 读取原子开发计划

## 计划元数据

- 计划 ID：ATOMIC-REGISTERED-PNG-001
- 类型：atomic-development
- 修订版本：1
- 状态：active
- 父级 ID：ATOM-32
- 创建基线：cebaf23

## 总体计划

### 产品交付单元

本计划实现 `UNIT-10 / ATOM-32`，安全读取 Windows 剪贴板注册 `PNG` 格式，并把
拥有型编码字节解码为 ATOM-31 的规范 RGBA8 像素。

### 目标结果

- PNG 编码和解码内存均有明确上限。
- 解码前先读取尺寸并检查像素乘法，拒绝解压炸弹。
- ClipboardIO 在关闭剪贴板前复制拥有型 PNG 字节，关闭后才允许解码。
- 读取前后 sequence 必须一致，旧更新事件不能取得新图片。

### 验收场景

1. 小型 RGBA、RGB、灰度或透明 PNG 得到精确的规范 RGBA8 像素。
2. 空、截断、损坏、编码超限、单维超限和 RGBA 结果超限均返回稳定错误。
3. 注册 PNG 字节在关闭剪贴板后仍可使用，且不携带 HGLOBAL。
4. 打开前或读取后 sequence 失配、格式缺失、剪贴板忙碌和关闭失败均明确失败。

### 明确排除

- 不读取 DIB、DIBV5 或 `CF_BITMAP`。
- 不修改 worker 捕获结果 DTO、格式优先级和历史提交。
- 不写图片文件、SQLite、UI、日志或设置。
- 不修改用户真实剪贴板进行自动测试。

### 已确认架构与代码接缝

- `src/clipboard/reader.rs` 已封装文本的打开重试、HGLOBAL 复制、sequence 和关闭协议。
- `src/domain/image_content.rs` 已提供规范像素与内容哈希。
- `ATOM-35` 后续消费注册 PNG、DIB 和 BITMAP 的读取接口进入 ImageWorker。
- `image 0.25.10` 支持 PNG-only feature、decoder limits 和 RGBA8 转换。

### 跨原子不变量

- 固定 `MAX_PNG_ENCODED_BYTES = 30 MiB`、`MAX_IMAGE_DIMENSION = 16384`、
  `MAX_IMAGE_RGBA_BYTES = 64 MiB`。
- 错误不包含图片字节、外部解码器错误文本或本地路径。
- 判定顺序固定为编码长度、PNG 头、单维、checked RGBA 长度、解码、规范像素复核。
- ClipboardIO 打开期间只复制编码字节，不做 PNG 解码。
- 所有 HGLOBAL 锁在返回前释放，剪贴板在 sequence 最终判断前完成关闭。

### 原子依赖顺序

1. `PNG-01`：有界解码注册 PNG。
2. `PNG-02`：从 Win32 剪贴板复制注册 PNG 字节。

### 整体验证

- 只运行 PNG 解码和 Clipboard reader 定向测试。
- 运行 `cargo check --lib`、库 Clippy、中文注释门禁和 diff-check。
- 不运行全量测试，不修改真实剪贴板。

### 执行与 Git 策略

- 执行模式：连续执行。
- 每个原子独立验证、L3 DDD 复核、提交并推送 `origin/main`。
- 规划文档先独立提交推送。

## PNG-01 有界解码注册 PNG

- 状态：in_progress
- 支持的验收场景：场景 1、2。
- 唯一目标：把有界 PNG 编码字节解码为规范 RGBA8 像素。
- 当前行为与目标行为：当前没有图片解码依赖；完成后纯函数可安全解码 PNG。
- 前置条件与依赖：ATOM-31 已提交。
- 代码定位依据：`src/domain/image_content.rs`、`Cargo.toml`。
- 允许修改：PNG 解码模块、PNG-only 依赖、模块导出、定向测试、注释门禁和计划状态。
- 明确不修改：ClipboardBackend、Win32、worker、存储和 UI。
- 实现步骤：
  1. 增加禁用默认 feature 的 `image 0.25.10` PNG 依赖。
  2. 先拒绝空输入和超过 30 MiB 的编码。
  3. 用带 limits 的 PNG decoder 读取头部尺寸。
  4. 在像素解码前检查单维和 `width × height × 4 <= 64 MiB`。
  5. 解码、转换 RGBA8 并交 `CanonicalImagePixels` 防御复核。
- 接口、数据与错误契约：纯函数借用编码切片；错误稳定区分空、编码超限、解码失败、
  单维超限、RGBA 超限和规范像素失败。
- 边界与异常：外部 decoder 的其余失败统一映射为 `DecodeFailed`，不透传错误文本。
- 测试要求：有效多色彩类型；空、截断、损坏、编码超限、单维和乘积超限。
- 验证命令：PNG 模块测试；库 check/Clippy；中文注释门禁；diff-check。
- 预期结果：超限图片在完整像素解码和 RGBA 分配前被拒绝。
- 完成判定：定向验证通过，完整 diff 经 L3 DDD `PASS`。
- 交付给下一原子的输出：可消费拥有型编码字节的纯 PNG 解码函数。
- 停止或重新规划条件：image decoder 无法在像素解码前可靠取得尺寸。
- 风险等级：L3
- DDD 门禁：提交前复核资源上限和错误分类。
- 计划提交信息：`feat(image): [PNG-01] 有界解码注册 PNG`

## PNG-02 从剪贴板复制注册 PNG 字节

- 状态：pending
- 支持的验收场景：场景 3、4。
- 唯一目标：在 ClipboardIO 协议内复制注册 PNG 的有界拥有型编码字节。
- 当前行为与目标行为：当前 backend 只读取 Unicode 文本；完成后可单独读取注册 PNG。
- 前置条件与依赖：PNG-01 已提交。
- 代码定位依据：`src/clipboard/reader.rs` 的 backend、打开重试和 sequence 协议。
- 允许修改：reader backend、注册 PNG Win32 适配、reader 定向测试和计划状态。
- 明确不修改：worker DTO、捕获格式选择、图片解码调用、持久化和 UI。
- 实现步骤：
  1. backend 增加注册 PNG 拥有型字节读取方法。
  2. 生产实现注册 `PNG` 格式并检查可用性。
  3. 按 GlobalSize 上限锁定、复制、解锁 HGLOBAL。
  4. 新增独立读取函数，复用打开重试、expected sequence、关闭和最终 sequence 语义。
- 接口、数据与错误契约：成功返回 `Vec<u8>`；错误新增格式缺失、全局内存不可用和
  PNG 编码超限，不携带原始字节。
- 边界与异常：关闭失败优先于读取或 sequence 结果；任何成功值都已脱离 HGLOBAL。
- 测试要求：有效字节、sequence 两处失配、忙碌、缺失、超限和关闭失败。
- 验证命令：reader PNG 定向测试；库 check/Clippy；中文注释门禁；diff-check。
- 预期结果：不解码、不泄漏句柄的注册 PNG 字节读取 API 可供 ATOM-35 使用。
- 完成判定：定向验证通过，完整 diff 经 L3 DDD `PASS`。
- 交付给下一原子的输出：ATOM-33/35 可复用的 ClipboardIO 注册 PNG 读取接缝。
- 停止或重新规划条件：注册 PNG 在目标 Windows 版本不使用可锁定的 HGLOBAL。
- 风险等级：L3
- DDD 门禁：提交前复核锁释放、关闭和 sequence 顺序。
- 计划提交信息：`feat(clipboard): [PNG-02] 复制注册 PNG 剪贴板字节`

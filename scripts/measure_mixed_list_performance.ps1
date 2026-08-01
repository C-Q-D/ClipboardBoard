# 此脚本运行 ATOM-43 的 Windows Release 混合历史性能探针，并执行固定硬门禁。
# 它只读取测试输出，不访问真实剪贴板、托盘、默认数据目录或系统配置。

[CmdletBinding()]
param(
    # 项目根目录；未指定时由脚本路径反推，兼容 PowerShell -File 调用。
    [string]$ProjectRoot = ''
)

$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($ProjectRoot)) {
    $scriptPath = $MyInvocation.MyCommand.Path
    if ([string]::IsNullOrWhiteSpace($scriptPath)) {
        $ProjectRoot = (Get-Location).Path
    } else {
        $scriptDirectory = Split-Path -Parent $scriptPath
        $ProjectRoot = (Resolve-Path (Join-Path $scriptDirectory '..')).Path
    }
}
Set-Location -LiteralPath $ProjectRoot

# ATOM-42 的纹理 active set、范围外释放、迟到隔离和 500 项容量是本门禁的前置证据；
# 先显式执行三组窄测试，任何失败都阻止性能结果进入硬门禁。
$lruTests = @(
    '缩略图',
    '混合卡片',
    '滚动五百张图片缓存容量保持有界'
)
foreach ($testName in $lruTests) {
    $ErrorActionPreference = 'SilentlyContinue'
    $lruOutput = & cargo test --locked --lib "app::ui_event::tests::$testName" -- --test-threads=1 2>&1
    $lruExitCode = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    $lruOutput | ForEach-Object { Write-Output $_ }
    if ($lruExitCode -ne 0) {
        throw "ATOM-42 LRU 窄测试失败：$testName，退出码：$lruExitCode"
    }
    $lruPassCount = 0
    foreach ($line in $lruOutput) {
        if ($line -match 'test result: ok\.\s+(?<passed>\d+) passed') {
            $lruPassCount += [int64]$Matches['passed']
        }
    }
    if ($lruPassCount -le 0) {
        throw "ATOM-42 LRU 窄测试没有可验证的通过样本：$testName"
    }
}

# 只运行 ATOM-43 的忽略测试，避免把旧 ATOM-14R 探针混入本次硬门禁。
$ErrorActionPreference = 'SilentlyContinue'
$output = & cargo test --release --locked --test list_performance '测量一万文本与一万图片混合列表' -- --ignored --nocapture 2>&1
$exitCode = $LASTEXITCODE
$ErrorActionPreference = 'Stop'
$output | ForEach-Object { Write-Output $_ }
if ($exitCode -ne 0) {
    throw "ATOM-43 混合性能探针进程失败，退出码：$exitCode"
}

$resultLine = $output | Where-Object { $_ -match 'ATOM43_RESULT ' } | Select-Object -Last 1
if (-not $resultLine) {
    throw '未找到 ATOM43_RESULT 性能结果行。'
}

$fields = @{}
foreach ($field in ($resultLine -split '\s+')) {
    if ($field -match '^(?<key>[^=]+)=(?<value>.*)$') {
        $fields[$Matches.key] = $Matches.value
    }
}

function Get-Number([string]$Name) {
    if (-not $fields.ContainsKey($Name)) {
        throw "性能结果缺少字段：$Name"
    }
    $number = 0.0
    if (-not [double]::TryParse($fields[$Name], [Globalization.NumberStyles]::Float, [Globalization.CultureInfo]::InvariantCulture, [ref]$number)) {
        throw "性能结果字段不是数字：$Name=$($fields[$Name])"
    }
    if ([double]::IsNaN($number) -or [double]::IsInfinity($number)) {
        throw "性能结果字段不能是 NaN 或 Infinity：$Name=$($fields[$Name])"
    }
    return $number
}

function Get-Integer([string]$Name) {
    $number = Get-Number $Name
    if ($number -lt 0 -or $number -ne [math]::Truncate($number)) {
        throw "性能结果字段必须是非负整数：$Name=$($fields[$Name])"
    }
    return [int64]$number
}

$itemCount = Get-Integer 'item_count'
$logicalItemCount = Get-Integer 'logical_item_count'
$textSummaryCount = Get-Integer 'text_summary_count'
$imageSummaryCount = Get-Integer 'image_summary_count'
$firstBatchItemCount = Get-Integer 'first_batch_item_count'
$firstBatchRows = Get-Integer 'first_batch_rows'
$firstBatchFirstRow = Get-Integer 'first_batch_first_row'
$firstBatchLastRow = Get-Integer 'first_batch_last_row'
$windowStart = Get-Integer 'window_start'
$windowLength = Get-Integer 'window_length'
$windowFirstAbsolute = Get-Integer 'window_first_absolute'
$windowLastAbsolute = Get-Integer 'window_last_absolute'
$datasetRevision = Get-Integer 'dataset_revision'
$windowRevision = Get-Integer 'window_revision'
$thumbnailSummaryCount = Get-Integer 'thumbnail_summary_count'
$thumbnailLoadedCount = Get-Integer 'thumbnail_loaded_count'
$thumbnailWidth = Get-Integer 'thumbnail_width'
$thumbnailHeight = Get-Integer 'thumbnail_height'
$expectedContentHeight = Get-Number 'expected_content_height'
$observedContentHeight = Get-Number 'observed_content_height'
$expectedGeometryDelta = [math]::Abs($observedContentHeight - $expectedContentHeight)
$openP95 = Get-Number 'open_p95_ms'
$firstBatch = Get-Number 'first_batch_ms'
$workingSet = Get-Number 'working_set_mib'
$postCleanupWorkingSet = Get-Number 'post_cleanup_working_set_mib'
$scrollInitialOffset = Get-Number 'scroll_initial_offset'
$scrollFinalOffset = Get-Number 'scroll_final_offset'
$scrollMaxOffset = Get-Number 'scroll_max_offset'
$longScrollSamples = Get-Integer 'long_scroll_samples'
$longScrollMaxBatchRows = Get-Integer 'long_scroll_max_batch_rows'
$longScrollFirstRow = Get-Integer 'long_scroll_first_row'
$longScrollLastRow = Get-Integer 'long_scroll_last_row'
$longScrollEmptyBatches = Get-Integer 'long_scroll_empty_batches'
$longScrollViewportMismatches = Get-Integer 'long_scroll_viewport_mismatches'
if (-not $fields.ContainsKey('long_scroll_supported') -or $fields['long_scroll_supported'] -notin @('true', 'false')) {
    throw "性能结果的 long_scroll_supported 必须是 true 或 false：$($fields['long_scroll_supported'])"
}
$longScrollSupported = $fields['long_scroll_supported'] -eq 'true'
if (-not $fields.ContainsKey('long_scroll_p95_ms') -or $fields['long_scroll_p95_ms'] -eq 'NA') {
    throw '长滚动 P95 缺失或为 NA，不能作为硬门禁证据。'
}
$longScrollP95 = Get-Number 'long_scroll_p95_ms'
if (-not $fields.ContainsKey('lru_contract_tests') -or $fields['lru_contract_tests'] -ne 'delegated_to_atom42_script') {
    throw '性能结果未声明待脚本注入的 ATOM-42 LRU 窄测试证据来源。'
}
# 只有本脚本已逐组执行并解析 passed>0 后，才把证据字段提升为最终 passed。
$fields['lru_contract_tests'] = 'passed'
if ($fields['lru_contract_tests'] -ne 'passed') {
    throw 'ATOM-42 LRU 窄测试证据字段未收口为 passed。'
}

$failures = [System.Collections.Generic.List[string]]::new()
if ($itemCount -ne 20000) { $failures.Add("混合模型条数 $itemCount != 20000") }
if ($logicalItemCount -ne 20000) { $failures.Add("逻辑数据集条数 $logicalItemCount != 20000") }
if ($textSummaryCount -ne 10000) { $failures.Add("文本摘要数 $textSummaryCount != 10000") }
if ($imageSummaryCount -ne 10000) { $failures.Add("图片摘要数 $imageSummaryCount != 10000") }
if ($firstBatchItemCount -ne 20000) { $failures.Add("首批绑定模型条数 $firstBatchItemCount != 20000") }
if ($firstBatchRows -le 0 -or $firstBatchRows -gt 100) { $failures.Add("首批访问行数 $firstBatchRows 不在 1..100") }
if ($firstBatchFirstRow -ne 0) { $failures.Add("首批起始行 $firstBatchFirstRow != 0") }
if ($firstBatchLastRow -ge 100) { $failures.Add("首批末行 $firstBatchLastRow >= 100") }
if ($windowStart -ne 0 -or $windowFirstAbsolute -ne 0) { $failures.Add("首帧窗口未从绝对索引 0 开始：start=$windowStart first=$windowFirstAbsolute") }
if ($windowLength -le 0 -or $windowLength -gt 100) { $failures.Add("首帧窗口长度 $windowLength 不在 1..100") }
if ($windowLastAbsolute -ge 100) { $failures.Add("首帧窗口末绝对索引 $windowLastAbsolute >= 100") }
if ($datasetRevision -le 0 -or $windowRevision -le 0) { $failures.Add('显式几何 revision 必须为正数') }
if ($thumbnailSummaryCount -ne 10000) { $failures.Add("图片缩略图摘要数 $thumbnailSummaryCount != 10000") }
if ($thumbnailLoadedCount -ne 10000) { $failures.Add("已加载代表性缩略图数 $thumbnailLoadedCount != 10000") }
if ($thumbnailWidth -ne 16 -or $thumbnailHeight -ne 16) { $failures.Add("代表性缩略图尺寸 ${thumbnailWidth}x${thumbnailHeight} != 16x16") }
if ($expectedGeometryDelta -gt 1.0) { $failures.Add("混合卡片内容高度差 $expectedGeometryDelta px > 1 px") }
if (-not $fields.ContainsKey('geometry_matches') -or $fields['geometry_matches'] -ne 'true') { $failures.Add('混合卡片几何证据未通过') }
if ($openP95 -le 0) { $failures.Add("混合列表呼出 P95 $openP95 ms 必须大于 0") }
if ($firstBatch -le 0) { $failures.Add("混合列表首批耗时 $firstBatch ms 必须大于 0") }
if ($workingSet -le 0 -or $postCleanupWorkingSet -le 0) { $failures.Add('Windows 工作集采样不可用') }
if ($workingSet -gt 60) { $failures.Add("混合列表峰值工作集 $workingSet MiB > 60 MiB") }
if ($openP95 -gt 100) { $failures.Add("混合列表呼出 P95 $openP95 ms > 100 ms") }
if ($firstBatch -gt 50) { $failures.Add("混合列表首批耗时 $firstBatch ms > 50 ms") }
$postCleanupTick = if ($fields.ContainsKey('post_cleanup_mock_tick')) { $fields['post_cleanup_mock_tick'] } else { '' }
if ($postCleanupTick -ne '1') { $failures.Add('隐藏后的 testing backend 清理 tick 证据缺失') }
if ($longScrollSamples -ne 200) { $failures.Add("长滚动样本数 $longScrollSamples != 200") }
if (-not $longScrollSupported) { $failures.Add('混合列表没有可验证的真实长滚动能力') }
if ($longScrollSupported -and ($longScrollP95 -lt 0 -or $longScrollP95 -gt 50)) {
    $failures.Add("混合列表长滚动 P95 $longScrollP95 ms 不在 0..50 ms")
}
if ($longScrollSupported -and $longScrollP95 -le 0) { $failures.Add('长滚动 P95 必须包含实际 ListView 更新') }
if ($longScrollMaxBatchRows -le 0 -or $longScrollMaxBatchRows -gt 100) {
    $failures.Add("单次滚动访问行数 $longScrollMaxBatchRows 不在 1..100")
}
if ($longScrollFirstRow -gt 100) { $failures.Add("起始可见行 $longScrollFirstRow 不在顶部窗口") }
if ($longScrollLastRow -lt ($itemCount - 100)) { $failures.Add("末尾可见行 $longScrollLastRow 未到达列表尾部") }
if ($longScrollEmptyBatches -ne 0) { $failures.Add("无 row_data 访问的滚动样本 $longScrollEmptyBatches != 0") }
if ($longScrollViewportMismatches -gt 20) { $failures.Add("视口越界或反向样本 $longScrollViewportMismatches > 20") }
if ([math]::Abs($scrollInitialOffset) -gt 1.0) { $failures.Add("滚动初始视口 $scrollInitialOffset 未位于顶部") }
if ([math]::Abs($scrollFinalOffset + $scrollMaxOffset) -gt 1.0) { $failures.Add("滚动最终视口 $scrollFinalOffset 未到达底部边界 -$scrollMaxOffset") }

if ($failures.Count -gt 0) {
    Write-Error ('ATOM-43 混合列表硬门禁未通过：' + ($failures -join '；'))
    exit 1
}

# 输出经本脚本真实运行 LRU 窄测后收口的机器结果；不把 worker 的 delegated 占位值当最终证据。
$canonicalResultLine = $resultLine -replace 'lru_contract_tests=delegated_to_atom42_script', 'lru_contract_tests=passed'
Write-Output $canonicalResultLine
Write-Output "ATOM-43 混合列表硬门禁通过：20,000 条混合摘要，呼出 P95=$openP95 ms，首批=$firstBatch ms，工作集=$workingSet MiB，长滚动 P95=$longScrollP95 ms，ATOM-42 LRU 窄测试通过。"

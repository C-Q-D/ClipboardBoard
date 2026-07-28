# 此脚本运行 ATOM-14R 的 Windows Release 大列表实验，并按计划中的硬门禁判定结果。
# 它不修改生产数据，只读取测试输出；失败时以非零退出码阻止把未通过的列表策略提交为完成。

[CmdletBinding()]
param(
    # 项目根目录；空值时优先使用脚本路径推导，兼容 Windows PowerShell -File 调用。
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

$ErrorActionPreference = 'SilentlyContinue'
$output = & cargo test --release --locked --test list_performance -- --ignored --nocapture 2>&1
$exitCode = $LASTEXITCODE
$ErrorActionPreference = 'Stop'
$output | ForEach-Object { Write-Output $_ }
if ($exitCode -ne 0) {
    throw "ATOM-14R 性能探针进程失败，退出码：$exitCode"
}

$resultLine = $output | Where-Object { $_ -match 'ATOM14_RESULT ' } | Select-Object -Last 1
if (-not $resultLine) {
    throw '未找到 ATOM14_RESULT 性能结果行。'
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
$openP95 = Get-Number 'open_p95_ms'
$firstBatch = Get-Number 'first_batch_ms'
$workingSet = Get-Number 'working_set_mib'
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
$longScrollP95 = $null
if ($longScrollSupported) {
    $longScrollP95 = Get-Number 'long_scroll_p95_ms'
}

$failures = [System.Collections.Generic.List[string]]::new()
if ($itemCount -ne 20000) { $failures.Add("探针条数 $itemCount != 20000") }
if ($longScrollSamples -ne 200) { $failures.Add("长滚动样本数 $longScrollSamples != 200") }
if ($workingSet -le 0) { $failures.Add('工作集采样不可用') }
if ($workingSet -gt 60) { $failures.Add("打开后工作集 $workingSet MiB > 60 MiB") }
if ($openP95 -lt 0) { $failures.Add("呼出 P95 $openP95 ms 不能为负数") }
if ($firstBatch -lt 0) { $failures.Add("首批显示 $firstBatch ms 不能为负数") }
if ($workingSet -lt 0) { $failures.Add("工作集 $workingSet MiB 不能为负数") }
if ($openP95 -gt 100) { $failures.Add("呼出 P95 $openP95 ms > 100 ms") }
if ($firstBatch -gt 50) { $failures.Add("首批显示 $firstBatch ms > 50 ms") }
if (-not $longScrollSupported) { $failures.Add('当前列表没有可验证的长滚动能力') }
if ($longScrollSupported -and $longScrollP95 -lt 0) { $failures.Add("长滚动 P95 $longScrollP95 ms 不能为负数") }
if ($longScrollSupported -and $longScrollP95 -gt 50) { $failures.Add("长滚动 P95 $longScrollP95 ms > 50 ms") }
if ($longScrollSupported -and $longScrollP95 -le 0) { $failures.Add('长滚动 P95 必须包含实际重复器刷新耗时') }
if ($longScrollMaxBatchRows -le 0 -or $longScrollMaxBatchRows -gt 100) { $failures.Add("单次滚动访问行数 $longScrollMaxBatchRows 不在 1..100") }
if ($longScrollFirstRow -gt 100) { $failures.Add("起始可见行 $longScrollFirstRow 不在顶部窗口") }
if ($longScrollLastRow -lt ($itemCount - 100)) { $failures.Add("末尾可见行 $longScrollLastRow 未到达列表尾部") }
if ($longScrollEmptyBatches -gt 20) { $failures.Add("无 row_data 访问的滚动样本 $longScrollEmptyBatches > 20") }
if ($longScrollViewportMismatches -gt 20) { $failures.Add("视口位置不一致样本 $longScrollViewportMismatches > 20") }

if ($failures.Count -gt 0) {
    Write-Error ('ATOM-14R 硬门禁未通过：' + ($failures -join '；'))
    exit 1
}

Write-Output "ATOM-14R 硬门禁通过：呼出 P95=$openP95 ms，首批=$firstBatch ms，工作集=$workingSet MiB，长滚动 P95=$longScrollP95 ms。"

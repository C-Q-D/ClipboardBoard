# 此脚本运行 ATOM-14 的 Windows Release 大列表实验，并按计划中的硬门禁判定结果。
# 它不修改生产数据，只读取测试输出；失败时以非零退出码阻止把未通过的列表策略提交为完成。

[CmdletBinding()]
param(
    # 项目根目录，默认使用脚本所在目录的父目录。
    [string]$ProjectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..'))
)

$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $ProjectRoot

$output = & cargo test --release --locked --test list_performance -- --ignored --nocapture 2>&1
$exitCode = $LASTEXITCODE
$output | ForEach-Object { Write-Output $_ }
if ($exitCode -ne 0) {
    throw "ATOM-14 性能探针进程失败，退出码：$exitCode"
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
    return $number
}

$openP95 = Get-Number 'open_p95_ms'
$firstBatch = Get-Number 'first_batch_ms'
$workingSet = Get-Number 'working_set_mib'
$longScrollSupported = $fields['long_scroll_supported'] -eq 'true'

$failures = [System.Collections.Generic.List[string]]::new()
if ($workingSet -le 0) { $failures.Add('工作集采样不可用') }
if ($workingSet -gt 60) { $failures.Add("打开后工作集 $workingSet MiB > 60 MiB") }
if ($openP95 -gt 100) { $failures.Add("呼出 P95 $openP95 ms > 100 ms") }
if ($firstBatch -gt 50) { $failures.Add("首批显示 $firstBatch ms > 50 ms") }
if (-not $longScrollSupported) { $failures.Add('当前列表没有可验证的长滚动能力') }

if ($failures.Count -gt 0) {
    Write-Error ('ATOM-14 硬门禁未通过：' + ($failures -join '；'))
    exit 1
}

Write-Output "ATOM-14 硬门禁通过：呼出 P95=$openP95 ms，首批=$firstBatch ms，工作集=$workingSet MiB。"

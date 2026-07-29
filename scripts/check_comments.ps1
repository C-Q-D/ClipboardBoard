# 此脚本检查当前项目的核心源码是否包含中文文件级职责说明。
# 它只执行机械检查；类型、字段和关键逻辑注释仍需在代码审查中人工确认。

[CmdletBinding()]
param(
    # 项目根目录，默认使用脚本所在目录的父目录。
    [string]$ProjectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..'))
)

$ErrorActionPreference = 'Stop'

# 这些文件构成当前原子的源码边界，后续原子应按需扩展清单。
$requiredFiles = @(
    'build.rs',
    'src/lib.rs',
    'src/main.rs',
    'ui/app-window.slint',
    'src/command.rs',
    'src/diagnostics.rs',
    'src/domain/mod.rs',
    'src/domain/clipboard_item.rs',
    'src/domain/hash.rs',
    'src/domain/image_metadata.rs',
    'src/history.rs',
    'src/storage/mod.rs',
    'src/storage/migration.rs',
    'src/storage/worker.rs',
    'tests/list_performance.rs',
    'src/clipboard/mod.rs',
    'src/clipboard/io_worker.rs',
    'src/clipboard/reader.rs',
    'src/clipboard/writer.rs',
    'src/history_bridge.rs',
    'src/history_restore.rs',
    'src/app/mod.rs',
    'src/app/ui_event.rs',
    'tests/ui_event.rs',
    'src/platform/mod.rs',
    'src/platform/windows/mod.rs',
    'src/platform/windows/hotkey.rs',
    'src/platform/windows/system_window.rs',
    'src/platform/windows/tray.rs',
    'src/platform/windows/single_instance.rs',
    'src/platform/windows/source.rs',
    'src/platform/windows/window/mod.rs',
    'src/platform/windows/window/lifecycle.rs'
)

foreach ($relativePath in $requiredFiles) {
    $fullPath = Join-Path $ProjectRoot $relativePath

    if (-not (Test-Path -LiteralPath $fullPath)) {
        throw "缺少需要检查的源码文件：$relativePath"
    }

    $firstLine = Get-Content -LiteralPath $fullPath -TotalCount 1

    if ($firstLine -notmatch '此|负责|定义|脚本|入口|库') {
        throw "源码文件缺少中文文件级职责说明：$relativePath"
    }
}

Write-Output '中文文件级注释检查通过。'

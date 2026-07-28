# 此脚本使用隔离 LOCALAPPDATA 预置 85 条文本历史，并在真实 Windows 窗口上执行滚轮冒烟。
# 它只验证桌面事件接缝和进程稳定性；30+50+5 的精确分页由 Rust 定向测试证明。

param(
    [string]$Executable = "target\x86_64-pc-windows-msvc\debug\clipboard-board.exe"
)

$ErrorActionPreference = "Stop"
$smokeRoot = Join-Path $env:TEMP ("clipboard-board-wcb10-" + [guid]::NewGuid().ToString("N"))
$process = $null
$hadLocalAppData = Test-Path Env:LOCALAPPDATA
$previousLocalAppData = $env:LOCALAPPDATA
$hadSmokeDatabase = Test-Path Env:WCB10_SMOKE_DB
$previousSmokeDatabase = $env:WCB10_SMOKE_DB

try {
    # 冒烟必须独占 ClipboardBoard 进程名；发现用户实例时安全中止，绝不向其发送输入。
    $existingProcesses = Get-CimInstance Win32_Process -Filter "Name='clipboard-board.exe'"
    if ($null -ne $existingProcesses) {
        throw "检测到正在运行的 ClipboardBoard，请退出后再执行隔离滚轮冒烟"
    }

    $dataDirectory = Join-Path $smokeRoot "ClipboardBoard\data"
    New-Item -ItemType Directory -Path $dataDirectory -Force | Out-Null
    $env:WCB10_SMOKE_DB = Join-Path $dataDirectory "clipboard.db"

    # Python 标准库只用于创建隔离 SQLite 数据库，不读写仓库文件。
    @'
import os
import sqlite3

connection = sqlite3.connect(os.environ["WCB10_SMOKE_DB"])
connection.executescript("""
CREATE TABLE clipboard_items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    item_type TEXT NOT NULL,
    text_content TEXT,
    preview_text TEXT NOT NULL,
    content_hash BLOB NOT NULL,
    source_exe TEXT,
    source_app TEXT,
    copy_count INTEGER NOT NULL DEFAULT 1,
    is_pinned INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    copied_at INTEGER NOT NULL,
    last_used_at INTEGER
);
CREATE UNIQUE INDEX idx_clipboard_items_content_hash ON clipboard_items(content_hash);
CREATE INDEX idx_clipboard_items_copied ON clipboard_items(copied_at DESC, id DESC);
PRAGMA user_version = 1;
""")
for index in range(85):
    text = f"WCB10-滚动记录-{index + 1:03d}"
    connection.execute(
        "INSERT INTO clipboard_items(item_type,text_content,preview_text,content_hash,source_app,created_at,copied_at) VALUES(?,?,?,?,?,?,?)",
        ("text", text, text, index.to_bytes(32, "little"), "WCB10冒烟", index + 1, index + 1),
    )
connection.commit()
connection.close()
'@ | python -
    if ($LASTEXITCODE -ne 0) {
        throw "无法创建隔离冒烟数据库"
    }

    $resolvedExecutable = (Resolve-Path $Executable).Path
    $env:LOCALAPPDATA = $smokeRoot
    # 冒烟需要真实显示 Slint 面板，因此不能把 GUI 进程强制设为隐藏窗口状态。
    $process = Start-Process -FilePath $resolvedExecutable -PassThru
    $env:LOCALAPPDATA = $previousLocalAppData
    Start-Sleep -Milliseconds 1200

    Add-Type @'
using System;
using System.Runtime.InteropServices;

public static class ClipboardBoardScrollSmoke
{
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern IntPtr FindWindowEx(IntPtr parent, IntPtr after, string className, string windowName);

    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);

    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr window);

    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr window, out Rectangle rectangle);

    [DllImport("user32.dll")]
    public static extern bool SetCursorPos(int x, int y);

    [DllImport("user32.dll")]
    public static extern void mouse_event(uint flags, uint x, uint y, int data, UIntPtr extraInfo);

    [DllImport("user32.dll")]
    public static extern void keybd_event(byte virtualKey, byte scanCode, uint flags, UIntPtr extraInfo);

    public struct Rectangle
    {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }
}
'@

    # 先确认应用自己的热键消息窗口已就绪，再发送真实 Alt+V 组合验证注册链路。
    $messageWindow = [ClipboardBoardScrollSmoke]::FindWindowEx(
        [IntPtr](-3),
        [IntPtr]::Zero,
        "ClipboardBoardHotkey",
        $null
    )
    if ($messageWindow -eq [IntPtr]::Zero) {
        throw "未找到 ClipboardBoard 热键消息窗口"
    }
    $messageProcessId = 0
    [ClipboardBoardScrollSmoke]::GetWindowThreadProcessId(
        $messageWindow,
        [ref]$messageProcessId
    ) | Out-Null
    if ($messageProcessId -ne $process.Id) {
        throw "热键消息窗口不属于本次冒烟进程"
    }
    [ClipboardBoardScrollSmoke]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero)
    [ClipboardBoardScrollSmoke]::keybd_event(0x56, 0, 0, [UIntPtr]::Zero)
    [ClipboardBoardScrollSmoke]::keybd_event(0x56, 0, 2, [UIntPtr]::Zero)
    [ClipboardBoardScrollSmoke]::keybd_event(0x12, 0, 2, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 1000

    $process.Refresh()
    $panel = [IntPtr]$process.MainWindowHandle
    if ($panel -eq [IntPtr]::Zero) {
        throw "呼出后未找到 ClipboardBoard 面板"
    }
    $panelProcessId = 0
    [ClipboardBoardScrollSmoke]::GetWindowThreadProcessId($panel, [ref]$panelProcessId) | Out-Null
    if ($panelProcessId -ne $process.Id) {
        throw "面板窗口不属于本次冒烟进程"
    }
    $rectangle = New-Object ClipboardBoardScrollSmoke+Rectangle
    [ClipboardBoardScrollSmoke]::GetWindowRect($panel, [ref]$rectangle) | Out-Null
    [ClipboardBoardScrollSmoke]::SetForegroundWindow($panel) | Out-Null
    [ClipboardBoardScrollSmoke]::SetCursorPos(
        [int](($rectangle.Left + $rectangle.Right) / 2),
        [int](($rectangle.Top + $rectangle.Bottom) / 2)
    ) | Out-Null

    # 测试脚本发送真实滚轮输入；生产源码仍禁止模拟粘贴或注入按键。
    1..36 | ForEach-Object {
        [ClipboardBoardScrollSmoke]::mouse_event(0x0800, 0, 0, -120, [UIntPtr]::Zero)
        Start-Sleep -Milliseconds 20
    }
    Start-Sleep -Milliseconds 1200

    $alive = Get-Process -Id $process.Id -ErrorAction SilentlyContinue
    if ($null -eq $alive) {
        throw "滚轮交互后测试进程已退出"
    }
    $alive.Refresh()
    $panelAfterScroll = [IntPtr]$alive.MainWindowHandle
    if ($panelAfterScroll -eq [IntPtr]::Zero) {
        throw "滚轮交互后进程或面板已退出"
    }
    $panelAfterProcessId = 0
    [ClipboardBoardScrollSmoke]::GetWindowThreadProcessId(
        $panelAfterScroll,
        [ref]$panelAfterProcessId
    ) | Out-Null
    if ($panelAfterProcessId -ne $process.Id) {
        throw "滚动后的面板窗口不属于本次冒烟进程"
    }

    [pscustomobject]@{
        PreloadedItems = 85
        ProcessAlive = $true
        PanelAlive = $true
        MessageWindow = "{0:X}" -f $messageWindow.ToInt64()
        PanelWindow = "{0:X}" -f $panelAfterScroll.ToInt64()
    }
}
finally {
    if ($null -ne $process) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
    }
    # 环境必须先于目录清理恢复，避免调用方会话残留指向已删除临时目录的变量。
    if ($hadLocalAppData) {
        $env:LOCALAPPDATA = $previousLocalAppData
    }
    else {
        Remove-Item Env:LOCALAPPDATA -ErrorAction SilentlyContinue
    }
    if ($hadSmokeDatabase) {
        $env:WCB10_SMOKE_DB = $previousSmokeDatabase
    }
    else {
        Remove-Item Env:WCB10_SMOKE_DB -ErrorAction SilentlyContinue
    }
    $resolvedSmokeRoot = [IO.Path]::GetFullPath($smokeRoot)
    $resolvedTempRoot = [IO.Path]::GetFullPath($env:TEMP).TrimEnd("\") + "\"
    if ($resolvedSmokeRoot.StartsWith($resolvedTempRoot, [StringComparison]::OrdinalIgnoreCase)) {
        Remove-Item -LiteralPath $resolvedSmokeRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

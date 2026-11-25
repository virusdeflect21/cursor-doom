# Checks a running Cursor Doom session from the outside:
#   - the overlay window exists with the click-through layered styles
#   - its size matches the requested cursor resolution
#   - it sits under the pointer according to the chosen anchor, and follows moves fast
#   - Windows is really showing a blank cursor while the session runs
#
# Usage: powershell -ExecutionPolicy Bypass -File tools/verify-cursor.ps1 [-Sample 20]

param(
    [int]$Sample = 20
)

$ErrorActionPreference = "Stop"
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT point);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr hWnd, int index);
    [DllImport("user32.dll")] public static extern IntPtr GetWindowLongPtr(IntPtr hWnd, int index);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
"@

# The session is per-monitor DPI aware; without this every rectangle and pointer
# position read here would be virtualised and would never line up.
[void][Win]::SetProcessDPIAware()

function Get-Style([IntPtr]$hwnd, [int]$index) {
    if ([IntPtr]::Size -eq 8) { return [Win]::GetWindowLongPtr($hwnd, $index).ToInt64() }
    return [Win]::GetWindowLong($hwnd, $index)
}

$statusPath = Join-Path $env:APPDATA "CursorDoom\session.status"
if (-not (Test-Path $statusPath)) {
    Write-Host "no session.status - start a session from the launcher first"
    exit 1
}

$status = @{}
Get-Content $statusPath | ForEach-Object {
    $parts = $_ -split "=", 2
    if ($parts.Count -eq 2) { $status[$parts[0]] = $parts[1] }
}

$exe = Join-Path $PSScriptRoot "..\cursor-doom\target\release\cursor-doom.exe"
$ok = $true

Write-Host "state        : $($status['state']) (stop reason: $($status['stop_reason']))"
Write-Host "fps          : $($status['fps'])"
Write-Host "frames       : $($status['frames'])"

if ($status['state'] -ne "running") {
    Write-Host "the session is not running - start it and try again" -ForegroundColor Yellow
    exit 1
}

$hwnd = [IntPtr][int64]$status['overlay_hwnd']
if ($hwnd -eq [IntPtr]::Zero) {
    Write-Host "FAIL: no overlay window handle in the status" -ForegroundColor Red
    exit 1
}

$exStyle = Get-Style $hwnd -20
$style = Get-Style $hwnd -16
$checks = @(
    @("layered", ($exStyle -band 0x80000) -ne 0),
    @("transparent", ($exStyle -band 0x20) -ne 0),
    @("topmost", ($exStyle -band 0x8) -ne 0),
    @("tool window", ($exStyle -band 0x80) -ne 0),
    @("no activate", ($exStyle -band 0x08000000) -ne 0),
    @("visible", [Win]::IsWindowVisible($hwnd))
)
foreach ($check in $checks) {
    if ($check[1]) { Write-Host "  ok   $($check[0])" }
    else { Write-Host "  FAIL $($check[0])" -ForegroundColor Red; $ok = $false }
}

$rect = New-Object Win+RECT
[void][Win]::GetWindowRect($hwnd, [ref]$rect)
$size = "$($rect.Right - $rect.Left)x$($rect.Bottom - $rect.Top)"
Write-Host "overlay size : $size (status says $($status['overlay_size']))"
if ($status['overlay_size'] -and $size -ne $status['overlay_size']) {
    Write-Host "FAIL: overlay is not the requested size" -ForegroundColor Red
    $ok = $false
}

# anchor=center keeps the pointer in the middle of the frame; topleft puts the
# image below/right of it.
$anchor = if ($status['anchor']) { $status['anchor'] } else { "center" }
$halfW = [int](($rect.Right - $rect.Left) / 2)
$halfH = [int](($rect.Bottom - $rect.Top) / 2)
Write-Host "anchor       : $anchor"

$point = New-Object Win+POINT
$worstX = 0
$worstY = 0
$latencies = @()
for ($i = 0; $i -lt $Sample; $i++) {
    if (-not [Win]::GetCursorPos([ref]$point)) { break }
    $expectedX = if ($anchor -eq "center") { $point.X - $halfW } else { $point.X }
    $expectedY = if ($anchor -eq "center") { $point.Y - $halfH } else { $point.Y }
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    $dx = 9999
    $dy = 9999
    while ($watch.ElapsedMilliseconds -lt 150) {
        [void][Win]::GetWindowRect($hwnd, [ref]$rect)
        $dx = [math]::Abs($rect.Left - $expectedX)
        $dy = [math]::Abs($rect.Top - $expectedY)
        if ($dx -le 1 -and $dy -le 1) { break }
        Start-Sleep -Milliseconds 1
    }
    $latencies += $watch.Elapsed.TotalMilliseconds
    $worstX = [math]::Max($worstX, $dx)
    $worstY = [math]::Max($worstY, $dy)
    Start-Sleep -Milliseconds 40
}
if ($latencies.Count -gt 0) {
    $avg = ($latencies | Measure-Object -Average).Average
    $max = ($latencies | Measure-Object -Maximum).Maximum
    Write-Host ("tracking     : max offset {0},{1} px; follow latency avg {2:N1} ms, max {3:N1} ms" -f $worstX, $worstY, $avg, $max)
    if ($worstX -gt 2 -or $worstY -gt 2) {
        Write-Host "FAIL: the overlay does not sit under the pointer" -ForegroundColor Red
        $ok = $false
    }
}

if (Test-Path $exe) {
    # The app is a GUI-subsystem binary, so its output is read through a file
    # instead of the console handle.
    $tmp = Join-Path $env:TEMP "cursor-doom-report.txt"
    Start-Process -FilePath $exe -ArgumentList @("--cursor-report") -NoNewWindow -Wait `
        -RedirectStandardOutput $tmp | Out-Null
    $report = (Get-Content $tmp -Raw) -replace "`r", ""
    Write-Host $report.Trim()
    if ($report -match 'invisible\s*:\s*true') {
        Write-Host "  ok   the system cursor is blank (hidden)"
    } else {
        Write-Host "  FAIL the system cursor is still visible while the doom cursor is on" -ForegroundColor Red
        $ok = $false
    }
} else {
    Write-Host "could not find $exe, skipped the cursor check" -ForegroundColor Yellow
}

if ($ok) { Write-Host "`nall checks passed" -ForegroundColor Green; exit 0 }
Write-Host "`nsome checks failed" -ForegroundColor Red
exit 2

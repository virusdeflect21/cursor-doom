# Starts a short session, moves the pointer around, and saves a screenshot plus the
# session status - handy on a normal desktop to see the cursor in action.
#
# Usage: powershell -ExecutionPolicy Bypass -File tools/smoke-overlay.ps1 [-Width 128] [-Height 80]

param(
    [int]$Width = 128,
    [int]$Height = 80,
    [int]$Frames = 140
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing, System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Smoke {
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT point);
}
"@

$root = Resolve-Path (Join-Path $PSScriptRoot "..")
$exe = Join-Path $root "cursor-doom\target\release\cursor-doom.exe"
$wad = Join-Path $root "DOOM.WAD"
if (-not (Test-Path $exe)) { throw "build first: cargo build --release in cursor-doom" }
if (-not (Test-Path $wad)) { throw "put DOOM.WAD in the repository root" }

$out = Join-Path $root "target\smoke"
New-Item -ItemType Directory -Force -Path $out | Out-Null

Write-Host "starting a $Width x $Height session for $Frames frames"
$proc = Start-Process -FilePath $exe -PassThru -ArgumentList @(
    "--session", "--wad", $wad, "--width", $Width, "--height", $Height,
    "--autostart", "1", "--max-frames", $Frames
)

$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$point = New-Object Smoke+POINT
$steps = @(
    @(($bounds.Width / 2), ($bounds.Height / 2)),
    @(($bounds.Width / 2 + 120), ($bounds.Height / 2 + 40)),
    @(($bounds.Width / 2 - 120), ($bounds.Height / 2 - 40))
)

$shot = $null
foreach ($step in $steps) {
    [void][Smoke]::SetCursorPos([int]$step[0], [int]$step[1])
    Start-Sleep -Milliseconds 700
    if (-not $proc.HasExited -and -not $shot) {
        Start-Sleep -Milliseconds 400
        $bitmap = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        $graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
        $path = Join-Path $out "screen.png"
        $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
        $graphics.Dispose()
        $bitmap.Dispose()
        $shot = $path
        Write-Host "screenshot: $shot"
    }
}

$proc.WaitForExit(20000) | Out-Null
if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
Start-Sleep -Milliseconds 500

$status = Join-Path $env:APPDATA "CursorDoom\session.status"
if (Test-Path $status) {
    Copy-Item $status (Join-Path $out "session.status") -Force
    Write-Host "status copied to $out\session.status"
}

& $exe --cursor-report
Write-Host "done (on a headless/VM desktop the screenshot comes out black - that is the environment, not the app)"

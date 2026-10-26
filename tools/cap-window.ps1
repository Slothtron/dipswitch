# Capture the demo window to a PNG.
#
# Two strategies, tried in order:
#   1. PrintWindow(PW_RENDERFULLCONTENT) — works without an interactive desktop
#      session, which is what a headless agent shell has. Measured sanity check:
#      a successful grab of this UI lands around 90+ distinct colours and a mean
#      brightness near 238 (light theme); a blank one is 1 colour / mean 0.
#   2. CopyFromScreen over the window rect — needs a real desktop; GPUI renders
#      through DirectX, so plain PrintWindow (flag 0) comes back black.
#
# Usage, with the demo already running:
#   powershell -ExecutionPolicy Bypass -File tools/cap-window.ps1
#   powershell -ExecutionPolicy Bypass -File tools/cap-window.ps1 -Out shots/capture.png
#
# 默认写到 shots/capture.png（已 gitignore）——不要指回 shots/ 里的已发布截图，
# 那会静默覆盖 README 用的图。

param(
    [string]$Title = "Dipswitch",
    [string]$Out = "shots/capture.png",
    [int]$SettleMs = 800
)

$ErrorActionPreference = "Stop"

Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinCap {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
    public struct RECT { public int Left, Top, Right, Bottom; }
}
"@

$root = [System.Windows.Automation.AutomationElement]::RootElement
$cond = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::NameProperty, $Title)

$win = $null
for ($i = 0; $i -lt 30 -and -not $win; $i++) {
    $win = $root.FindFirst([System.Windows.Automation.TreeScope]::Children, $cond)
    if (-not $win) { Start-Sleep -Milliseconds 300 }
}
if (-not $win) { Write-Output "WINDOW NOT FOUND: $Title"; exit 1 }

$hwnd = [IntPtr]$win.Current.NativeWindowHandle
[WinCap]::ShowWindow($hwnd, 9) | Out-Null        # SW_RESTORE
[WinCap]::SetForegroundWindow($hwnd) | Out-Null
Start-Sleep -Milliseconds $SettleMs

$rect = New-Object WinCap+RECT
[WinCap]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
$w = $rect.Right - $rect.Left
$h = $rect.Bottom - $rect.Top

# Strategy 1: PrintWindow with PW_RENDERFULLCONTENT.
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$dc = $g.GetHdc()
[WinCap]::PrintWindow($hwnd, $dc, 2) | Out-Null
$g.ReleaseHdc($dc)
$g.Dispose()

$distinct = @{}
for ($y = 0; $y -lt $h; $y += 7) {
    for ($x = 0; $x -lt $w; $x += 7) { $distinct[$bmp.GetPixel($x, $y).ToArgb()] = 1 }
}

$how = "PrintWindow"
if ($distinct.Count -le 2) {
    # Strategy 2: grab the screen region instead.
    $bmp.Dispose()
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($rect.Left, $rect.Top, 0, 0, (New-Object System.Drawing.Size($w, $h)))
    $g.Dispose()
    $how = "CopyFromScreen"
}

$full = [System.IO.Path]::GetFullPath($Out)
$dir = [System.IO.Path]::GetDirectoryName($full)
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
$bmp.Save($full, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()

Write-Output "saved: $full (${w}x${h}) via $how, $($distinct.Count) distinct colours"

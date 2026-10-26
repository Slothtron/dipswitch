# 用 ExtractIconExW 枚举 exe 里的图标组，验证 RT_GROUP_ICON 是否可被 Windows 解析。
# ExtractAssociatedIcon 只挑"最合适"的一枚，遇到 PNG 压缩的 ICO 条目可能返回 null，
# 所以它不足以判定图标资源坏了；能枚举出全部尺寸才是真信号。
param(
    [string]$Exe = "target\debug\dipswitch.exe"
)
$ErrorActionPreference = "Stop"
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class IconProbe {
    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    public static extern uint ExtractIconExW(string file, int index, IntPtr[] big, IntPtr[] small, uint n);
    [DllImport("user32.dll")] public static extern bool DestroyIcon(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetIconInfo(IntPtr h, out ICONINFO info);
    [StructLayout(LayoutKind.Sequential)]
    public struct ICONINFO { public bool fIcon; public int xHotspot; public int yHotspot; public IntPtr hbmMask; public IntPtr hbmColor; }
    [DllImport("gdi32.dll")] public static extern int GetObjectW(IntPtr h, int n, IntPtr pv);
    public static int IconWidth(IntPtr h) {
        ICONINFO info;
        if (GetIconInfo(h, out info) == 0) return -1;
        // BITMAP: 两个 mask/color 位图，宽在 bmWidth 处（int32，偏移 4）。
        IntPtr buf = Marshal.AllocHGlobal(64);
        GetObjectW(info.hbmColor != IntPtr.Zero ? info.hbmColor : info.hbmMask, 64, buf);
        int w = Marshal.ReadInt32(buf, 4);
        Marshal.FreeHGlobal(buf);
        return w;
    }
}
"@
$full = (Resolve-Path $Exe).Path
$count = [IconProbe]::ExtractIconExW($full, -1, $null, $null, 0)
Write-Output "icon groups in exe: $count"
if ($count -eq 0) { exit 1 }
$big = New-Object IntPtr[] $count
[void][IconProbe]::ExtractIconExW($full, -1, $big, $null, $count)
foreach ($h in $big) {
    if ($h -ne [IntPtr]::Zero) {
        Write-Output ("  group hicon={0} width={1}" -f $h, [IconProbe]::IconWidth($h))
        [void][IconProbe]::DestroyIcon($h)
    }
}

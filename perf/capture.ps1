<#
.SYNOPSIS
  LeafViewer를 띄우고 창을 PNG로 캡처한다 (화면 확인용, D2D 내용 포함).
.EXAMPLE
  .\perf\capture.ps1 -Target .\docs\SPEC.md -Out shot.png
#>
param(
    [string]$Exe = "$PSScriptRoot\..\target\release\leafviewer.exe",
    [string]$Target = "",
    [string]$Out = "$PSScriptRoot\results\capture.png",
    [int]$SettleMs = 2500,
    [string[]]$Keys = @()
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing, System.Windows.Forms
Add-Type @"
using System; using System.Runtime.InteropServices;
public static class W {
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr c);
  public struct RECT { public int L, T, R, B; }
}
"@
[W]::SetThreadDpiAwarenessContext([IntPtr]-4) | Out-Null  # PER_MONITOR_AWARE_V2: 실제 픽셀 크기로 캡처
$p = if ($Target) { Start-Process (Resolve-Path $Exe) -ArgumentList "`"$Target`"" -PassThru } else { Start-Process (Resolve-Path $Exe) -PassThru }
Start-Sleep -Milliseconds $SettleMs
$p.Refresh()
$h = $p.MainWindowHandle
[W]::SetForegroundWindow($h) | Out-Null
foreach ($k in $Keys) { [System.Windows.Forms.SendKeys]::SendWait($k); Start-Sleep -Milliseconds 700 }
$r = New-Object W+RECT; [W]::GetWindowRect($h, [ref]$r) | Out-Null
$bmp = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc(); [W]::PrintWindow($h, $hdc, 2) | Out-Null; $g.ReleaseHdc($hdc)
New-Item -ItemType Directory -Force (Split-Path $Out) | Out-Null
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
Stop-Process -Id $p.Id -Force
"saved: $Out"

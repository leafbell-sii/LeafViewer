<#
.SYNOPSIS
  LeafViewer를 띄우고, 창에 메시지를 보내 조작한 뒤 창을 PNG로 캡처한다 (D2D 내용 포함).

  조작은 전역 키 입력(SendKeys) 대신 LeafViewer 창에만 PostMessage로 보낸다 —
  다른 창이 앞에 있어도 입력이 새지 않는다.

  -Actions 항목 (순서대로 실행):
    key:F3            포커스된 컨트롤에 WM_KEYDOWN (가상 키 이름: F3, RETURN, NEXT, DOWN, ESCAPE …)
    syskey:Z          Alt+키 (WM_SYSKEYDOWN, 예: Alt+Z 자동 줄바꿈)
    text:문자열        포커스된 컨트롤(EDIT)에 WM_SETTEXT
    click:x,y         창 왼쪽 위 기준 물리 픽셀 위치의 자식 컨트롤에 왼쪽 클릭
    drag:x1,y1,x2,y2  끌기
    wheel:x,y,delta[,keys]  휠 (delta: 120 = 한 칸 위, keys: 8 = Ctrl, 4 = Shift)
    sleep:ms
.EXAMPLE
  .\perf\capture.ps1 -Target .\testdata\out\outline.pdf -Actions 'key:F3','text:page0005','key:RETURN' -Out find.png
#>
param(
    [string]$Exe = "$PSScriptRoot\..\target\release\leafviewer.exe",
    [string]$Target = "",
    [string]$Out = "$PSScriptRoot\results\capture.png",
    [int]$SettleMs = 2500,
    [string[]]$Actions = @(),
    [int]$StepMs = 600
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices;
public static class W {
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr c);
  [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr SendMessageW(IntPtr h, uint m, IntPtr w, string l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetGUIThreadInfo(uint tid, ref GUITHREADINFO gi);
  [DllImport("user32.dll")] public static extern IntPtr ChildWindowFromPointEx(IntPtr h, POINT p, uint f);
  [DllImport("user32.dll")] public static extern bool ScreenToClient(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  public struct RECT { public int L, T, R, B; }
  public struct POINT { public int X, Y; }
  [StructLayout(LayoutKind.Sequential)] public struct GUITHREADINFO {
    public int cbSize; public int flags; public IntPtr hwndActive; public IntPtr hwndFocus; public IntPtr hwndCapture;
    public IntPtr hwndMenuOwner; public IntPtr hwndMoveSize; public IntPtr hwndCaret; public RECT rcCaret; }
  public static IntPtr MakeL(int x, int y) { return (IntPtr)(((y & 0xFFFF) << 16) | (x & 0xFFFF)); }
}
"@
[W]::SetThreadDpiAwarenessContext([IntPtr]-4) | Out-Null  # PER_MONITOR_AWARE_V2: 실제 픽셀 크기
$p = if ($Target) { Start-Process (Resolve-Path $Exe) -ArgumentList "`"$Target`"" -PassThru } else { Start-Process (Resolve-Path $Exe) -PassThru }
Start-Sleep -Milliseconds $SettleMs
$p.Refresh()
$h = $p.MainWindowHandle
$r = New-Object W+RECT; [W]::GetWindowRect($h, [ref]$r) | Out-Null

function Get-Focus {
    $pid2 = 0; $tid = [W]::GetWindowThreadProcessId($h, [ref]$pid2)
    $gi = New-Object W+GUITHREADINFO; $gi.cbSize = [Runtime.InteropServices.Marshal]::SizeOf($gi)
    [W]::GetGUIThreadInfo($tid, [ref]$gi) | Out-Null
    if ($gi.hwndFocus -ne [IntPtr]::Zero) { $gi.hwndFocus } else { $h }
}
# 창 기준 좌표 → (가장 안쪽 자식 HWND, 그 자식의 클라이언트 좌표)
function Get-ChildAt([int]$x, [int]$y) {
    $sp = New-Object W+POINT; $sp.X = $r.L + $x; $sp.Y = $r.T + $y
    $cur = $h
    while ($true) {
        $cp = $sp; [W]::ScreenToClient($cur, [ref]$cp) | Out-Null
        $child = [W]::ChildWindowFromPointEx($cur, $cp, 1)  # CWP_SKIPINVISIBLE
        if ($child -eq [IntPtr]::Zero -or $child -eq $cur) { return @($cur, $cp) }
        $cur = $child
    }
}
$vk = @{ F3 = 0x72; RETURN = 0x0D; ESCAPE = 0x1B; NEXT = 0x22; PRIOR = 0x21; DOWN = 0x28; UP = 0x26; HOME = 0x24; END = 0x23; SPACE = 0x20; Z = 0x5A }
foreach ($a in $Actions) {
    $kind, $arg = $a -split ':', 2
    switch ($kind) {
        'key'   { $f = Get-Focus; [W]::PostMessageW($f, 0x100, [IntPtr]$vk[$arg], [IntPtr]1) | Out-Null; [W]::PostMessageW($f, 0x101, [IntPtr]$vk[$arg], [IntPtr]0xC0000001) | Out-Null }
        'syskey' { $f = Get-Focus; [W]::PostMessageW($f, 0x104, [IntPtr]$vk[$arg], [IntPtr]0x20000001) | Out-Null; [W]::PostMessageW($f, 0x105, [IntPtr]$vk[$arg], [IntPtr]0xE0000001) | Out-Null }
        'text'  { $f = Get-Focus; [W]::SendMessageW($f, 0x000C, [IntPtr]::Zero, $arg) | Out-Null }
        'click' { $x, $y = $arg -split ','; $t = Get-ChildAt $x $y; $l = [W]::MakeL($t[1].X, $t[1].Y)
                  [W]::PostMessageW($t[0], 0x201, [IntPtr]1, $l) | Out-Null; [W]::PostMessageW($t[0], 0x202, [IntPtr]0, $l) | Out-Null }
        'drag'  { $x1, $y1, $x2, $y2 = $arg -split ','; $t = Get-ChildAt $x1 $y1; $dx = [int]$x2 - [int]$x1; $dy = [int]$y2 - [int]$y1
                  [W]::PostMessageW($t[0], 0x201, [IntPtr]1, [W]::MakeL($t[1].X, $t[1].Y)) | Out-Null
                  foreach ($s in 1..8) { [W]::PostMessageW($t[0], 0x200, [IntPtr]1, [W]::MakeL($t[1].X + $dx * $s / 8, $t[1].Y + $dy * $s / 8)) | Out-Null }
                  [W]::PostMessageW($t[0], 0x202, [IntPtr]0, [W]::MakeL($t[1].X + $dx, $t[1].Y + $dy)) | Out-Null }
        'wheel' { $x, $y, $d, $k = $arg -split ','; $t = Get-ChildAt $x $y; $sp = New-Object W+POINT; $sp.X = $r.L + [int]$x; $sp.Y = $r.T + [int]$y
                  $wp = ((([int]$d -band 0xFFFF) -shl 16) -bor [int]$k)
                  [W]::PostMessageW($t[0], 0x20A, [IntPtr]$wp, [W]::MakeL($sp.X, $sp.Y)) | Out-Null }
        'sleep' { Start-Sleep -Milliseconds ([int]$arg) }
    }
    Start-Sleep -Milliseconds $StepMs
}
$bmp = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc(); [W]::PrintWindow($h, $hdc, 2) | Out-Null; $g.ReleaseHdc($hdc)
New-Item -ItemType Directory -Force (Split-Path $Out) | Out-Null
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$p.CloseMainWindow() | Out-Null
if (-not $p.WaitForExit(3000)) { Stop-Process -Id $p.Id -Force }
"saved: $Out"

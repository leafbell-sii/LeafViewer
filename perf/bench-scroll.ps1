<#
.SYNOPSIS
  PDF·TXT 연속 스크롤 벤치마크 (NFR-P07 60fps, NFR-P06 열람 중 메모리).
  `--bench-scroll`로 실행하면 앱이 첫 쪽 표시 후 16ms마다 40DIP씩 끝까지(최대 600틱) 내리며
  프레임(그리기) 시간을 기록하고 스스로 종료한다. 이 스크립트는 그동안 메모리를 표본 추출한다.
.EXAMPLE
  .\perf\bench-scroll.ps1 -Target .\testdata\out\large.pdf
#>
param(
    [string]$Exe = "$PSScriptRoot\..\target\release\leafviewer.exe",
    [Parameter(Mandatory)] [string]$Target,
    [int]$TimeoutSec = 60
)
$ErrorActionPreference = 'Stop'
$log = Join-Path $env:LOCALAPPDATA 'LeafViewer\logs\perf.log'
$p = Start-Process (Resolve-Path $Exe) -ArgumentList '--bench-scroll', "`"$((Resolve-Path $Target).Path)`"" -PassThru
$peakPriv = 0; $peakWs = 0; $samples = @()
$deadline = (Get-Date).AddSeconds($TimeoutSec)
while (-not $p.HasExited -and (Get-Date) -lt $deadline) {
    $p.Refresh()
    if ($p.PrivateMemorySize64 -gt $peakPriv) { $peakPriv = $p.PrivateMemorySize64 }
    if ($p.WorkingSet64 -gt $peakWs) { $peakWs = $p.WorkingSet64 }
    $samples += $p.PrivateMemorySize64
    Start-Sleep -Milliseconds 100
}
if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force; Write-Warning "timeout" }
$lines = Get-Content $log | Where-Object { $_ -like "pid=$($p.Id) *" }
$lines | Where-Object { $_ -match 'bench_scroll|pdf_first_page|pdf_load|txt_' }
"peak private bytes: {0:N1} MB, peak working set: {1:N1} MB, samples: {2}" -f ($peakPriv / 1MB), ($peakWs / 1MB), $samples.Count

<#
.SYNOPSIS
  M0 성능 측정 (SPEC §6.1, §10). 새 프로세스를 띄워 첫 화면 시간과 메모리를 기록한다.

.EXAMPLE
  .\perf\measure.ps1                       # 빈 창 (NFR-P01, NFR-P02)
  .\perf\measure.ps1 -Target C:\docs\a.pdf # PDF 첫 페이지 (NFR-P05)
#>
param(
    [string]$Exe = "$PSScriptRoot\..\target\release\leafviewer.exe",
    [string]$Target = "",
    [int]$Runs = 5,
    [int]$SettleMs = 2500
)

$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path $Exe).Path
$log = Join-Path $env:LOCALAPPDATA 'LeafViewer\logs\perf.log'
$outDir = Join-Path $PSScriptRoot 'results'
New-Item -ItemType Directory -Force $outDir | Out-Null
$csv = Join-Path $outDir ("m0-{0:yyyyMMdd-HHmmss}.csv" -f (Get-Date))

$rows = for ($i = 1; $i -le $Runs; $i++) {
    $argList = @('--new-instance')
    if ($Target) { $argList += "`"$Target`"" }
    $p = if ($argList) { Start-Process $exe -ArgumentList $argList -PassThru } else { Start-Process $exe -PassThru }
    Start-Sleep -Milliseconds $SettleMs
    $p.Refresh()
    $ws = $p.WorkingSet64
    $priv = $p.PrivateMemorySize64
    # "메모리(활성 개인 작업 집합)" = 작업 관리자 기본 열
    $pws = (Get-CimInstance Win32_PerfFormattedData_PerfProc_Process -Filter "IDProcess=$($p.Id)").WorkingSetPrivate
    $lines = Get-Content $log -ErrorAction SilentlyContinue | Where-Object { $_ -like "pid=$($p.Id) *" }
    $get = { param($k) ($lines | Where-Object { $_ -match " $k=([\d.]+)" } | Select-Object -First 1) -replace ".* $k=([\d.]+).*", '$1' }
    Stop-Process -Id $p.Id -Force
    [pscustomobject]@{
        run               = $i
        target            = $Target
        first_paint_ms    = & $get 'first_paint_ms'
        pdf_first_page_ms = & $get 'pdf_first_page_ms'
        pdfium_bind_ms    = & $get 'pdfium_bind_ms'
        private_ws_mb     = [math]::Round($pws / 1MB, 1)
        working_set_mb    = [math]::Round($ws / 1MB, 1)
        private_bytes_mb  = [math]::Round($priv / 1MB, 1)
    }
    Start-Sleep -Milliseconds 300
}

$rows | Format-Table -AutoSize
$rows | Export-Csv -NoTypeInformation -Encoding utf8 $csv
"exe size: {0:N2} MB" -f ((Get-Item $exe).Length / 1MB)
"saved: $csv"

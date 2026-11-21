<#
.SYNOPSIS
  포터블 배포 zip을 만든다 (SPEC §9, M5).

  dist\LeafViewer-<버전>-win-x64.zip
    LeafViewer.exe              릴리스 빌드 (정적 CRT — VC++ 재배포 패키지 불필요)
    pdfium.dll                  bblanchon/pdfium-binaries (V8/XFA 미포함)
    README.txt                  사용법·단축키·설정 위치
    THIRD-PARTY-NOTICES.txt     실행 파일에 들어간 Rust 크레이트와 PDFium의 라이선스

  설정·세션은 %APPDATA%·%LOCALAPPDATA%에 두므로 zip을 어디에 풀어도 동작한다.
.PARAMETER Pdfium
  pdfium.dll 경로. 기본: target\release\pdfium.dll
.PARAMETER PdfiumLicense
  pdfium-binaries 압축에 든 LICENSE 파일. 주면 고지 파일에 원문을 넣는다
.PARAMETER SkipBuild
  cargo build를 건너뛴다
.EXAMPLE
  .\tools\package.ps1
#>
param(
    [string]$Pdfium = "$PSScriptRoot\..\target\release\pdfium.dll",
    [string]$PdfiumLicense = "",
    [switch]$SkipBuild
)
$ErrorActionPreference = 'Stop'
# cargo 출력(UTF-8, 한글 설명 포함)을 콘솔 코드 페이지(CP949)로 읽지 않게
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$root = (Resolve-Path "$PSScriptRoot\..").Path
Set-Location $root

if (-not $SkipBuild) {
    cargo build --release -p leaf-app
    if ($LASTEXITCODE -ne 0) { throw "cargo build 실패" }
}
$exe = Join-Path $root 'target\release\leafviewer.exe'
if (-not (Test-Path $exe)) { throw "실행 파일이 없습니다: $exe" }
if (-not (Test-Path $Pdfium)) { throw "pdfium.dll이 없습니다: $Pdfium (-Pdfium으로 지정)" }

# ── 버전·의존성 (cargo metadata) ──
# PS 5.1의 ConvertFrom-Json은 대소문자만 다른 키(windows 크레이트 features)에서 실패한다 → JavaScriptSerializer
$json = (cargo metadata --format-version 1 --locked --filter-platform x86_64-pc-windows-msvc) -join "`n"
if ($LASTEXITCODE -ne 0) { throw "cargo metadata 실패" }
Add-Type -AssemblyName System.Web.Extensions
$js = New-Object System.Web.Script.Serialization.JavaScriptSerializer
$js.MaxJsonLength = [int]::MaxValue
$meta = $js.DeserializeObject($json)
$byId = @{}; foreach ($p in $meta['packages']) { $byId[$p['id']] = $p }
$nodes = @{}; foreach ($n in $meta['resolve']['nodes']) { $nodes[$n['id']] = $n }
$app = $meta['packages'] | Where-Object { $_['name'] -eq 'leaf-app' }
$version = $app['version']

# leaf-app에서 일반 의존성(빌드·개발 의존성 제외)만 따라간다 = 실행 파일에 들어간 크레이트
$seen = @{}; $stack = New-Object System.Collections.Stack; $stack.Push($app['id'])
while ($stack.Count) {
    $id = $stack.Pop()
    if ($seen[$id]) { continue }
    $seen[$id] = $true
    foreach ($d in $nodes[$id]['deps']) {
        if ($d['dep_kinds'] | Where-Object { $null -eq $_['kind'] }) { $stack.Push($d['pkg']) }
    }
}
$deps = $seen.Keys | ForEach-Object { $p = $byId[$_]
        [pscustomobject]@{ name = $p['name']; version = $p['version']; license = $p['license']
            repository = $p['repository']; source = $p['source']; manifest_path = $p['manifest_path'] } } |
    Where-Object { $null -ne $_.source } |   # 워크스페이스 크레이트 제외
    Sort-Object name, version

# ── 고지 파일 ──
$sb = New-Object System.Text.StringBuilder
[void]$sb.AppendLine("LeafViewer $version — 서드파티 라이선스 고지")
[void]$sb.AppendLine("LeafViewer 자체: MIT")
[void]$sb.AppendLine("")
[void]$sb.AppendLine("== 요약 ==")
[void]$sb.AppendLine(("{0,-28} {1,-12} {2}" -f 'PDFium (pdfium.dll)', '', 'BSD-3-Clause, Apache-2.0 — https://pdfium.googlesource.com/pdfium/'))
foreach ($d in $deps) {
    [void]$sb.AppendLine(("{0,-28} {1,-12} {2}" -f $d.name, $d.version, $d.license))
}
[void]$sb.AppendLine("")
[void]$sb.AppendLine("== PDFium ==")
if ($PdfiumLicense -and (Test-Path $PdfiumLicense)) {
    [void]$sb.AppendLine((Get-Content -Raw -Encoding UTF8 $PdfiumLicense))
} else {
    Write-Warning "PDFium LICENSE 원문이 없습니다 (-PdfiumLicense). 요약 줄만 넣습니다."
    [void]$sb.AppendLine("Copyright 2014 The PDFium Authors. BSD-3-Clause (일부 Apache-2.0).")
    [void]$sb.AppendLine("원문: https://pdfium.googlesource.com/pdfium/+/main/LICENSE")
    [void]$sb.AppendLine("바이너리: https://github.com/bblanchon/pdfium-binaries")
}
$missing = @()
foreach ($d in $deps) {
    $dir = Split-Path $d.manifest_path
    $files = Get-ChildItem $dir -File | Where-Object { $_.Name -match '^(LICEN[CS]E|COPYING|NOTICE|UNLICENSE)' } | Sort-Object Name
    [void]$sb.AppendLine("")
    [void]$sb.AppendLine("== $($d.name) $($d.version) ($($d.license)) ==")
    if ($d.repository) { [void]$sb.AppendLine($d.repository) }
    if (-not $files) { $missing += "$($d.name) $($d.version)"; continue }
    foreach ($f in $files) {
        [void]$sb.AppendLine("--- $($f.Name) ---")
        [void]$sb.AppendLine((Get-Content -Raw -Encoding UTF8 $f.FullName).TrimEnd())
    }
}
if ($missing) { Write-Warning ("라이선스 파일이 없는 크레이트: " + ($missing -join ', ')) }

$readme = @"
LeafViewer $version — 가벼운 문서 뷰어 (Windows 11 x64)

설치 없이 이 폴더째 원하는 곳에 두고 LeafViewer.exe를 실행한다.
  LeafViewer.exe [폴더|파일]   폴더를 트리로 열거나, 파일을 바로 연다
  지원 형식: PDF · MD · TXT · 이미지(PNG JPG GIF BMP WebP TIFF ICO)
  pdfium.dll은 PDF를 처음 열 때만 읽는다. exe와 같은 폴더에 둔다

주요 단축키
  Ctrl+O 폴더 열기 · Ctrl+F 찾기 · Ctrl+B 사이드바 · Alt+←/→ 뒤로/앞으로 · F5 새로 고침
  Ctrl+휠 / Ctrl +/- 확대 · Ctrl+0 맞춤 · Ctrl+1 실제 크기 (PDF·이미지)
  Ctrl+G 쪽/줄 이동 · Ctrl+U MD 원문 보기 · Alt+Z TXT 자동 줄바꿈 · Ctrl+, 설정 파일

파일 위치 (exe 폴더에는 아무것도 쓰지 않는다)
  설정  %APPDATA%\LeafViewer\config.toml   (저장하면 바로 반영)
  세션  %LOCALAPPDATA%\LeafViewer\session.toml
  로그  %LOCALAPPDATA%\LeafViewer\logs\

지우려면 이 폴더와 위 두 LeafViewer 폴더를 지우면 된다.
"@

# ── 묶기 ──
$name = "LeafViewer-$version-win-x64"
$dist = Join-Path $root 'dist'
$stage = Join-Path $dist $name
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $exe (Join-Path $stage 'LeafViewer.exe')
Copy-Item $Pdfium (Join-Path $stage 'pdfium.dll')
# 메모장에서도 한글이 깨지지 않게 BOM 있는 UTF-8, CRLF
$utf8bom = New-Object System.Text.UTF8Encoding $true
[IO.File]::WriteAllText((Join-Path $stage 'README.txt'), ($readme -replace "`r?`n", "`r`n"), $utf8bom)
[IO.File]::WriteAllText((Join-Path $stage 'THIRD-PARTY-NOTICES.txt'), ($sb.ToString() -replace "`r?`n", "`r`n"), $utf8bom)

$zip = Join-Path $dist "$name.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $stage -DestinationPath $zip
$exeMb = (Get-Item (Join-Path $stage 'LeafViewer.exe')).Length / 1MB
$dllMb = (Get-Item (Join-Path $stage 'pdfium.dll')).Length / 1MB
$zipMb = (Get-Item $zip).Length / 1MB
"크레이트 $($deps.Count)개 고지"
"LeafViewer.exe {0:N2}MB · pdfium.dll {1:N2}MB · zip {2:N2}MB" -f $exeMb, $dllMb, $zipMb
"saved: $zip"

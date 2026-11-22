# LeafViewer

지정한 폴더를 트리로 보여 주고, 그 안의 문서(.md, .txt, .pdf …)를 즉시 열람하는 Windows 전용 경량 문서 뷰어. Rust로 작성했다.
주 용도는 PDF 열람이며, MD·TXT는 보조 포맷이다.

자세한 내용은 [docs/SPEC.md](docs/SPEC.md)를 참고한다.

## 빌드

### 요구 사항

- Windows 10 이상 (x64)
- Rust (stable, 2024 에디션 지원 버전) + MSVC 툴체인(`x86_64-pc-windows-msvc`, Visual Studio Build Tools의 C++ 구성 요소)

### 빌드·실행

```powershell
cargo build --release
```

실행 파일은 `target\release\leafviewer.exe`에 생성된다. 정적 CRT로 링크하므로 VC++ 재배포 패키지는 필요 없다.

### PDF 보기 (pdfium.dll)

PDF를 열려면 `pdfium.dll`이 실행 파일 옆에 있어야 한다(첫 PDF를 열 때 로드). [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries)의 V8/XFA 미포함 win-x64 빌드를 받아 `target\release\`에 넣는다. 없어도 TXT·MD·이미지 보기는 동작한다.

### 테스트

```powershell
cargo test --workspace
```

### 배포용 zip 만들기

```powershell
.\tools\package.ps1
```

`dist\LeafViewer-<버전>-win-x64.zip`이 만들어진다. 자세한 내용은 [docs/SPEC.md §9](docs/SPEC.md)를 참고한다.

## 라이선스

이 프로젝트는 [MIT 라이선스](LICENSE)로 배포된다.

//! LeafViewer 프론트엔드: Win32 창·공용 컨트롤 + Direct2D/DirectWrite 렌더러.
//! 워크스페이스에서 `unsafe`가 허용되는 유일한 크레이트다 (SPEC §3.4).

mod app;
mod bar;
mod mdload;
mod mdview;
mod pdfview;
mod render;
mod textload;
mod textview;
mod tree;
mod util;

use std::path::PathBuf;

pub struct StartOptions {
    /// 명령줄 인자: 폴더면 루트로, 파일이면 상위 폴더를 루트로 열고 그 파일을 선택 (FR-A01)
    pub path: Option<PathBuf>,
    /// `pdfium.dll` 경로. 첫 PDF를 열 때만 로드한다.
    pub pdfium_path: PathBuf,
    /// `--bench-scroll`: 문서(PDF·TXT) 첫 화면이 뜨면 자동으로 끝까지 스크롤하며 프레임 시간을 기록하고 종료 (NFR-P07)
    pub bench_scroll: bool,
}

pub fn run(opts: StartOptions) -> windows::core::Result<()> {
    app::run(opts)
}

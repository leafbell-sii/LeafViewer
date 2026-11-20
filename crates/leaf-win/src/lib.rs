//! LeafViewer 프론트엔드: Win32 창·공용 컨트롤 + Direct2D/DirectWrite 렌더러.
//! 워크스페이스에서 `unsafe`가 허용되는 유일한 크레이트다 (SPEC §3.4).

mod app;
mod render;
mod tree;
mod util;

use std::path::PathBuf;

pub struct StartOptions {
    /// 명령줄 인자: 폴더면 루트로, 파일이면 상위 폴더를 루트로 열고 그 파일을 선택 (FR-A01)
    pub path: Option<PathBuf>,
    /// `pdfium.dll` 경로. 첫 PDF를 열 때만 로드한다.
    pub pdfium_path: PathBuf,
}

pub fn run(opts: StartOptions) -> windows::core::Result<()> {
    app::run(opts)
}

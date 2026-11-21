//! 전용 보기 포맷의 공통 경로 (SPEC §4.3 `OpenedDoc::Custom`).
//!
//! TXT·MD·PDF 보기는 앱(`app.rs`)이 직접 알고 있지만, 그 밖의 포맷은 이 trait 두 개만
//! 구현하면 앱 코드를 고치지 않고 붙는다 — 스크롤바·휠·키·마우스·찾기 막대 숨김·세션 위치·
//! 뒤로/앞으로·최소화 때 비트맵 해제·파일 감시 새로고침은 앱이 이 trait로 처리한다.
//! 등록은 `formats.rs`의 목록 한 줄이다.

use std::any::Any;
use std::path::{Path, PathBuf};

use leaf_core::config::Config;
use leaf_core::session::FilePos;
use leaf_doc::format::FormatInfo;

use crate::pdfview::CursorKind;
use crate::render::Target;

/// 워커에서 돌릴 작업. 결과는 `CustomView::on_result`로 돌아온다
pub type Job = Box<dyn FnOnce() -> Box<dyn Any + Send> + Send>;

pub enum CustomCmd {
    /// 워커에서 실행 (디코딩 등 — UI 스레드는 I/O를 기다리지 않는다, §4.2)
    Spawn(Job),
}

/// 열 때 보기에 넘기는 환경
#[derive(Clone)]
pub struct OpenCtx {
    /// 문서 영역 크기(DIP)와 DPI
    pub view: (f32, f32),
    pub dpi: f32,
    /// 비트맵 예산(바이트)
    pub bitmap_budget: usize,
}

impl OpenCtx {
    pub fn from_config(c: &Config, view: (f32, f32), dpi: f32) -> Self {
        Self {
            view,
            dpi,
            bitmap_budget: (c.pdf.bitmap_cache_mb.max(16) as usize) << 20,
        }
    }
}

/// 포맷 하나. `load`는 워커, `create`는 UI 스레드에서 부른다.
pub trait CustomFormat: Sync {
    fn info(&self) -> &'static FormatInfo;
    /// 파일을 읽어 첫 화면에 필요한 것까지 준비한다 (워커). 실패하면 오류 화면에 보일 문구
    fn load(&self, path: &Path, ctx: &OpenCtx) -> Result<Box<dyn Any + Send>, String>;
    fn create(
        &self,
        generation: u64,
        path: PathBuf,
        loaded: Box<dyn Any + Send>,
        ctx: &OpenCtx,
    ) -> Box<dyn CustomView>;
}

/// 열린 문서 하나. 좌표는 DIP(문서 영역 왼쪽 위 원점)
pub trait CustomView {
    fn path(&self) -> &Path;
    fn generation(&self) -> u64;
    /// 세션 `FilePos.view`에 남길 이름 — 같은 파일을 다른 보기로 연 위치와 섞이지 않게
    fn view_key(&self) -> &'static str;
    /// 상태 표시줄 왼쪽: "이름 · 형식 · 크기"
    fn info(&self) -> String;
    /// 상태 표시줄 오른쪽: 배율 등
    fn status(&self) -> String;

    fn set_view(&mut self, vw: f32, vh: f32, dpi: f32);
    /// 배경은 앱이 지운다. 필요한 워커 작업을 돌려준다
    fn paint(&mut self, t: &Target) -> Vec<CustomCmd>;
    /// `Spawn`한 작업의 결과
    fn on_result(&mut self, t: &Target, result: Box<dyn Any + Send>);
    /// 첫 화면을 그렸는지 (측정·벤치마크)
    fn first_paint_done(&self) -> bool;

    /// ((전체 폭, 창 폭, 위치), (전체 높이, 창 높이, 위치))
    fn scroll_metrics(&self) -> ((f32, f32, f32), (f32, f32, f32));
    fn scroll_to(&mut self, x: Option<f32>, y: Option<f32>);
    fn scroll_by(&mut self, dx: f32, dy: f32);
    fn at_end(&self) -> bool;
    /// `at`: 고정할 화면 점. 없으면 가운데
    fn zoom_by(&mut self, factor: f32, at: Option<(f32, f32)>);
    /// 처리했으면 true
    fn on_key(&mut self, vk: u16, ctrl: bool, shift: bool) -> bool;
    fn on_mouse_down(&mut self, _x: f32, _y: f32) {}
    fn on_mouse_move(&mut self, _x: f32, _y: f32, _dragging: bool) -> CursorKind {
        CursorKind::Arrow
    }
    fn on_mouse_up(&mut self) {}

    fn position(&self) -> FilePos;
    fn restore(&mut self, pos: &FilePos);
    /// 창 최소화: 비트맵을 비운다 (§4.5)
    fn release(&mut self);
    /// (비트맵 수, 바이트) — 벤치마크 기록용
    fn bitmap_stats(&self) -> (u32, usize) {
        (0, 0)
    }
}

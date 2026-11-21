//! TXT 보기: 행 가상화·자동 줄바꿈·확대·찾기·선택/복사 (FR-T01~T06, FR-V03~V05).
//!
//! 스크롤 위치는 (화면 맨 위 행의 시작 바이트, 그 행 안에서 잘린 높이)로 나타낸다. 문서 전체 높이를
//! 알 필요가 없으므로 1GB 파일도 첫 화면을 바로 그린다. 스크롤바는 줄 색인이 끝나면 줄 단위,
//! 그 전에는 바이트 비율로 근사한다 (FR-T05).
//!
//! 이 모듈은 Win32 메시지를 보내지 않는다. 워커 작업은 `SearchJob`·`CopyJob`으로 돌려준다.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use leaf_core::lru::BudgetLru;
use leaf_core::session::FilePos;
use leaf_fmt_text::search::Hit;
use leaf_fmt_text::{LineIndex, Pos, TextDoc};
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::DirectWrite::*;
use windows::core::{BOOL, Result, w};

use crate::pdfview::CursorKind;
use crate::render::{self, Target};

pub const MARGIN: f32 = 16.0;
const PAD_Y: f32 = 8.0;
/// FR-T04: 고정폭 폰트(Consolas), 한글은 DirectWrite 대체 폰트
const FONT_PT: f32 = 11.0;
/// FR-T10 기본값
const TAB_WIDTH: f32 = 4.0;
const ZOOM_MIN: f32 = 0.5;
const ZOOM_MAX: f32 = 4.0;
/// 레이아웃 캐시 예산 (UTF-16 단위 수)
/// 화면 몇 장 분량이면 충분하다(짧은 줄 약 2천 개, 4KB 행 30여 개). 크면 DWrite 레이아웃 메모리만 늘어난다.
const LAYOUT_BUDGET: usize = 128 << 10;
pub const MAX_HITS: usize = 100_000;
/// 이보다 큰 선택은 워커에서 복사
const COPY_SYNC_BYTES: usize = 2 << 20;
/// 복사 상한 (클립보드에 GB 단위 텍스트를 넣지 않는다)
pub const COPY_MAX_BYTES: usize = 64 << 20;

struct RowLayout {
    layout: IDWriteTextLayout,
    h: f32,
    w: f32,
    len: u32,
    next: Option<usize>,
    newline: bool,
}

struct FrameRow {
    start: usize,
    top: f32,
    h: f32,
    text_y: f32,
    len: u32,
}

struct TextSearch {
    id: u64,
    query: String,
    hits: Vec<Hit>,
    current: Option<usize>,
    done: bool,
    capped: bool,
    progress: f32,
    cancel: Arc<AtomicBool>,
}

pub struct SearchJob {
    pub id: u64,
    pub doc: Arc<TextDoc>,
    pub query: String,
    pub cancel: Arc<AtomicBool>,
}

pub enum CopyJob {
    Ready(String),
    Background { doc: Arc<TextDoc>, a: Pos, b: Pos },
    TooLarge(usize),
}

pub struct TextView {
    pub generation: u64,
    pub path: PathBuf,
    pub doc: Arc<TextDoc>,
    pub size: u64,
    index: Option<Arc<LineIndex>>,
    index_progress: f32,
    /// 줄 색인·찾기 작업 취소 (문서를 떠날 때)
    pub cancel: Arc<AtomicBool>,
    dwrite: IDWriteFactory,
    format: Option<IDWriteTextFormat>,
    space_w: f32,
    zoom: f32,
    wrap: bool,
    anchor: usize,
    y_off: f32,
    sx: f32,
    vw: f32,
    vh: f32,
    layouts: BudgetLru<usize, RowLayout>,
    layout_w: f32,
    frame: Vec<FrameRow>,
    max_w: f32,
    search: Option<TextSearch>,
    next_search_id: u64,
    selection: Option<(Pos, Pos)>,
    line_cache: Option<(usize, u64)>,
    opened_at: Instant,
    pub first_paint_ms: Option<f64>,
}

impl TextView {
    pub fn new(
        generation: u64,
        path: PathBuf,
        doc: Arc<TextDoc>,
        size: u64,
        index: Option<LineIndex>,
        dwrite: IDWriteFactory,
        opened_at: Instant,
    ) -> Self {
        let anchor = doc.start();
        Self {
            generation,
            path,
            doc,
            size,
            index: index.map(Arc::new),
            index_progress: 0.0,
            cancel: Arc::new(AtomicBool::new(false)),
            dwrite,
            format: None,
            space_w: 8.0,
            zoom: 1.0,
            wrap: true,
            anchor,
            y_off: 0.0,
            sx: 0.0,
            vw: 0.0,
            vh: 0.0,
            layouts: BudgetLru::new(LAYOUT_BUDGET),
            layout_w: 0.0,
            frame: Vec::new(),
            max_w: 0.0,
            search: None,
            next_search_id: 1,
            selection: None,
            line_cache: None,
            opened_at,
            first_paint_ms: None,
        }
    }

    pub fn set_index(&mut self, ix: LineIndex) {
        self.index = Some(Arc::new(ix));
        self.line_cache = None;
    }

    pub fn set_index_progress(&mut self, f: f32) {
        self.index_progress = f;
    }

    /// 문서를 떠날 때: 진행 중인 색인·찾기 중단
    pub fn cancel_all(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(s) = &self.search {
            s.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// 최소화 시 레이아웃 캐시 비움 (SPEC §4.5)
    pub fn release(&mut self) {
        self.layouts.clear();
    }

    // ───── 서식 · 레이아웃 ─────

    fn ensure_format(&mut self) -> Result<IDWriteTextFormat> {
        if let Some(f) = &self.format {
            return Ok(f.clone());
        }
        unsafe {
            let f = self.dwrite.CreateTextFormat(
                w!("Consolas"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                FONT_PT * 96.0 / 72.0 * self.zoom,
                w!("ko-KR"),
            )?;
            f.SetWordWrapping(if self.wrap {
                DWRITE_WORD_WRAPPING_WRAP
            } else {
                DWRITE_WORD_WRAPPING_NO_WRAP
            })?;
            let sp = self
                .dwrite
                .CreateTextLayout(&[b' ' as u16], &f, 1000.0, 1000.0)?;
            let mut m = DWRITE_TEXT_METRICS::default();
            sp.GetMetrics(&mut m)?;
            self.space_w = m.widthIncludingTrailingWhitespace.max(1.0);
            f.SetIncrementalTabStop(self.space_w * TAB_WIDTH)?;
            self.format = Some(f.clone());
            self.layouts.clear();
            Ok(f)
        }
    }

    fn line_h(&self) -> f32 {
        FONT_PT * 96.0 / 72.0 * self.zoom * 1.25
    }

    fn make_layout(&mut self, pos: usize) -> Result<()> {
        if self.layouts.contains(&pos) {
            return Ok(());
        }
        let f = self.ensure_format()?;
        let (row, text) = self.doc.decode_row(pos);
        let layout = unsafe {
            self.dwrite
                .CreateTextLayout(&text, &f, self.layout_w.max(50.0), f32::MAX)?
        };
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe { layout.GetMetrics(&mut m)? };
        let cost = text.len().max(64);
        self.layouts.insert(
            pos,
            RowLayout {
                layout,
                h: m.height,
                w: m.widthIncludingTrailingWhitespace,
                len: text.len() as u32,
                next: row.next,
                newline: row.has_newline(),
            },
            cost,
            &[],
        );
        Ok(())
    }

    fn row_info(&mut self, pos: usize) -> Option<(f32, Option<usize>)> {
        self.make_layout(pos).ok()?;
        let l = self.layouts.peek(&pos)?;
        Some((l.h, l.next))
    }

    /// 행이 차지하는 높이 (문서 첫 행·마지막 행은 위아래 여백 포함)
    fn vis_h(&mut self, pos: usize) -> f32 {
        let (h, next) = self.row_info(pos).unwrap_or((self.line_h(), None));
        let top = if pos == self.doc.start() { PAD_Y } else { 0.0 };
        let bottom = if next.is_none() { PAD_Y } else { 0.0 };
        h + top + bottom
    }

    fn next_of(&mut self, pos: usize) -> Option<usize> {
        match self.row_info(pos) {
            Some((_, n)) => n,
            None => self.doc.next_row(pos),
        }
    }

    // ───── 스크롤 ─────

    /// 아래로 `d` DIP (음수면 위로). 문서 끝에서는 마지막 행이 화면 아래에 붙도록 맞춘다.
    pub fn scroll_px(&mut self, d: f32) {
        self.y_off += d;
        self.rebalance();
        self.clamp_bottom();
    }

    fn rebalance(&mut self) {
        while self.y_off < 0.0 {
            match self.doc.prev_row(self.anchor) {
                Some(p) => {
                    self.anchor = p;
                    self.y_off += self.vis_h(p);
                }
                None => {
                    self.y_off = 0.0;
                    break;
                }
            }
        }
        loop {
            let h = self.vis_h(self.anchor);
            if self.y_off < h {
                break;
            }
            match self.next_of(self.anchor) {
                Some(n) => {
                    self.y_off -= h;
                    self.anchor = n;
                }
                None => {
                    self.y_off = (h - 1.0).max(0.0);
                    break;
                }
            }
        }
    }

    fn clamp_bottom(&mut self) {
        if self.vh <= 0.0 {
            return;
        }
        let mut sum = 0.0;
        let mut p = Some(self.anchor);
        while let Some(r) = p {
            sum += self.vis_h(r);
            if sum - self.y_off >= self.vh {
                return;
            }
            p = self.next_of(r);
        }
        let gap = self.vh - (sum - self.y_off);
        if gap > 0.0 {
            self.y_off -= gap;
            while self.y_off < 0.0 {
                match self.doc.prev_row(self.anchor) {
                    Some(p) => {
                        self.anchor = p;
                        self.y_off += self.vis_h(p);
                    }
                    None => {
                        self.y_off = 0.0;
                        break;
                    }
                }
            }
        }
    }

    pub fn scroll_x(&mut self, dx: f32) {
        if !self.wrap {
            self.sx = (self.sx + dx).clamp(0.0, (self.max_w + 2.0 * MARGIN - self.vw).max(0.0));
        }
    }

    pub fn set_sx(&mut self, x: f32) {
        self.sx = 0.0;
        self.scroll_x(x);
    }

    /// 마지막 행이 화면 안에 다 보이는지 (스크롤 벤치마크 종료 조건)
    pub fn at_end(&self) -> bool {
        self.frame.last().is_some_and(|f| {
            f.top + f.h <= self.vh + 0.5
                && self
                    .layouts
                    .peek(&f.start)
                    .is_some_and(|l| l.next.is_none())
        })
    }

    pub fn home(&mut self) {
        self.anchor = self.doc.start();
        self.y_off = 0.0;
        self.sx = 0.0;
    }

    pub fn end(&mut self) {
        self.anchor = self.doc.last_row();
        self.y_off = 0.0;
        self.scroll_px(f32::MAX / 4.0);
    }

    fn line_of_anchor(&mut self) -> Option<u64> {
        let ix = self.index.clone()?;
        match self.line_cache {
            Some((a, l)) if a == self.anchor => Some(l),
            _ => {
                let l = ix.line_of(&self.doc, self.anchor);
                self.line_cache = Some((self.anchor, l));
                Some(l)
            }
        }
    }

    /// (가로 전체·페이지·위치, 세로 전체·페이지·위치). 세로는 줄 단위(색인 후) 또는 바이트 단위.
    pub fn scroll_metrics(&mut self) -> ((f64, f64, f64), (f64, f64, f64)) {
        let h = if self.wrap {
            (0.0, 0.0, 0.0)
        } else {
            (
                (self.max_w + 2.0 * MARGIN) as f64,
                self.vw as f64,
                self.sx as f64,
            )
        };
        let first_h = self.frame.first().map_or(1.0, |r| r.h.max(1.0));
        let frac = (self.y_off / first_h).clamp(0.0, 1.0) as f64;
        let rows = self
            .frame
            .iter()
            .filter(|r| r.top >= 0.0 && r.top + r.h <= self.vh + 0.5)
            .count()
            .max(1) as f64;
        let v = match (self.line_of_anchor(), &self.index) {
            (Some(line), Some(ix)) => (ix.lines as f64, rows, line as f64 + frac),
            _ => {
                let s = self.doc.start();
                let total = (self.doc.len() - s).max(1) as f64;
                let span = match (self.frame.first(), self.frame.last()) {
                    (Some(a), Some(b)) => (b.start - a.start).max(1) as f64,
                    _ => 1.0,
                };
                (total, span.min(total), (self.anchor - s) as f64)
            }
        };
        (h, v)
    }

    /// 스크롤바 끌기: 줄 번호(색인 후) 또는 바이트 위치로 이동
    pub fn scroll_to_unit(&mut self, v: f64) {
        let v = v.max(0.0);
        self.anchor = match &self.index {
            Some(ix) => ix.line_start(&self.doc, v as u64),
            None => self.doc.row_containing(self.doc.start() + v as usize),
        };
        self.y_off = 0.0;
        self.scroll_px(0.0);
    }

    /// Ctrl+G (FR-T09). 색인 전이면 false.
    pub fn go_line(&mut self, line1: u64) -> bool {
        let Some(ix) = &self.index else { return false };
        self.anchor = ix.line_start(&self.doc, line1.saturating_sub(1));
        self.y_off = 0.0;
        self.scroll_px(0.0);
        true
    }

    // ───── 배율 · 줄바꿈 ─────

    pub fn zoom_by(&mut self, f: f32) {
        self.set_zoom(self.zoom * f);
    }

    pub fn set_zoom(&mut self, z: f32) {
        let z = z.clamp(ZOOM_MIN, ZOOM_MAX);
        if (z - self.zoom).abs() < 1e-4 {
            return;
        }
        self.y_off *= z / self.zoom;
        self.sx *= z / self.zoom;
        self.zoom = z;
        self.format = None;
        self.layouts.clear();
        self.max_w = 0.0;
    }

    /// FR-T03 자동 줄바꿈 토글
    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        self.format = None;
        self.layouts.clear();
        self.sx = 0.0;
        self.max_w = 0.0;
        self.y_off = 0.0;
    }

    // ───── 세션 ─────

    pub fn position(&self) -> FilePos {
        FilePos {
            path: self.path.clone(),
            page: 0,
            y_pt: self.y_off,
            zoom: format!("{:.4}", self.zoom),
            offset: self.anchor as u64,
            view: "txt".into(),
        }
    }

    pub fn restore(&mut self, pos: &FilePos) {
        if !(pos.view.is_empty() || pos.view == "txt") {
            return;
        }
        if let Ok(z) = pos.zoom.parse::<f32>() {
            self.zoom = z.clamp(ZOOM_MIN, ZOOM_MAX);
        }
        self.anchor = self.doc.row_containing(pos.offset as usize);
        self.y_off = pos.y_pt.max(0.0);
    }

    // ───── 상태 ─────

    pub fn status(&mut self) -> String {
        let line = match (self.line_of_anchor(), &self.index) {
            (Some(l), Some(ix)) => format!("줄 {} / {}", l + 1, ix.lines),
            _ => format!("줄 색인 중 {:.0}%", self.index_progress * 100.0),
        };
        let mut s = format!(
            "{line} · {:.0}% · 줄바꿈 {}",
            self.zoom * 100.0,
            if self.wrap { "켬" } else { "끔" }
        );
        if let Some(f) = self.search_status() {
            s.push_str(" · ");
            s.push_str(&f);
        }
        s
    }

    // ───── 찾기 (FR-V04) ─────

    pub fn search_query(&self) -> Option<&str> {
        self.search.as_ref().map(|s| s.query.as_str())
    }

    pub fn start_search(&mut self, query: &str) -> Option<SearchJob> {
        self.clear_search();
        let query = query.trim();
        if query.is_empty() {
            return None;
        }
        let id = self.next_search_id;
        self.next_search_id += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        self.search = Some(TextSearch {
            id,
            query: query.to_string(),
            hits: Vec::new(),
            current: None,
            done: false,
            capped: false,
            progress: 0.0,
            cancel: cancel.clone(),
        });
        Some(SearchJob {
            id,
            doc: self.doc.clone(),
            query: query.to_string(),
            cancel,
        })
    }

    pub fn clear_search(&mut self) {
        if let Some(s) = self.search.take() {
            s.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn on_search_hits(
        &mut self,
        id: u64,
        hits: Vec<Hit>,
        done_bytes: usize,
        finished: bool,
        capped: bool,
    ) {
        let len = self.doc.len().max(1);
        let anchor = self.anchor;
        let Some(s) = self.search.as_mut().filter(|s| s.id == id) else {
            return;
        };
        s.hits.extend(hits);
        s.progress = done_bytes as f32 / len as f32;
        s.done |= finished;
        s.capped |= capped;
        if s.current.is_none() && !s.hits.is_empty() {
            let i = s.hits.partition_point(|h| h.row < anchor);
            if i < s.hits.len() {
                s.current = Some(i);
            } else if s.done {
                s.current = Some(0);
            }
            if s.current.is_some() {
                self.reveal_current();
            }
        }
    }

    pub fn search_status(&self) -> Option<String> {
        let s = self.search.as_ref()?;
        let total = if s.capped {
            format!("{}+", s.hits.len())
        } else {
            s.hits.len().to_string()
        };
        Some(match s.current {
            _ if s.hits.is_empty() && s.done => "찾기: 없음".to_string(),
            Some(i) if s.done => format!("찾기 {}/{total}", i + 1),
            Some(i) => format!("찾기 {}/{total}… {:.0}%", i + 1, s.progress * 100.0),
            None => format!("찾는 중… {total} ({:.0}%)", s.progress * 100.0),
        })
    }

    pub fn find_step(&mut self, forward: bool) {
        let anchor = self.anchor;
        let Some(s) = self.search.as_mut() else {
            return;
        };
        let n = s.hits.len();
        if n == 0 {
            return;
        }
        s.current = Some(match s.current {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None => s.hits.partition_point(|h| h.row < anchor) % n,
        });
        self.reveal_current();
    }

    /// 현재 일치 항목이 화면에 없으면 화면 위쪽 1/3에 오도록 스크롤
    fn reveal_current(&mut self) {
        let Some((row, start)) = self
            .search
            .as_ref()
            .and_then(|s| s.current.map(|i| (s.hits[i].row, s.hits[i].range.start)))
        else {
            return;
        };
        if self.make_layout(row).is_err() {
            return;
        }
        let Some(l) = self.layouts.peek(&row) else {
            return;
        };
        let (mut x, mut y) = (0.0f32, 0.0f32);
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        if unsafe {
            l.layout
                .HitTestTextPosition(start, false, &mut x, &mut y, &mut m)
        }
        .is_err()
        {
            return;
        }
        let pad = if row == self.doc.start() { PAD_Y } else { 0.0 };
        let visible = self.frame.iter().any(|f| {
            let top = f.text_y + y;
            f.start == row && top >= 0.0 && top + m.height <= self.vh
        });
        if !visible {
            self.anchor = row;
            self.y_off = pad + y - self.vh / 3.0;
            self.rebalance();
            self.clamp_bottom();
        }
        if !self.wrap && (x < self.sx || x + m.width > self.sx + self.vw - 2.0 * MARGIN) {
            self.set_sx(x - self.vw / 3.0);
        }
    }

    // ───── 선택 · 복사 (FR-V05) ─────

    fn hit_test(&mut self, x: f32, y: f32) -> Option<Pos> {
        let i = match self.frame.iter().position(|f| y < f.top + f.h) {
            Some(i) => i,
            None => self.frame.len().checked_sub(1)?,
        };
        let (start, text_y, len) = {
            let f = &self.frame[i];
            (f.start, f.text_y, f.len)
        };
        let l = self.layouts.peek(&start)?;
        let mut trailing = BOOL(0);
        let mut inside = BOOL(0);
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        unsafe {
            l.layout
                .HitTestPoint(
                    x - (MARGIN - self.sx),
                    y - text_y,
                    &mut trailing,
                    &mut inside,
                    &mut m,
                )
                .ok()?;
        }
        let idx = m.textPosition + if trailing.as_bool() { m.length } else { 0 };
        Some(Pos {
            row: start,
            idx: idx.min(len),
        })
    }

    pub fn on_mouse_down(&mut self, x: f32, y: f32, shift: bool) {
        let Some(p) = self.hit_test(x, y) else { return };
        self.selection = match (shift, self.selection) {
            (true, Some((a, _))) => Some((a, p)),
            _ => Some((p, p)),
        };
    }

    pub fn on_mouse_move(&mut self, x: f32, y: f32, dragging: bool) -> CursorKind {
        if dragging && self.selection.is_some() {
            // 화면 밖으로 끌면 그쪽으로 스크롤
            if y < 0.0 {
                self.scroll_px(y.max(-60.0));
            } else if y > self.vh {
                self.scroll_px((y - self.vh).min(60.0));
            }
            if !self.wrap {
                if x < 0.0 {
                    self.scroll_x(x.max(-60.0));
                } else if x > self.vw {
                    self.scroll_x((x - self.vw).min(60.0));
                }
            }
            if let (Some((a, _)), Some(p)) = (
                self.selection,
                self.hit_test(x, y.clamp(0.0, self.vh - 1.0)),
            ) {
                self.selection = Some((a, p));
            }
        }
        CursorKind::IBeam
    }

    pub fn select_all(&mut self) {
        let last = self.doc.last_row();
        let (_, t) = self.doc.decode_row(last);
        self.selection = Some((
            Pos {
                row: self.doc.start(),
                idx: 0,
            },
            Pos {
                row: last,
                idx: t.len() as u32,
            },
        ));
    }

    pub fn copy(&self) -> Option<CopyJob> {
        let (a, b) = self.selection?;
        if a == b {
            return None;
        }
        let span = a.row.abs_diff(b.row);
        Some(if span > COPY_MAX_BYTES {
            CopyJob::TooLarge(span)
        } else if span > COPY_SYNC_BYTES {
            CopyJob::Background {
                doc: self.doc.clone(),
                a,
                b,
            }
        } else {
            match self.doc.extract(a, b, COPY_MAX_BYTES) {
                Ok(s) => CopyJob::Ready(s),
                Err(n) => CopyJob::TooLarge(n),
            }
        })
    }

    // ───── 키보드 ─────

    pub fn on_key(&mut self, vk: u16, shift: bool) -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        let page = (self.vh - self.line_h()).max(self.line_h());
        match VIRTUAL_KEY(vk) {
            VK_DOWN => self.scroll_px(self.line_h()),
            VK_UP => self.scroll_px(-self.line_h()),
            VK_NEXT => self.scroll_px(page),
            VK_PRIOR => self.scroll_px(-page),
            VK_SPACE => self.scroll_px(if shift { -page } else { page }),
            VK_RIGHT => self.scroll_x(self.space_w * 4.0),
            VK_LEFT => self.scroll_x(-self.space_w * 4.0),
            VK_HOME => self.home(),
            VK_END => self.end(),
            VK_ESCAPE => self.selection = None,
            _ => return false,
        }
        true
    }

    // ───── 그리기 ─────

    pub fn set_view(&mut self, vw: f32, vh: f32) {
        self.vw = vw;
        self.vh = vh;
    }

    pub fn view_size(&self) -> (f32, f32) {
        (self.vw, self.vh)
    }

    pub fn paint(&mut self, t: &Target) -> Result<()> {
        self.ensure_format()?;
        let lw = (self.vw - 2.0 * MARGIN).max(50.0);
        if (lw - self.layout_w).abs() > 0.5 {
            self.layout_w = lw;
            if self.wrap {
                self.layouts.clear();
            }
        }
        self.rebalance();
        self.clamp_bottom();
        self.scroll_x(0.0);

        let sel = self
            .selection
            .map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
        let x0 = MARGIN - self.sx;
        self.frame.clear();
        let mut y = -self.y_off;
        let mut p = Some(self.anchor);
        while let Some(rs) = p {
            if y >= self.vh {
                break;
            }
            self.make_layout(rs)?;
            let vis = self.vis_h(rs);
            let Some(l) = self.layouts.peek(&rs) else {
                break;
            };
            let (layout, len, newline, next, w) = (l.layout.clone(), l.len, l.newline, l.next, l.w);
            let text_y = y + if rs == self.doc.start() { PAD_Y } else { 0.0 };

            // 선택 영역
            if let Some((a, b)) = sel
                && a.row <= rs
                && rs <= b.row
                && a != b
            {
                let s = if a.row == rs { a.idx } else { 0 };
                let e = if b.row == rs { b.idx } else { len };
                for r in range_rects(&layout, s, e.saturating_sub(s), x0, text_y) {
                    unsafe { t.rt.FillRectangle(&r, &t.selection) };
                }
                // 줄바꿈까지 선택됐으면 줄 끝에 작은 표시
                if rs < b.row && newline {
                    let (mut px, mut py) = (0.0f32, 0.0f32);
                    let mut m = DWRITE_HIT_TEST_METRICS::default();
                    if unsafe { layout.HitTestTextPosition(len, false, &mut px, &mut py, &mut m) }
                        .is_ok()
                    {
                        let r = render::rect(
                            x0 + px,
                            text_y + py,
                            x0 + px + self.space_w * 0.6,
                            text_y + py + m.height,
                        );
                        unsafe { t.rt.FillRectangle(&r, &t.selection) };
                    }
                }
            }
            // 찾기 일치 항목
            if let Some(s) = &self.search {
                let mut i = s.hits.partition_point(|h| h.row < rs);
                while let Some(h) = s.hits.get(i).filter(|h| h.row == rs) {
                    let brush = if s.current == Some(i) {
                        &t.hit_current
                    } else {
                        &t.hit
                    };
                    for r in range_rects(
                        &layout,
                        h.range.start,
                        h.range.end - h.range.start,
                        x0,
                        text_y,
                    ) {
                        unsafe { t.rt.FillRectangle(&r, brush) };
                    }
                    i += 1;
                }
            }
            render::draw_layout(t, &layout, x0, text_y, &t.text);

            self.max_w = self.max_w.max(w);
            self.frame.push(FrameRow {
                start: rs,
                top: y,
                h: vis,
                text_y,
                len,
            });
            y += vis;
            p = next;
        }
        if self.first_paint_ms.is_none() {
            self.first_paint_ms = Some(self.opened_at.elapsed().as_secs_f64() * 1000.0);
        }
        Ok(())
    }
}

/// 텍스트 범위의 줄 단위 사각형 (DIP)
pub(crate) fn range_rects(
    layout: &IDWriteTextLayout,
    start: u32,
    len: u32,
    x0: f32,
    y0: f32,
) -> Vec<D2D_RECT_F> {
    if len == 0 {
        return Vec::new();
    }
    let mut buf = vec![DWRITE_HIT_TEST_METRICS::default(); 8];
    let mut n = 0u32;
    unsafe {
        if layout
            .HitTestTextRange(start, len, x0, y0, Some(&mut buf), &mut n)
            .is_err()
        {
            if n as usize <= buf.len() {
                return Vec::new();
            }
            buf.resize(n as usize, DWRITE_HIT_TEST_METRICS::default());
            if layout
                .HitTestTextRange(start, len, x0, y0, Some(&mut buf), &mut n)
                .is_err()
            {
                return Vec::new();
            }
        }
    }
    buf.truncate(n as usize);
    buf.iter()
        .map(|m| render::rect(m.left, m.top, m.left + m.width, m.top + m.height))
        .collect()
}

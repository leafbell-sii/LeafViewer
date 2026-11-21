//! PDF 보기 상태: 연속 스크롤·배율·타일 캐시·찾기·선택·링크 (FR-P01~P07).
//!
//! 이 모듈은 Win32 메시지를 보내지 않는다. 엔진에 보낼 요청은 `Cmd`로 돌려주고
//! 호출자(app)가 빌림을 놓은 뒤 처리한다 (재진입 규칙).

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::PathBuf;
use std::time::Instant;

use leaf_core::lru::BudgetLru;
use leaf_core::perf;
use leaf_core::session::FilePos;
use leaf_doc::paged::{self, PageLayout, TileKey, TileSpec, ZoomMode};
use leaf_doc::text::PageText;
use leaf_doc::{Dest, Link, LinkTarget, OutlineItem, RectF};
use leaf_fmt_pdf::TextPos;
use windows::Win32::Graphics::Direct2D::{D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, ID2D1Bitmap};

use crate::render::{self, Target, rgb};

/// 쪽 텍스트·링크 캐시 크기(쪽 수)
const TEXT_CACHE_PAGES: usize = 64;
const BITMAP_BUDGET_MIN: usize = 64 * 1024 * 1024;
const LINE_STEP: f32 = 40.0;
const CLICK_SLOP: f32 = 4.0;

pub enum Cmd {
    Render(Vec<TileSpec>),
    PageInfo(Vec<u32>),
    Search { id: u64, query: String, start: u32 },
    CancelSearch,
    Copy { from: TextPos, to: TextPos },
    SetClipboard(String),
    OpenUri(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CursorKind {
    Arrow,
    IBeam,
    Hand,
}

struct Tile {
    bmp: ID2D1Bitmap,
}

struct PageData {
    text: PageText,
    links: Vec<Link>,
}

/// 일치 항목: (글자 범위, 줄 단위 사각형 pt)
type Hit = (Range<usize>, Vec<RectF>);

#[derive(Default)]
struct Search {
    id: u64,
    query: String,
    /// 쪽 → (글자 범위, 사각형들)
    hits: BTreeMap<u32, Vec<Hit>>,
    total: usize,
    current: Option<(u32, usize)>,
    done: bool,
}

struct Press {
    x: f32,
    y: f32,
    link: Option<LinkTarget>,
    moved: bool,
}

pub struct PdfView {
    pub generation: u64,
    pub path: PathBuf,
    pub outline: Vec<OutlineItem>,
    layout: PageLayout,
    zoom_mode: ZoomMode,
    sx: f32,
    sy: f32,
    vw: f32,
    vh: f32,
    dpi: f32,
    epoch: u64,
    tiles: BudgetLru<TileKey, Tile>,
    visible_keys: Vec<TileKey>,
    sent_tiles: Vec<TileSpec>,
    pages: BudgetLru<u32, PageData>,
    sent_info: Vec<u32>,
    search: Option<Search>,
    next_search_id: u64,
    selection: Option<(TextPos, TextPos)>,
    press: Option<Press>,
    // 측정
    opened_at: Instant,
    pub timings: String,
    first_page_logged: bool,
    /// 마지막 프레임에서 보이는 쪽이 모두 고해상도였는지 (벤치마크용)
    pub last_frame_complete: bool,
    render_ms_total: f64,
    tiles_rendered: u32,
}

fn order(a: TextPos, b: TextPos) -> (TextPos, TextPos) {
    if a <= b { (a, b) } else { (b, a) }
}

impl PdfView {
    pub fn new(
        generation: u64,
        path: PathBuf,
        sizes: Vec<(f32, f32)>,
        outline: Vec<OutlineItem>,
        opened_at: Instant,
        timings: String,
    ) -> Self {
        Self {
            generation,
            path,
            outline,
            layout: PageLayout::new(sizes),
            zoom_mode: ZoomMode::FitWidth,
            sx: 0.0,
            sy: 0.0,
            vw: 0.0,
            vh: 0.0,
            dpi: 96.0,
            epoch: 0,
            tiles: BudgetLru::new(BITMAP_BUDGET_MIN),
            visible_keys: Vec::new(),
            sent_tiles: Vec::new(),
            pages: BudgetLru::new(TEXT_CACHE_PAGES),
            sent_info: Vec::new(),
            search: None,
            next_search_id: 1,
            selection: None,
            press: None,
            opened_at,
            timings,
            first_page_logged: false,
            last_frame_complete: false,
            render_ms_total: 0.0,
            tiles_rendered: 0,
        }
    }

    pub fn page_count(&self) -> usize {
        self.layout.page_count()
    }

    // ───── 배치 · 스크롤 · 배율 ─────

    /// 보기 크기(DIP)·DPI 갱신. 맞춤 배율이면 다시 계산하고, 화면 맨 위 내용을 고정한다.
    pub fn set_view(&mut self, vw: f32, vh: f32, dpi: f32) {
        if (vw - self.vw).abs() < 0.5
            && (vh - self.vh).abs() < 0.5
            && dpi == self.dpi
            && self.vw > 0.0
        {
            return;
        }
        let first = self.vw == 0.0;
        self.vw = vw;
        self.vh = vh;
        if dpi != self.dpi {
            self.dpi = dpi;
            self.drop_bitmaps();
        }
        let z = self
            .layout
            .resolve_zoom(self.zoom_mode, vw, vh, self.current_page());
        if first {
            self.layout.set(z, vw);
        } else {
            self.relayout(z, (0.0, 0.0));
        }
        // 비트맵 예산 = max(64MB, 뷰포트 바이트 × 3) (SPEC §4.5)
        let k = dpi / 96.0;
        let view_bytes = (vw * k * vh * k * 4.0) as usize;
        self.tiles.set_budget(BITMAP_BUDGET_MIN.max(view_bytes * 3));
        self.clamp();
    }

    fn relayout(&mut self, zoom: f32, at: (f32, f32)) {
        let a = self.layout.anchor(self.sx + at.0, self.sy + at.1);
        self.layout.set(zoom, self.vw);
        let (x, y) = self.layout.doc_pos(a);
        self.sx = x - at.0;
        self.sy = y - at.1;
        self.clamp();
    }

    fn clamp(&mut self) {
        let (tw, th) = self.layout.total_size();
        self.sx = self.sx.clamp(0.0, (tw - self.vw).max(0.0));
        self.sy = self.sy.clamp(0.0, (th - self.vh).max(0.0));
    }

    pub fn scroll_metrics(&self) -> ((f32, f32, f32), (f32, f32, f32)) {
        let (tw, th) = self.layout.total_size();
        ((tw, self.vw, self.sx), (th, self.vh, self.sy))
    }

    pub fn scroll_to(&mut self, x: Option<f32>, y: Option<f32>) {
        if let Some(x) = x {
            self.sx = x;
        }
        if let Some(y) = y {
            self.sy = y;
        }
        self.clamp();
    }

    pub fn scroll_by(&mut self, dx: f32, dy: f32) {
        self.scroll_to(Some(self.sx + dx), Some(self.sy + dy));
    }

    pub fn at_end(&self) -> bool {
        self.sy >= self.layout.total_size().1 - self.vh - 0.5
    }

    pub fn current_page(&self) -> usize {
        self.layout.page_at(self.sy + self.vh * 0.3)
    }

    pub fn go_to(&mut self, dest: Dest) {
        let page = (dest.page as usize).min(self.page_count().saturating_sub(1));
        let r = self.layout.page_rect(page);
        let y = r.t + dest.y_pt.unwrap_or(0.0) * self.layout.scale()
            - if dest.y_pt.is_some() {
                8.0
            } else {
                paged::GAP * 0.5
            };
        self.scroll_to(None, Some(y));
    }

    pub fn go_page(&mut self, page: usize) {
        self.go_to(Dest {
            page: page as u32,
            y_pt: None,
        });
    }

    pub fn set_zoom_mode(&mut self, mode: ZoomMode, at: Option<(f32, f32)>) {
        self.zoom_mode = mode;
        let z = self
            .layout
            .resolve_zoom(mode, self.vw, self.vh, self.current_page());
        self.relayout(z, at.unwrap_or((self.vw * 0.5, self.vh * 0.5)));
        if mode == ZoomMode::FitPage {
            let p = self.current_page();
            self.go_page(p);
        }
    }

    /// 배율에 비율을 곱한다 (Ctrl+휠, Ctrl +/-).
    pub fn zoom_by(&mut self, factor: f32, at: Option<(f32, f32)>) {
        let z = (self.layout.zoom() * factor).clamp(paged::ZOOM_MIN, paged::ZOOM_MAX);
        self.set_zoom_mode(ZoomMode::Custom(z), at);
    }

    pub fn on_key(&mut self, vk: u16, shift: bool) -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        let page = self.current_page();
        let last = self.page_count().saturating_sub(1);
        match VIRTUAL_KEY(vk) {
            VK_DOWN => self.scroll_by(0.0, LINE_STEP),
            VK_UP => self.scroll_by(0.0, -LINE_STEP),
            VK_RIGHT => self.scroll_by(LINE_STEP, 0.0),
            VK_LEFT => self.scroll_by(-LINE_STEP, 0.0),
            // FR-P03: PgUp/PgDn = 이전/다음 쪽
            VK_NEXT => self.go_page((page + 1).min(last)),
            VK_PRIOR => {
                // 쪽 중간이면 그 쪽 맨 위로, 맨 위면 이전 쪽으로
                let top = self.layout.page_rect(page).t - paged::GAP * 0.5;
                if self.sy > top + 2.0 {
                    self.go_page(page)
                } else {
                    self.go_page(page.saturating_sub(1))
                }
            }
            VK_SPACE => self.scroll_by(0.0, if shift { -self.vh * 0.9 } else { self.vh * 0.9 }),
            VK_HOME => self.scroll_to(Some(0.0), Some(0.0)),
            VK_END => self.scroll_to(None, Some(f32::MAX)),
            VK_ESCAPE => self.selection = None,
            _ => return false,
        }
        true
    }

    pub fn status(&self) -> String {
        let mode = match self.zoom_mode {
            ZoomMode::FitWidth => " (폭 맞춤)",
            ZoomMode::FitPage => " (쪽 맞춤)",
            ZoomMode::Custom(_) => "",
        };
        let mut s = format!(
            "{} / {}쪽 · {:.0}%{mode}",
            self.current_page() + 1,
            self.page_count(),
            self.layout.zoom() * 100.0
        );
        if let Some(f) = self.search_status() {
            s.push_str(" · ");
            s.push_str(&f);
        }
        s
    }

    /// 찾기 진행 상태 ("3/17", "없음", "찾는 중…")
    pub fn search_status(&self) -> Option<String> {
        let st = self.search.as_ref()?;
        let idx = st
            .current
            .map(|(p, i)| st.hits.range(..p).map(|(_, v)| v.len()).sum::<usize>() + i + 1);
        Some(match idx {
            _ if st.total == 0 && st.done => "찾기: 없음".to_string(),
            Some(i) if st.done => format!("찾기 {i}/{}", st.total),
            Some(i) => format!("찾기 {i}/{}…", st.total),
            None => format!("찾는 중… {}", st.total),
        })
    }

    pub fn first_page_shown(&self) -> bool {
        self.first_page_logged
    }

    /// 세션 저장용 위치 (FR-V09)
    pub fn position(&self) -> FilePos {
        let page = self.layout.page_at(self.sy);
        let (_, y) = self.layout.to_page_pt(page, 0.0, self.sy);
        FilePos {
            path: self.path.clone(),
            page: page as u32,
            y_pt: y.max(0.0),
            zoom: match self.zoom_mode {
                ZoomMode::FitWidth => "fit-width".into(),
                ZoomMode::FitPage => "fit-page".into(),
                ZoomMode::Custom(z) => format!("{z:.4}"),
            },
        }
    }

    pub fn restore(&mut self, pos: &FilePos) {
        let mode = match pos.zoom.as_str() {
            "fit-page" => ZoomMode::FitPage,
            "fit-width" | "" => ZoomMode::FitWidth,
            z => z
                .parse()
                .map(ZoomMode::Custom)
                .unwrap_or(ZoomMode::FitWidth),
        };
        self.zoom_mode = mode;
        let z = self
            .layout
            .resolve_zoom(mode, self.vw, self.vh, pos.page as usize);
        self.layout.set(z, self.vw);
        let page = (pos.page as usize).min(self.page_count().saturating_sub(1));
        let r = self.layout.page_rect(page);
        self.scroll_to(None, Some(r.t + pos.y_pt * self.layout.scale()));
    }

    // ───── 엔진 결과 ─────

    fn drop_bitmaps(&mut self) {
        self.tiles.clear();
        self.sent_tiles.clear();
    }

    /// 렌더 타깃이 바뀌었으면(디바이스 손실·최소화) 비트맵을 버린다.
    pub fn check_epoch(&mut self, epoch: u64) {
        if self.epoch != epoch {
            self.epoch = epoch;
            self.drop_bitmaps();
        }
    }

    pub fn release_bitmaps(&mut self) {
        self.drop_bitmaps();
        self.epoch = 0;
    }

    pub fn on_tile(
        &mut self,
        t: &Target,
        spec: TileSpec,
        stride: u32,
        bgra: &[u8],
        render_ms: f64,
    ) -> windows::core::Result<()> {
        if t.epoch != self.epoch {
            return Ok(());
        }
        self.sent_tiles.retain(|s| s.key != spec.key);
        let bmp = render::create_bitmap(t, spec.w, spec.h, stride, bgra)?;
        let cost = (spec.w * spec.h * 4) as usize;
        self.tiles
            .insert(spec.key, Tile { bmp }, cost, &self.visible_keys);
        if spec.key.page_w_px != paged::LOW_RES_W {
            self.render_ms_total += render_ms;
            self.tiles_rendered += 1;
        }
        Ok(())
    }

    pub fn on_page_info(&mut self, page: u32, text: PageText, links: Vec<Link>) {
        self.sent_info.retain(|p| *p != page);
        self.pages.insert(page, PageData { text, links }, 1, &[]);
    }

    pub fn tile_stats(&self) -> (u32, f64, usize) {
        (self.tiles_rendered, self.render_ms_total, self.tiles.used())
    }

    // ───── 찾기 (FR-P05) ─────

    pub fn start_search(&mut self, query: &str) -> Cmd {
        let query = query.trim();
        if query.is_empty() {
            self.search = None;
            return Cmd::CancelSearch;
        }
        let id = self.next_search_id;
        self.next_search_id += 1;
        self.search = Some(Search {
            id,
            query: query.to_owned(),
            ..Default::default()
        });
        Cmd::Search {
            id,
            query: query.to_owned(),
            start: self.current_page() as u32,
        }
    }

    pub fn clear_search(&mut self) -> Option<Cmd> {
        self.search.take().map(|_| Cmd::CancelSearch)
    }

    pub fn search_query(&self) -> Option<&str> {
        self.search.as_ref().map(|s| s.query.as_str())
    }

    pub fn on_search_hits(&mut self, id: u64, page: u32, hits: Vec<Hit>) {
        let Some(st) = self.search.as_mut().filter(|s| s.id == id) else {
            return;
        };
        st.total += hits.len();
        st.hits.insert(page, hits);
        if st.current.is_none() {
            // 검색은 현재 쪽부터 시작하므로 첫 결과가 곧 "다음" 일치
            st.current = Some((page, 0));
            self.reveal_current();
        }
    }

    pub fn on_search_done(&mut self, id: u64) {
        if let Some(st) = self.search.as_mut().filter(|s| s.id == id) {
            st.done = true;
        }
    }

    /// F3 / Shift+F3
    pub fn find_step(&mut self, forward: bool) {
        let Some(st) = self.search.as_mut() else {
            return;
        };
        let flat: Vec<(u32, usize)> = st
            .hits
            .iter()
            .flat_map(|(p, v)| (0..v.len()).map(move |i| (*p, i)))
            .collect();
        if flat.is_empty() {
            return;
        }
        let pos = st.current.and_then(|c| flat.iter().position(|x| *x == c));
        let n = flat.len();
        let next = match (pos, forward) {
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
            (None, _) => 0,
        };
        st.current = Some(flat[next]);
        self.reveal_current();
    }

    fn reveal_current(&mut self) {
        let Some(st) = &self.search else { return };
        let Some((page, i)) = st.current else { return };
        let Some(r) = st
            .hits
            .get(&page)
            .and_then(|v| v.get(i))
            .and_then(|(_, rs)| rs.first())
            .copied()
        else {
            return;
        };
        let (x0, y0) = self.layout.to_doc(page as usize, r.l, r.t);
        let (x1, y1) = self.layout.to_doc(page as usize, r.r, r.b);
        if y0 < self.sy || y1 > self.sy + self.vh {
            self.sy = y0 - self.vh / 3.0;
        }
        if x0 < self.sx || x1 > self.sx + self.vw {
            self.sx = x0 - self.vw / 3.0;
        }
        self.clamp();
    }

    // ───── 선택 · 복사 · 링크 (FR-P06, FR-P07) ─────

    fn hit_page(&self, x: f32, y: f32) -> (usize, f32, f32) {
        let page = self.layout.page_at(self.sy + y);
        let (px, py) = self.layout.to_page_pt(page, self.sx + x, self.sy + y);
        (page, px, py)
    }

    fn link_at(&self, x: f32, y: f32) -> Option<&Link> {
        let (page, px, py) = self.hit_page(x, y);
        self.pages
            .peek(&(page as u32))?
            .links
            .iter()
            .find(|l| l.rect.contains(px, py))
    }

    fn caret_at(&self, x: f32, y: f32) -> Option<TextPos> {
        let (page, px, py) = self.hit_page(x, y);
        let data = self.pages.peek(&(page as u32))?;
        data.text.caret_at(px, py).map(|c| (page as u32, c))
    }

    pub fn on_mouse_down(&mut self, x: f32, y: f32, shift: bool) {
        let link = self.link_at(x, y).map(|l| l.target.clone());
        let caret = self.caret_at(x, y);
        match (shift, self.selection, caret) {
            (true, Some((a, _)), Some(c)) => self.selection = Some((a, c)),
            (_, _, Some(c)) => self.selection = Some((c, c)),
            _ => self.selection = None,
        }
        self.press = Some(Press {
            x,
            y,
            link,
            moved: false,
        });
    }

    /// 드래그 중이면 선택을 늘리고, 보기 밖으로 나가면 자동 스크롤한다.
    pub fn on_mouse_move(&mut self, x: f32, y: f32, dragging: bool) -> CursorKind {
        if dragging && let Some(p) = self.press.as_mut() {
            if (p.x - x).abs() > CLICK_SLOP || (p.y - y).abs() > CLICK_SLOP {
                p.moved = true;
            }
            if y < 0.0 {
                self.scroll_by(0.0, y);
            } else if y > self.vh {
                self.scroll_by(0.0, y - self.vh);
            }
            if let (Some((a, _)), Some(c)) = (self.selection, self.caret_at(x, y)) {
                self.selection = Some((a, c));
            }
            return CursorKind::IBeam;
        }
        if self.link_at(x, y).is_some() {
            return CursorKind::Hand;
        }
        let (page, px, py) = self.hit_page(x, y);
        match self.pages.peek(&(page as u32)) {
            Some(d) if d.text.char_at(px, py).is_some() => CursorKind::IBeam,
            _ => CursorKind::Arrow,
        }
    }

    /// 클릭(끌지 않음)이 링크 위였으면 이동하거나 외부 링크 명령을 돌려준다.
    pub fn on_mouse_up(&mut self) -> Option<Cmd> {
        let p = self.press.take()?;
        if p.moved {
            return None;
        }
        if self.selection.is_some_and(|(a, b)| a == b) {
            self.selection = None;
        }
        match p.link? {
            LinkTarget::Internal(d) => {
                self.go_to(d);
                None
            }
            LinkTarget::Uri(u) => Some(Cmd::OpenUri(u)),
        }
    }

    pub fn select_all(&mut self) {
        let last = self.page_count().saturating_sub(1) as u32;
        self.selection = Some(((0, 0), (last, usize::MAX)));
    }

    /// 선택한 쪽 텍스트가 모두 캐시에 있으면 바로, 아니면 엔진에 요청한다.
    pub fn copy(&self) -> Option<Cmd> {
        let (a, b) = order(self.selection?.0, self.selection?.1);
        if a == b {
            return None;
        }
        let mut out = String::new();
        for page in a.0..=b.0 {
            let Some(d) = self.pages.peek(&page) else {
                return Some(Cmd::Copy { from: a, to: b });
            };
            let s = if page == a.0 { a.1 } else { 0 };
            let e = if page == b.0 { b.1 } else { usize::MAX };
            if !out.is_empty() && !out.ends_with('\n') {
                out.push_str("\r\n");
            }
            out.push_str(&d.text.text(s..e));
        }
        Some(Cmd::SetClipboard(out))
    }

    // ───── 그리기 ─────

    /// 보이는 쪽을 그리고, 필요한 타일·텍스트 요청을 돌려준다.
    pub fn paint(&mut self, t: &Target) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        let dpi = self.dpi;
        let px = |v: f32| (v * dpi / 96.0).round();
        let dip = |p: f32| p * 96.0 / dpi;
        let range = self.layout.visible(self.sy, self.sy + self.vh);
        let mut low_needed = Vec::new();
        let mut full_needed = Vec::new();
        let mut info_needed = Vec::new();
        self.visible_keys.clear();
        let sel = self.selection.map(|(a, b)| order(a, b));

        unsafe { t.rt.Clear(Some(&rgb(render::COLOR_PDF_BG))) };
        let mut frame_complete = !range.is_empty();
        for page in range.clone() {
            let p32 = page as u32;
            let r = self.layout.page_rect(page);
            // 화면 좌표, 장치 픽셀에 맞춤
            let (ox, oy) = (px(r.l - self.sx), px(r.t - self.sy));
            let (pw, ph) = self.layout.page_px(page, dpi);
            let dest = RectF::new(dip(ox), dip(oy), dip(ox + pw as f32), dip(oy + ph as f32));
            render::fill(
                t,
                RectF::new(dest.l - 1.0, dest.t - 1.0, dest.r + 1.0, dest.b + 1.0),
                &t.page_border,
            );
            render::fill(t, dest, &t.placeholder);

            // 저해상도(점진 표시, FR-P04)
            let (lw, lh) = paged::low_res_px(
                self.layout.page_size_pt(page).0,
                self.layout.page_size_pt(page).1,
            );
            let low_key = TileKey {
                page: p32,
                page_w_px: lw,
                x: 0,
                y: 0,
            };
            let has_low = if let Some(tile) = self.tiles.get(&low_key) {
                unsafe {
                    t.rt.DrawBitmap(
                        &tile.bmp,
                        Some(&render::rect(dest.l, dest.t, dest.r, dest.b)),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        None,
                    );
                }
                self.visible_keys.push(low_key);
                true
            } else {
                false
            };

            // 보이는 픽셀 구간(쪽 px 기준)
            let vis = RectF::new(-ox, -oy, -ox + px(self.vw), -oy + px(self.vh));
            let mut all_full = true;
            for spec in paged::plan_tiles(p32, pw, ph, vis) {
                self.visible_keys.push(spec.key);
                if let Some(tile) = self.tiles.get(&spec.key) {
                    let (x, y) = (ox + spec.key.x as f32, oy + spec.key.y as f32);
                    let d = render::rect(
                        dip(x),
                        dip(y),
                        dip(x + spec.w as f32),
                        dip(y + spec.h as f32),
                    );
                    unsafe {
                        t.rt.DrawBitmap(
                            &tile.bmp,
                            Some(&d),
                            1.0,
                            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                            None,
                        )
                    };
                } else {
                    all_full = false;
                    full_needed.push(spec);
                }
            }
            if !all_full && !has_low {
                low_needed.push(TileSpec {
                    key: low_key,
                    page_h_px: lh,
                    w: lw,
                    h: lh,
                });
            }
            frame_complete &= all_full;

            // 덧그리기: 찾기 결과, 선택
            let k = self.layout.scale();
            let to_screen = |q: &RectF| {
                let (x, y) = (dest.l + q.l * k, dest.t + q.t * k);
                RectF::new(x, y, x + q.w() * k, y + q.h() * k)
            };
            if let Some(st) = &self.search
                && let Some(hits) = st.hits.get(&p32)
            {
                for (i, (_, rects)) in hits.iter().enumerate() {
                    let brush = if st.current == Some((p32, i)) {
                        &t.hit_current
                    } else {
                        &t.hit
                    };
                    for q in rects {
                        render::fill(t, to_screen(q), brush);
                    }
                }
            }
            if let Some(d) = self.pages.peek(&p32) {
                if let Some((a, b)) = sel
                    && a.0 <= p32
                    && p32 <= b.0
                {
                    let s = if p32 == a.0 { a.1 } else { 0 };
                    let e = if p32 == b.0 { b.1 } else { usize::MAX };
                    for q in d.text.range_rects(s..e) {
                        render::fill(t, to_screen(&q), &t.selection);
                    }
                }
            } else {
                info_needed.push(p32);
            }
        }

        self.last_frame_complete = frame_complete;
        // 첫 완성 프레임: 보이는 쪽이 모두 고해상도로 그려진 첫 순간 (NFR-P05)
        if frame_complete && !self.first_page_logged {
            self.first_page_logged = true;
            let total = self.opened_at.elapsed().as_secs_f64() * 1000.0;
            perf::record("pdf_first_page_ms", format!("{total:.1}"));
            self.timings = format!("첫 페이지 {total:.0}ms ({})", self.timings);
        }

        // 다음 쪽 미리 렌더 (작은 쪽일 때만 — 스크롤 시작 시 빈 화면 방지)
        if range.end < self.page_count() && full_needed.is_empty() && low_needed.is_empty() {
            let page = range.end;
            let (pw, ph) = self.layout.page_px(page, dpi);
            let spec = paged::plan_tiles(
                page as u32,
                pw,
                ph,
                RectF::new(0.0, 0.0, pw as f32, ph as f32),
            );
            if spec.len() == 1 && !self.tiles.contains(&spec[0].key) {
                full_needed.push(spec[0]);
            }
        }

        // 저해상도 먼저, 그다음 고해상도 (FR-P04)
        let mut needed = low_needed;
        needed.extend(full_needed);
        if needed != self.sent_tiles {
            self.sent_tiles = needed.clone();
            cmds.push(Cmd::Render(needed));
        }
        if !info_needed.is_empty() && info_needed != self.sent_info {
            self.sent_info = info_needed.clone();
            cmds.push(Cmd::PageInfo(info_needed));
        }
        cmds
    }
}

/// 목차를 트리 삽입 순서로 펼친다: (깊이, 제목, 목적지)
pub fn flatten_outline(items: &[OutlineItem]) -> Vec<(usize, String, Option<Dest>)> {
    fn walk(items: &[OutlineItem], depth: usize, out: &mut Vec<(usize, String, Option<Dest>)>) {
        for it in items {
            out.push((depth, it.title.clone(), it.dest));
            walk(&it.children, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    walk(items, 0, &mut out);
    out
}

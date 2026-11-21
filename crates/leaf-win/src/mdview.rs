//! MD 보기 (FR-M01~M10): DirectWrite 측정기 + 가상화 레이아웃(`leaf-layout`) 그리기,
//! 이미지(WIC, 지연 디코딩), 링크, 목차, 찾기, 선택·복사.
//!
//! 이 모듈은 Win32 메시지를 보내지 않는다. 워커 작업·창 동작은 `MdCmd`로 돌려준다.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use leaf_core::lru::BudgetLru;
use leaf_core::session::FilePos;
use leaf_doc::flow::slugify;
use leaf_doc::{Dest, OutlineItem, RectF};
use leaf_layout::content::{HeadingRef, Para, ParaKind, TextAlign, style};
use leaf_layout::flow::{CODE_SPAN_EM, SMALL_SPAN_EM, base_px, font_size};
use leaf_layout::{
    Content, FillKind, FlowLayout, ImageId, ImgSize, Item, Measured, Measurer, ParaId,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, ID2D1Bitmap, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::*;
use windows::core::{BOOL, IUnknown, Interface, PCWSTR, Ref, Result, implement, w};

use crate::mdload::{self, DecodedImage, ImgSrc, Target as LinkTarget};
use crate::pdfview::CursorKind;
use crate::render::{self, Target};
use crate::textview::range_rects;

/// 본문 최대 폭(em), 가운데 정렬 (FR-M10, 설정 `[markdown] max_width_em`)
/// MD 보기 서식 (설정 `[markdown]`·`[memory]`, FR-M10)
#[derive(Clone, Debug, PartialEq)]
pub struct MdStyle {
    /// NUL로 끝나는 UTF-16 본문 글꼴 이름
    pub font: Vec<u16>,
    /// 본문 크기(pt). 레이아웃 기준(11pt) 대비 비율을 배율에 곱한다
    pub size_pt: f32,
    pub max_width_em: f32,
    pub image_budget: usize,
}
/// 좌우 최소 여백(em)
const SIDE_EM: f32 = 2.0;
const ZOOM_MIN: f32 = 0.5;
const ZOOM_MAX: f32 = 3.0;
/// MD 이미지 비트맵 캐시 예산 (SPEC §4.5, 설정 `[memory] image_cache_mb`)
pub const MAX_HITS: usize = 10_000;

pub enum MdCmd {
    LoadImage {
        id: ImageId,
        path: PathBuf,
        max_w: u32,
    },
    OpenUri(String),
    /// 앱 안에서 열기 (트리 선택 동기화)
    OpenDoc {
        path: PathBuf,
        anchor: Option<String>,
    },
    /// 지원하지 않는 로컬 파일: 실행하지 않고 탐색기에서 보기만 (§6.3)
    Reveal(PathBuf),
    Status(String),
    /// 문서 안 앵커로 이동했다: 이동 전 위치를 뒤로 가기 기록에 (FR-V02)
    Jumped(FilePos),
}

// ───────────────────────── DirectWrite 측정기 ─────────────────────────

/// 인라인 이미지 자리만 차지하는 객체. 그림은 MdView가 위치를 물어 직접 그린다.
#[implement(IDWriteInlineObject)]
struct Spacer {
    w: f32,
    h: f32,
}

impl IDWriteInlineObject_Impl for Spacer_Impl {
    fn Draw(
        &self,
        _ctx: *const core::ffi::c_void,
        _renderer: Ref<IDWriteTextRenderer>,
        _x: f32,
        _y: f32,
        _sideways: BOOL,
        _rtl: BOOL,
        _effect: Ref<IUnknown>,
    ) -> Result<()> {
        Ok(())
    }

    fn GetMetrics(&self) -> Result<DWRITE_INLINE_OBJECT_METRICS> {
        Ok(DWRITE_INLINE_OBJECT_METRICS {
            width: self.w,
            height: self.h,
            baseline: self.h,
            supportsSideways: false.into(),
        })
    }

    fn GetOverhangMetrics(&self) -> Result<DWRITE_OVERHANG_METRICS> {
        Ok(DWRITE_OVERHANG_METRICS::default())
    }

    fn GetBreakConditions(
        &self,
        before: *mut DWRITE_BREAK_CONDITION,
        after: *mut DWRITE_BREAK_CONDITION,
    ) -> Result<()> {
        unsafe {
            if !before.is_null() {
                *before = DWRITE_BREAK_CONDITION_NEUTRAL;
            }
            if !after.is_null() {
                *after = DWRITE_BREAK_CONDITION_NEUTRAL;
            }
        }
        Ok(())
    }
}

struct DwInner {
    layout: Option<IDWriteTextLayout>,
    /// 링크 색(drawing effect)을 입힌 렌더 타깃 세대 — 브러시는 타깃에 묶여 있다
    epoch: Cell<u64>,
    /// 인라인 코드 배경 사각형 (문단 기준)
    code_rects: RefCell<Option<Vec<RectF>>>,
}

#[derive(Clone)]
pub struct DwPara(Rc<DwInner>);

impl DwPara {
    fn layout(&self) -> Option<&IDWriteTextLayout> {
        self.0.layout.as_ref()
    }

    fn apply_effects(&self, p: &Para, t: &Target) {
        if self.0.epoch.get() == t.epoch || p.links.is_empty() {
            return;
        }
        if let (Some(l), Ok(brush)) = (self.layout(), t.link.cast::<IUnknown>()) {
            for s in &p.links {
                let r = DWRITE_TEXT_RANGE {
                    startPosition: s.start,
                    length: s.end - s.start,
                };
                unsafe {
                    let _ = l.SetDrawingEffect(&brush, r);
                }
            }
        }
        self.0.epoch.set(t.epoch);
    }

    fn code_rects(&self, p: &Para) -> Vec<RectF> {
        if let Some(v) = &*self.0.code_rects.borrow() {
            return v.clone();
        }
        let mut v = Vec::new();
        if let Some(l) = self.layout() {
            for s in p.spans.iter().filter(|s| s.style & style::CODE != 0) {
                for r in range_rects(l, s.start, s.end - s.start, 0.0, 0.0) {
                    v.push(RectF::new(r.left, r.top, r.right, r.bottom));
                }
            }
        }
        *self.0.code_rects.borrow_mut() = Some(v.clone());
        v
    }
}

pub struct DwMeasurer {
    dw: IDWriteFactory,
    family: Vec<u16>,
    zoom: f32,
    formats: HashMap<ParaKind, IDWriteTextFormat>,
}

impl DwMeasurer {
    pub fn new(dw: IDWriteFactory, family: Vec<u16>) -> Self {
        Self {
            dw,
            family,
            zoom: 1.0,
            formats: HashMap::new(),
        }
    }

    fn format(&mut self, kind: ParaKind) -> Result<IDWriteTextFormat> {
        if let Some(f) = self.formats.get(&kind) {
            return Ok(f.clone());
        }
        let size = font_size(kind, self.zoom);
        let body = PCWSTR(self.family.as_ptr());
        let (family, weight) = match kind {
            ParaKind::Code => (w!("Consolas"), DWRITE_FONT_WEIGHT_NORMAL),
            ParaKind::Heading(_) | ParaKind::TableHead => (body, DWRITE_FONT_WEIGHT_SEMI_BOLD),
            _ => (body, DWRITE_FONT_WEIGHT_NORMAL),
        };
        unsafe {
            let f = self.dw.CreateTextFormat(
                family,
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!("ko-KR"),
            )?;
            f.SetWordWrapping(if kind == ParaKind::Marker {
                DWRITE_WORD_WRAPPING_NO_WRAP
            } else {
                // 표 칸이 좁으면 단어 중간에서도 끊는다
                DWRITE_WORD_WRAPPING_EMERGENCY_BREAK
            })?;
            if kind == ParaKind::Code {
                // Consolas 한 칸 ≈ 0.55em, 탭 4칸
                f.SetIncrementalTabStop(size * 0.55 * 4.0)?;
            }
            self.formats.insert(kind, f.clone());
            Ok(f)
        }
    }

    fn make(&mut self, p: &Para, width: f32, objs: &[(f32, f32)]) -> Result<IDWriteTextLayout> {
        let f = self.format(p.kind)?;
        let size = font_size(p.kind, self.zoom);
        unsafe {
            let l = self
                .dw
                .CreateTextLayout(&p.text, &f, width.max(1.0), f32::MAX)?;
            l.SetTextAlignment(match p.align {
                TextAlign::Leading => DWRITE_TEXT_ALIGNMENT_LEADING,
                TextAlign::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                TextAlign::Trailing => DWRITE_TEXT_ALIGNMENT_TRAILING,
            })?;
            for s in &p.spans {
                let r = DWRITE_TEXT_RANGE {
                    startPosition: s.start,
                    length: s.end - s.start,
                };
                if s.style & style::BOLD != 0 {
                    l.SetFontWeight(DWRITE_FONT_WEIGHT_BOLD, r)?;
                }
                if s.style & style::ITALIC != 0 {
                    l.SetFontStyle(DWRITE_FONT_STYLE_ITALIC, r)?;
                }
                if s.style & style::STRIKE != 0 {
                    l.SetStrikethrough(true, r)?;
                }
                if s.style & style::CODE != 0 {
                    l.SetFontFamilyName(w!("Consolas"), r)?;
                    l.SetFontSize(size * CODE_SPAN_EM, r)?;
                }
                if s.style & style::SMALL != 0 {
                    l.SetFontSize(size * SMALL_SPAN_EM, r)?;
                }
            }
            for (o, &(w, h)) in p.objects.iter().zip(objs) {
                let obj: IDWriteInlineObject = Spacer { w, h }.into();
                l.SetInlineObject(
                    &obj,
                    DWRITE_TEXT_RANGE {
                        startPosition: o.pos,
                        length: 1,
                    },
                )?;
            }
            Ok(l)
        }
    }
}

fn metrics(l: &IDWriteTextLayout) -> DWRITE_TEXT_METRICS {
    let mut m = DWRITE_TEXT_METRICS::default();
    unsafe {
        let _ = l.GetMetrics(&mut m);
    }
    m
}

impl Measurer for DwMeasurer {
    type Handle = DwPara;

    fn set_zoom(&mut self, zoom: f32) {
        if (zoom - self.zoom).abs() > 1e-4 {
            self.zoom = zoom;
            self.formats.clear();
        }
    }

    fn layout(&mut self, p: &Para, width: f32, objs: &[(f32, f32)]) -> Measured<DwPara> {
        let layout = self.make(p, width, objs).ok();
        let h = layout
            .as_ref()
            .map_or(font_size(p.kind, self.zoom) * 1.33, |l| metrics(l).height);
        Measured {
            handle: DwPara(Rc::new(DwInner {
                layout,
                epoch: Cell::new(0),
                code_rects: RefCell::new(None),
            })),
            h,
        }
    }

    fn intrinsic(&mut self, p: &Para) -> (f32, f32) {
        let em = base_px(self.zoom);
        let objs = vec![(em, em); p.objects.len()];
        match self.make(p, 1.0e6, &objs) {
            Ok(l) => {
                let max = metrics(&l).widthIncludingTrailingWhitespace;
                let min = unsafe { l.DetermineMinWidth() }.unwrap_or(max);
                (min.min(max), max)
            }
            Err(_) => (em, em),
        }
    }

    fn caret(&self, h: &DwPara, idx: u32) -> (f32, f32, f32) {
        let Some(l) = h.layout() else {
            return (0.0, 0.0, 0.0);
        };
        let (mut x, mut y) = (0.0f32, 0.0f32);
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        unsafe {
            let _ = l.HitTestTextPosition(idx, false, &mut x, &mut y, &mut m);
        }
        (x, m.top, m.height)
    }

    fn object_pos(&self, h: &DwPara, pos: u32, size: (f32, f32)) -> (f32, f32) {
        let Some(l) = h.layout() else {
            return (0.0, 0.0);
        };
        let (mut x, mut y) = (0.0f32, 0.0f32);
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        unsafe {
            let _ = l.HitTestTextPosition(pos, false, &mut x, &mut y, &mut m);
        }
        // 객체 아래 끝이 줄 기준선에 붙는다
        let mut n = 0u32;
        unsafe {
            let _ = l.GetLineMetrics(None, &mut n);
        }
        let mut lines = vec![DWRITE_LINE_METRICS::default(); n as usize];
        if unsafe { l.GetLineMetrics(Some(&mut lines), &mut n) }.is_ok() {
            let (mut top, mut acc) = (0.0f32, 0u32);
            for lm in &lines {
                if pos < acc + lm.length {
                    return (m.left, top + lm.baseline - size.1);
                }
                acc += lm.length;
                top += lm.height;
            }
        }
        (m.left, m.top)
    }
}

// ───────────────────────── 보기 ─────────────────────────

type Pos = (ParaId, u32);

struct MdBitmap {
    bmp: ID2D1Bitmap,
    pw: u32,
}

struct FrameText {
    para: ParaId,
    rect: RectF,
    handle: DwPara,
}

struct FrameImage {
    rect: RectF,
    link: Option<String>,
}

struct MdSearch {
    query: String,
    hits: Vec<(ParaId, u32, u32)>,
    current: Option<usize>,
}

pub struct MdView {
    pub generation: u64,
    pub path: PathBuf,
    style: MdStyle,
    /// 글꼴 크기 설정 / 레이아웃 기준 크기
    base: f32,
    root: Option<PathBuf>,
    flow: FlowLayout<DwMeasurer>,
    srcs: Vec<ImgSrc>,
    bitmaps: BudgetLru<ImageId, MdBitmap>,
    pending: HashSet<ImageId>,
    epoch: u64,
    dwrite: IDWriteFactory,
    alt_format: Option<IDWriteTextFormat>,
    vw: f32,
    vh: f32,
    dpi: f32,
    frame: Vec<FrameText>,
    frame_images: Vec<FrameImage>,
    visible_images: Vec<ImageId>,
    sel: Option<(Pos, Pos)>,
    press: Option<(f32, f32)>,
    search: Option<MdSearch>,
    opened_at: Instant,
    pub first_paint_ms: Option<f64>,
}

impl MdView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        generation: u64,
        path: PathBuf,
        root: Option<PathBuf>,
        content: Content,
        sizes: Vec<ImgSize>,
        srcs: Vec<ImgSrc>,
        dwrite: IDWriteFactory,
        style: MdStyle,
        opened_at: Instant,
    ) -> Self {
        let base = (style.size_pt / leaf_layout::flow::BASE_PT).clamp(0.3, 5.0);
        let mut flow = FlowLayout::new(
            content,
            DwMeasurer::new(dwrite.clone(), style.font.clone()),
            base,
        );
        for (i, s) in sizes.into_iter().enumerate() {
            flow.set_image_size(i as ImageId, s);
        }
        Self {
            generation,
            path,
            root,
            flow,
            srcs,
            bitmaps: BudgetLru::new(style.image_budget),
            base,
            style,
            pending: HashSet::new(),
            epoch: 0,
            dwrite,
            alt_format: None,
            vw: 0.0,
            vh: 0.0,
            dpi: 96.0,
            frame: Vec::new(),
            frame_images: Vec::new(),
            visible_images: Vec::new(),
            sel: None,
            press: None,
            search: None,
            opened_at,
            first_paint_ms: None,
        }
    }

    /// 목차 (FR-M07): 제목 단계로 중첩. `Dest.page` = 단위 번호.
    pub fn outline(&self) -> Vec<OutlineItem> {
        fn build(hs: &[HeadingRef], i: &mut usize, level: u8) -> Vec<OutlineItem> {
            let mut out = Vec::new();
            while *i < hs.len() && hs[*i].level > level {
                let h = &hs[*i];
                *i += 1;
                let children = build(hs, i, h.level);
                out.push(OutlineItem {
                    title: h.text.clone(),
                    dest: Some(Dest {
                        page: h.unit as u32,
                        y_pt: None,
                    }),
                    children,
                });
            }
            out
        }
        build(&self.flow.c.headings, &mut 0, 0)
    }

    /// 화면 위쪽(20%) 지점이 속한 제목 (목차 현재 위치 강조)
    pub fn current_heading(&mut self) -> Option<usize> {
        if self.flow.c.headings.is_empty() || self.vh <= 0.0 {
            return None;
        }
        let u = self.flow.unit_at(self.vh * 0.2);
        let i = self.flow.c.headings.partition_point(|h| h.unit <= u);
        i.checked_sub(1)
    }

    pub fn image_count(&self) -> usize {
        self.srcs.len()
    }

    pub fn check_epoch(&mut self, epoch: u64) {
        if self.epoch != epoch {
            self.epoch = epoch;
            self.bitmaps.clear();
        }
    }

    /// 최소화: 레이아웃·비트맵 캐시 비움 (SPEC §4.5)
    pub fn release(&mut self) {
        self.flow.release();
        self.bitmaps.clear();
        self.frame.clear();
    }

    pub fn set_view(&mut self, vw: f32, vh: f32, dpi: f32) {
        self.vw = vw;
        self.vh = vh;
        self.dpi = dpi;
        let (cw, _) = self.content_box();
        self.flow.set_viewport(cw, vh);
    }

    pub fn view_size(&self) -> (f32, f32) {
        (self.vw, self.vh)
    }

    fn em(&self) -> f32 {
        base_px(self.flow.zoom())
    }

    /// (본문 폭, 왼쪽 x)
    fn content_box(&self) -> (f32, f32) {
        let em = self.em();
        let cw = (self.vw - 2.0 * SIDE_EM * em)
            .min(self.style.max_width_em * em)
            .max(4.0 * em);
        (cw, ((self.vw - cw) * 0.5).max(0.0))
    }

    // ───── 스크롤 ─────

    pub fn scroll_by(&mut self, d: f32) {
        self.flow.scroll_by(d);
    }

    pub fn scroll_to(&mut self, y: f32) {
        self.flow.scroll_to(y);
    }

    pub fn home(&mut self) {
        self.flow.home();
    }

    pub fn end(&mut self) {
        self.flow.end();
    }

    pub fn at_end(&mut self) -> bool {
        self.flow.at_end()
    }

    pub fn go_unit(&mut self, u: usize) {
        self.flow.go_unit(u);
    }

    /// `#앵커`로 이동. 찾지 못하면 false.
    pub fn go_anchor(&mut self, a: &str) -> bool {
        let c = &self.flow.c;
        let lower = a.to_lowercase();
        let u = c
            .anchors
            .get(a)
            .or_else(|| c.anchors.get(&lower))
            .or_else(|| c.anchors.get(&slugify(a)))
            .copied();
        match u {
            Some(u) => {
                self.flow.go_unit(u);
                true
            }
            None => false,
        }
    }

    pub fn scroll_metrics(&self) -> ((f64, f64, f64), (f64, f64, f64)) {
        let (total, pos) = self.flow.metrics();
        ((0.0, 0.0, 0.0), (total as f64, self.vh as f64, pos as f64))
    }

    // ───── 배율 ─────

    /// 사용자 배율 (글꼴 크기 설정 비율 제외)
    fn zoom(&self) -> f32 {
        self.flow.zoom() / self.base
    }

    pub fn zoom_by(&mut self, f: f32) {
        self.set_zoom(self.zoom() * f);
    }

    pub fn set_zoom(&mut self, z: f32) {
        self.flow.set_zoom(z.clamp(ZOOM_MIN, ZOOM_MAX) * self.base);
        self.alt_format = None;
    }

    // ───── 세션 ─────

    pub fn position(&self) -> FilePos {
        let (u, off) = self.flow.position();
        FilePos {
            path: self.path.clone(),
            page: u as u32,
            y_pt: off,
            zoom: format!("{:.4}", self.zoom()),
            offset: 0,
            view: "md".into(),
            wrap: None,
        }
    }

    pub fn restore(&mut self, pos: &FilePos) {
        if pos.view != "md" {
            return;
        }
        if let Ok(z) = pos.zoom.parse::<f32>() {
            self.set_zoom(z);
        }
        self.flow.restore(pos.page as usize, pos.y_pt);
    }

    pub fn status(&self) -> String {
        let (total, pos) = self.flow.metrics();
        let pct = if total > self.vh + 1.0 {
            (pos / (total - self.vh) * 100.0).clamp(0.0, 100.0)
        } else {
            100.0
        };
        let mut s = format!("{pct:.0}% · {:.0}%", self.zoom() * 100.0);
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

    pub fn start_search(&mut self, q: &str) {
        let q = q.trim();
        if q.is_empty() {
            self.search = None;
            return;
        }
        let hits = self.flow.c.find(q, MAX_HITS);
        let (anchor, _) = self.flow.position();
        let pu = &self.flow.c.para_unit;
        let current = (!hits.is_empty()).then(|| {
            hits.iter()
                .position(|h| pu[h.0 as usize] as usize >= anchor)
                .unwrap_or(0)
        });
        self.search = Some(MdSearch {
            query: q.to_string(),
            hits,
            current,
        });
        self.reveal_current();
    }

    pub fn clear_search(&mut self) {
        self.search = None;
    }

    pub fn find_step(&mut self, forward: bool) {
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
            None => 0,
        });
        self.reveal_current();
    }

    fn reveal_current(&mut self) {
        let Some((p, st)) = self
            .search
            .as_ref()
            .and_then(|s| s.current.map(|i| (s.hits[i].0, s.hits[i].1)))
        else {
            return;
        };
        self.flow.reveal(p, st);
    }

    pub fn search_status(&self) -> Option<String> {
        let s = self.search.as_ref()?;
        let total = if s.hits.len() >= MAX_HITS {
            format!("{}+", s.hits.len())
        } else {
            s.hits.len().to_string()
        };
        Some(match s.current {
            Some(i) => format!("찾기 {}/{total}", i + 1),
            None => "찾기: 없음".into(),
        })
    }

    // ───── 선택 · 복사 · 링크 ─────

    /// 점에 가장 가까운 글자 위치. 두 번째 값은 글자 위에 정확히 있는지.
    fn hit(&self, x: f32, y: f32) -> Option<(Pos, bool, u32)> {
        let f = self
            .frame
            .iter()
            .find(|f| f.rect.contains(x, y))
            .or_else(|| {
                self.frame
                    .iter()
                    .filter(|f| y >= f.rect.t && y <= f.rect.b)
                    .min_by(|a, b| a.rect.dist2(x, y).total_cmp(&b.rect.dist2(x, y)))
            })
            .or_else(|| {
                self.frame
                    .iter()
                    .min_by(|a, b| a.rect.dist2(x, y).total_cmp(&b.rect.dist2(x, y)))
            })?;
        let l = f.handle.layout()?;
        let mut trailing = BOOL(0);
        let mut inside = BOOL(0);
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        unsafe {
            l.HitTestPoint(
                x - f.rect.l,
                y - f.rect.t,
                &mut trailing,
                &mut inside,
                &mut m,
            )
            .ok()?;
        }
        let idx = m.textPosition + if trailing.as_bool() { m.length } else { 0 };
        let exact = inside.as_bool() && f.rect.contains(x, y);
        Some(((f.para, idx), exact, m.textPosition))
    }

    fn link_at(&self, x: f32, y: f32) -> Option<String> {
        if let Some(img) = self
            .frame_images
            .iter()
            .find(|i| i.link.is_some() && i.rect.contains(x, y))
        {
            return img.link.clone();
        }
        let ((para, _), exact, at) = self.hit(x, y)?;
        if !exact {
            return None;
        }
        self.flow.c.paras[para as usize]
            .link_at(at)
            .map(str::to_string)
    }

    pub fn on_mouse_down(&mut self, x: f32, y: f32, shift: bool) {
        self.press = Some((x, y));
        let Some((p, _, _)) = self.hit(x, y) else {
            return;
        };
        self.sel = match (shift, self.sel) {
            (true, Some((a, _))) => Some((a, p)),
            _ => Some((p, p)),
        };
    }

    pub fn on_mouse_move(&mut self, x: f32, y: f32, dragging: bool) -> CursorKind {
        if dragging && self.sel.is_some() {
            if y < 0.0 {
                self.flow.scroll_by(y.max(-60.0));
            } else if y > self.vh {
                self.flow.scroll_by((y - self.vh).min(60.0));
            }
            if let (Some((a, _)), Some((p, _, _))) =
                (self.sel, self.hit(x, y.clamp(0.0, self.vh - 1.0)))
            {
                self.sel = Some((a, p));
            }
            return CursorKind::IBeam;
        }
        if self.link_at(x, y).is_some() {
            return CursorKind::Hand;
        }
        match self.hit(x, y) {
            Some((_, true, _)) => CursorKind::IBeam,
            _ => CursorKind::Arrow,
        }
    }

    /// 누른 자리에서 놓았으면(끌지 않았으면) 링크를 따라간다
    pub fn on_mouse_up(&mut self, x: f32, y: f32) -> Vec<MdCmd> {
        let Some((px, py)) = self.press.take() else {
            return Vec::new();
        };
        if (px - x).abs() > 4.0 || (py - y).abs() > 4.0 {
            return Vec::new();
        }
        match self.link_at(x, y) {
            Some(href) => {
                self.sel = None;
                self.follow(&href)
            }
            None => Vec::new(),
        }
    }

    /// 링크 따라가기 (FR-M04)
    pub fn follow(&mut self, href: &str) -> Vec<MdCmd> {
        let base = self.path.parent().unwrap_or(Path::new(""));
        let before = self.position();
        match mdload::resolve(base, self.root.as_deref(), href) {
            LinkTarget::Anchor(a) => {
                if self.go_anchor(&a) {
                    vec![MdCmd::Jumped(before)]
                } else {
                    vec![MdCmd::Status(format!("문서에 없는 앵커: #{a}"))]
                }
            }
            LinkTarget::Local(p, anchor) => {
                if same_file(&p, &self.path) {
                    return match anchor {
                        Some(a) if !self.go_anchor(&a) => {
                            vec![MdCmd::Status(format!("문서에 없는 앵커: #{a}"))]
                        }
                        Some(_) => vec![MdCmd::Jumped(before)],
                        None => Vec::new(),
                    };
                }
                let supported = p
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(leaf_core::is_supported_ext);
                if p.is_file() && supported {
                    vec![MdCmd::OpenDoc { path: p, anchor }]
                } else if p.exists() {
                    vec![MdCmd::Reveal(p)]
                } else {
                    vec![MdCmd::Status(format!("파일이 없습니다: {}", p.display()))]
                }
            }
            LinkTarget::Uri(u) => vec![MdCmd::OpenUri(u)],
        }
    }

    pub fn select_all(&mut self) {
        self.sel = Some(((0, 0), self.flow.c.end_pos()));
    }

    pub fn copy(&self) -> Option<String> {
        let (a, b) = self.sel?;
        if a == b {
            return None;
        }
        Some(self.flow.c.text_between(a, b).replace('\n', "\r\n"))
    }

    // ───── 키보드 ─────

    pub fn on_key(&mut self, vk: u16, shift: bool) -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        let line = self.em() * 3.0;
        let page = (self.vh - 2.0 * self.em()).max(line);
        match VIRTUAL_KEY(vk) {
            VK_DOWN => self.flow.scroll_by(line),
            VK_UP => self.flow.scroll_by(-line),
            VK_NEXT => self.flow.scroll_by(page),
            VK_PRIOR => self.flow.scroll_by(-page),
            VK_SPACE => self.flow.scroll_by(if shift { -page } else { page }),
            VK_HOME => self.flow.home(),
            VK_END => self.flow.end(),
            VK_ESCAPE => self.sel = None,
            _ => return false,
        }
        true
    }

    // ───── 이미지 ─────

    pub fn on_image(
        &mut self,
        t: &Target,
        id: ImageId,
        r: std::result::Result<DecodedImage, String>,
    ) {
        self.pending.remove(&id);
        self.check_epoch(t.epoch);
        match r {
            Ok(d) => {
                self.flow
                    .set_image_size(id, ImgSize::Known(d.iw as f32, d.ih as f32));
                if let Ok(bmp) = render::create_bitmap_premul(t, d.pw, d.ph, &d.bgra) {
                    let cost = d.bgra.len();
                    self.bitmaps
                        .insert(id, MdBitmap { bmp, pw: d.pw }, cost, &self.visible_images);
                }
            }
            Err(_) => self.flow.set_image_size(id, ImgSize::Failed),
        }
    }

    pub fn bitmap_bytes(&self) -> usize {
        self.bitmaps.used()
    }

    // ───── 그리기 ─────

    fn ensure_alt_format(&mut self) -> Option<IDWriteTextFormat> {
        if self.alt_format.is_none() {
            self.alt_format = unsafe {
                self.dwrite
                    .CreateTextFormat(
                        PCWSTR(self.style.font.as_ptr()),
                        None,
                        DWRITE_FONT_WEIGHT_NORMAL,
                        DWRITE_FONT_STYLE_NORMAL,
                        DWRITE_FONT_STRETCH_NORMAL,
                        self.em() * 0.9,
                        w!("ko-KR"),
                    )
                    .ok()
            };
            if let Some(f) = &self.alt_format {
                unsafe {
                    let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
                }
            }
        }
        self.alt_format.clone()
    }

    pub fn paint(&mut self, t: &Target) -> Vec<MdCmd> {
        self.check_epoch(t.epoch);
        let alt_fmt = self.ensure_alt_format();
        let (cw, x0) = self.content_box();
        self.flow.set_viewport(cw, self.vh);
        let vis = self.flow.visible();
        self.frame.clear();
        self.frame_images.clear();
        self.visible_images.clear();
        let mut cmds = Vec::new();
        let sel = self.sel.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
        let scale = self.dpi / 96.0;
        let em = self.em();

        for (u, uy) in vis {
            let Some(bl) = self.flow.block(u) else {
                continue;
            };
            for item in &bl.items {
                match item {
                    Item::Fill { kind, rect } => {
                        if rect.is_empty() {
                            continue;
                        }
                        let brush = match kind {
                            FillKind::CodeBg | FillKind::TableHead | FillKind::TableStripe => {
                                &t.code_bg
                            }
                            _ => &t.border,
                        };
                        render::fill(t, offset(*rect, x0, uy), brush);
                    }
                    Item::Image {
                        image,
                        rect,
                        link,
                        inline,
                    } => {
                        let r = offset(*rect, x0, uy);
                        if r.b < 0.0 || r.t > self.vh {
                            continue;
                        }
                        self.visible_images.push(*image);
                        self.frame_images.push(FrameImage {
                            rect: r,
                            link: link.clone(),
                        });
                        let size = self.flow.image_size(*image);
                        let need_px = (r.w() * scale).ceil() as u32;
                        let mut drawn = false;
                        if let Some(b) = self.bitmaps.get(image) {
                            unsafe {
                                t.rt.DrawBitmap(
                                    &b.bmp,
                                    Some(&render::rect(r.l, r.t, r.r, r.b)),
                                    1.0,
                                    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                                    None,
                                );
                            }
                            drawn = true;
                            // 더 크게 보여야 하면 다시 디코딩 (원본보다 크게는 하지 않는다)
                            let full = matches!(size, ImgSize::Known(iw, _) if b.pw as f32 >= iw);
                            if !full && (b.pw as f32) < need_px as f32 * 0.9 {
                                request(&mut cmds, &mut self.pending, &self.srcs, *image, need_px);
                            }
                        } else if size != ImgSize::Failed {
                            request(&mut cmds, &mut self.pending, &self.srcs, *image, need_px);
                        }
                        if !drawn {
                            draw_placeholder(
                                t,
                                r,
                                *inline,
                                &self.dwrite,
                                alt_fmt.as_ref(),
                                &self.flow.c.images[*image as usize].alt,
                                matches!(self.srcs.get(*image as usize), Some(ImgSrc::Remote)),
                                size == ImgSize::Failed,
                                em,
                            );
                        }
                    }
                    Item::Text {
                        para,
                        x,
                        y,
                        w,
                        h,
                        handle,
                    } => {
                        let (ax, ay) = (x0 + x, uy + y);
                        if ay > self.vh || ay + h < 0.0 {
                            continue;
                        }
                        let p = &self.flow.c.paras[*para as usize];
                        let Some(layout) = handle.layout() else {
                            continue;
                        };
                        handle.apply_effects(p, t);
                        for r in handle.code_rects(p) {
                            render::fill(
                                t,
                                RectF::new(ax + r.l - 2.0, ay + r.t, ax + r.r + 2.0, ay + r.b),
                                &t.inline_code,
                            );
                        }
                        if let Some((a, b)) = sel
                            && a.0 <= *para
                            && *para <= b.0
                            && a != b
                        {
                            let len = p.text.len() as u32;
                            let s = if a.0 == *para { a.1 } else { 0 };
                            let e = if b.0 == *para { b.1 } else { len };
                            for r in range_rects(layout, s, e.saturating_sub(s), ax, ay) {
                                unsafe { t.rt.FillRectangle(&r, &t.selection) };
                            }
                        }
                        if let Some(s) = &self.search {
                            let mut i = s.hits.partition_point(|h| h.0 < *para);
                            while let Some(&(_, hs, he)) = s.hits.get(i).filter(|h| h.0 == *para) {
                                let brush = if s.current == Some(i) {
                                    &t.hit_current
                                } else {
                                    &t.hit
                                };
                                for r in range_rects(layout, hs, he - hs, ax, ay) {
                                    unsafe { t.rt.FillRectangle(&r, brush) };
                                }
                                i += 1;
                            }
                        }
                        let brush: &ID2D1SolidColorBrush = if p.dim || p.kind == ParaKind::Marker {
                            &t.dim
                        } else {
                            &t.text
                        };
                        render::draw_layout(t, layout, ax, ay, brush);
                        self.frame.push(FrameText {
                            para: *para,
                            rect: RectF::new(ax, ay, ax + w, ay + h),
                            handle: handle.clone(),
                        });
                    }
                }
            }
        }
        if self.first_paint_ms.is_none() {
            self.first_paint_ms = Some(self.opened_at.elapsed().as_secs_f64() * 1000.0);
        }
        cmds
    }
}

fn offset(r: RectF, dx: f32, dy: f32) -> RectF {
    RectF::new(r.l + dx, r.t + dy, r.r + dx, r.b + dy)
}

fn same_file(a: &Path, b: &Path) -> bool {
    a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
}

fn request(
    cmds: &mut Vec<MdCmd>,
    pending: &mut HashSet<ImageId>,
    srcs: &[ImgSrc],
    id: ImageId,
    need_px: u32,
) {
    if pending.contains(&id) {
        return;
    }
    if let Some(ImgSrc::Local(p)) = srcs.get(id as usize) {
        pending.insert(id);
        cmds.push(MdCmd::LoadImage {
            id,
            path: p.clone(),
            max_w: need_px.max(1),
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_placeholder(
    t: &Target,
    r: RectF,
    inline: bool,
    dw: &IDWriteFactory,
    fmt: Option<&IDWriteTextFormat>,
    alt: &str,
    remote: bool,
    failed: bool,
    em: f32,
) {
    render::fill(t, r, &t.placeholder);
    unsafe {
        t.rt.DrawRectangle(
            &render::rect(r.l + 0.5, r.t + 0.5, r.r - 0.5, r.b - 0.5),
            &t.border,
            1.0,
            None,
        );
    }
    if inline || r.w() < 3.0 * em {
        return;
    }
    let label = if remote {
        format!("원격 이미지 차단됨 — {alt}")
    } else if failed {
        format!("이미지를 표시할 수 없음 — {alt}")
    } else {
        format!("불러오는 중… {alt}")
    };
    let Some(fmt) = fmt else { return };
    let text: Vec<u16> = label.encode_utf16().collect();
    if let Ok(l) = unsafe { dw.CreateTextLayout(&text, fmt, (r.w() - em).max(1.0), r.h()) } {
        unsafe {
            let _ = l.SetTrimming(
                &DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                },
                None,
            );
        }
        let m = metrics(&l);
        render::draw_layout(
            t,
            &l,
            r.l + em * 0.5,
            r.t + (r.h() - m.height) * 0.5,
            &t.dim,
        );
    }
}

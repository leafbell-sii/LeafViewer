//! 가상화 블록 레이아웃 (SPEC §4.4).
//!
//! - 단위 높이는 처음엔 추정값(글자 수 × 평균 폭)이고, 화면에 들어온 단위만 실제로 레이아웃한다.
//! - 스크롤 위치는 (화면 맨 위 단위, 그 단위 안의 오프셋)이다. 위쪽 단위의 실제 높이가 추정과
//!   달라도 화면 맨 위 단위가 기준이라 화면이 튀지 않는다. 스크롤바만 추정 합계로 근사한다.
//! - 폭이 바뀌면 캐시를 버리고 높이는 직전 값을 추정치로 남긴다 → 보이는 단위만 즉시 다시 레이아웃.
//! - 레이아웃 캐시는 화면 ±1 화면 범위의 단위만 보유한다 (§4.5).

use std::collections::HashMap;

use leaf_doc::RectF;

use crate::content::{Content, ImageId, Node, Para, ParaId, ParaKind};

/// 본문 글자 크기(pt). 설정 `[markdown] font_size` (§8)
pub const BASE_PT: f32 = 11.0;

pub fn base_px(zoom: f32) -> f32 {
    BASE_PT * 96.0 / 72.0 * zoom
}

/// 문단 종류별 글자 크기(DIP). 측정기(DirectWrite)와 추정이 같은 값을 쓴다.
pub fn font_size(kind: ParaKind, zoom: f32) -> f32 {
    let em = match kind {
        ParaKind::Heading(1) => 2.0,
        ParaKind::Heading(2) => 1.5,
        ParaKind::Heading(3) => 1.25,
        ParaKind::Heading(5) => 0.875,
        ParaKind::Heading(6) => 0.85,
        ParaKind::Code => 0.9,
        _ => 1.0,
    };
    base_px(zoom) * em
}

/// 인라인 코드·각주 참조처럼 문단 안에서 크기만 다른 범위의 배율
pub const CODE_SPAN_EM: f32 = 0.9;
pub const SMALL_SPAN_EM: f32 = 0.75;

// 여백 (본문 em 단위)
const BOTTOM_PAD: f32 = 2.0;
const LIST_GUTTER: f32 = 1.8;
const MARKER_GAP: f32 = 0.45;
const QUOTE_BAR: f32 = 0.25;
const QUOTE_INDENT: f32 = 1.0;
const CODE_PAD: f32 = 0.8;
const CELL_PAD_X: f32 = 0.6;
const CELL_PAD_Y: f32 = 0.35;
const RULE_H: f32 = 1.2;
const RULE_THICK: f32 = 0.2;
const HEADING_RULE_PAD: f32 = 0.3;
/// 이미지 크기를 모를 때·읽지 못했을 때의 자리표시자
const PLACEHOLDER_H: f32 = 2.2;
const PLACEHOLDER_W: f32 = 16.0;

/// 텍스트 측정 (SPEC §4.1: 실제 구현은 leaf-win의 DirectWrite, 테스트는 가짜).
pub trait Measurer {
    type Handle: Clone;
    /// 배율이 바뀌면 글꼴을 다시 만든다
    fn set_zoom(&mut self, zoom: f32);
    /// `objects`: 문단의 인라인 객체 크기(DIP), `para.objects`와 같은 순서
    fn layout(&mut self, para: &Para, width: f32, objects: &[(f32, f32)])
    -> Measured<Self::Handle>;
    /// (최소 폭: 가장 긴 단어, 최대 폭: 줄바꿈 없이)
    fn intrinsic(&mut self, para: &Para) -> (f32, f32);
    /// 글자 위치의 (x, y, 줄 높이), 문단 왼쪽 위 기준
    fn caret(&self, h: &Self::Handle, idx: u32) -> (f32, f32, f32);
    /// 인라인 객체의 왼쪽 위, 문단 왼쪽 위 기준
    fn object_pos(&self, h: &Self::Handle, pos: u32, size: (f32, f32)) -> (f32, f32);
}

pub struct Measured<H> {
    pub handle: H,
    pub h: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ImgSize {
    Unknown,
    /// 원본 크기 (1px = 1DIP로 표시)
    Known(f32, f32),
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillKind {
    CodeBg,
    QuoteBar,
    Rule,
    HeadingRule,
    TableBorder,
    TableHead,
    TableStripe,
}

#[derive(Debug, Clone)]
pub enum Item<H> {
    Text {
        para: ParaId,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        handle: H,
    },
    Fill {
        kind: FillKind,
        rect: RectF,
    },
    Image {
        image: ImageId,
        rect: RectF,
        /// 블록 이미지를 감싼 링크
        link: Option<String>,
        /// 인라인 이미지면 true (자리표시자를 작게 그린다)
        inline: bool,
    },
}

/// 단위 하나의 레이아웃. 좌표는 단위 안쪽 왼쪽 위(간격 제외) 기준.
pub struct BlockLayout<H> {
    pub h: f32,
    pub items: Vec<Item<H>>,
}

pub struct FlowLayout<M: Measurer> {
    pub c: Content,
    pub m: M,
    zoom: f32,
    width: f32,
    view_h: f32,
    heights: Vec<f32>,
    estimated: bool,
    cache: HashMap<usize, BlockLayout<M::Handle>>,
    /// 표 → (계산한 폭, 열 폭들)
    table_cols: HashMap<u32, (f32, Vec<f32>)>,
    images: Vec<ImgSize>,
    anchor: usize,
    off: f32,
    /// 이번 프레임에 실제로 레이아웃한 단위 수 (테스트·계측)
    pub layouts_made: usize,
}

struct Ctx<'a, M: Measurer> {
    c: &'a Content,
    m: &'a mut M,
    images: &'a [ImgSize],
    table_cols: &'a mut HashMap<u32, (f32, Vec<f32>)>,
    em: f32,
}

impl<M: Measurer> FlowLayout<M> {
    pub fn new(c: Content, mut m: M, zoom: f32) -> Self {
        m.set_zoom(zoom);
        let n = c.units.len();
        let images = vec![ImgSize::Unknown; c.images.len()];
        Self {
            c,
            m,
            zoom,
            width: 0.0,
            view_h: 0.0,
            heights: vec![0.0; n],
            estimated: false,
            cache: HashMap::new(),
            table_cols: HashMap::new(),
            images,
            anchor: 0,
            off: 0.0,
            layouts_made: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.c.units.len()
    }

    pub fn is_empty(&self) -> bool {
        self.c.units.is_empty()
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    pub fn width(&self) -> f32 {
        self.width
    }

    pub fn view_h(&self) -> f32 {
        self.view_h
    }

    fn em(&self) -> f32 {
        base_px(self.zoom)
    }

    fn gap_px(&self, u: usize) -> f32 {
        self.c.units[u].gap * self.em()
    }

    fn bottom_px(&self, u: usize) -> f32 {
        if u + 1 == self.len() {
            BOTTOM_PAD * self.em()
        } else {
            0.0
        }
    }

    /// 본문 폭과 화면 높이 (DIP). 폭이 바뀌면 레이아웃을 버린다.
    pub fn set_viewport(&mut self, width: f32, view_h: f32) {
        let width = width.max(40.0);
        self.view_h = view_h;
        if !self.estimated {
            self.width = width;
            self.estimate_all();
            self.estimated = true;
        } else if (width - self.width).abs() > 0.25 {
            self.width = width;
            self.cache.clear();
        }
    }

    pub fn set_zoom(&mut self, zoom: f32) {
        if (zoom - self.zoom).abs() < 1e-4 {
            return;
        }
        let k = zoom / self.zoom;
        self.off *= k;
        for h in &mut self.heights {
            *h *= k;
        }
        self.zoom = zoom;
        self.m.set_zoom(zoom);
        self.cache.clear();
        self.table_cols.clear();
    }

    pub fn image_size(&self, id: ImageId) -> ImgSize {
        self.images
            .get(id as usize)
            .copied()
            .unwrap_or(ImgSize::Failed)
    }

    /// 이미지 크기를 알게 됐을 때: 그 이미지가 든 단위만 다시 레이아웃
    pub fn set_image_size(&mut self, id: ImageId, s: ImgSize) {
        let Some(slot) = self.images.get_mut(id as usize) else {
            return;
        };
        if *slot == s {
            return;
        }
        *slot = s;
        for &u in &self.c.image_units[id as usize] {
            self.cache.remove(&u);
            if self.estimated {
                let mut cx = Ctx {
                    c: &self.c,
                    m: &mut self.m,
                    images: &self.images,
                    table_cols: &mut self.table_cols,
                    em: base_px(self.zoom),
                };
                self.heights[u] = cx.estimate(&self.c.units[u].node, self.width);
            }
        }
    }

    pub fn release(&mut self) {
        self.cache.clear();
    }

    fn estimate_all(&mut self) {
        let mut cx = Ctx {
            c: &self.c,
            m: &mut self.m,
            images: &self.images,
            table_cols: &mut self.table_cols,
            em: base_px(self.zoom),
        };
        for (i, u) in self.c.units.iter().enumerate() {
            self.heights[i] = cx.estimate(&u.node, self.width);
        }
    }

    /// 단위를 레이아웃해 캐시에 넣는다 (이미 있으면 그대로)
    fn ensure(&mut self, u: usize) {
        if self.cache.contains_key(&u) || u >= self.len() {
            return;
        }
        if !self.estimated {
            self.estimate_all();
            self.estimated = true;
        }
        let mut cx = Ctx {
            c: &self.c,
            m: &mut self.m,
            images: &self.images,
            table_cols: &mut self.table_cols,
            em: base_px(self.zoom),
        };
        let mut items = Vec::new();
        let h = cx.layout(&self.c.units[u].node, 0.0, 0.0, self.width, &mut items);
        self.heights[u] = h;
        self.cache.insert(u, BlockLayout { h, items });
        self.layouts_made += 1;
    }

    pub fn block(&self, u: usize) -> Option<&BlockLayout<M::Handle>> {
        self.cache.get(&u)
    }

    pub fn is_laid_out(&self, u: usize) -> bool {
        self.cache.contains_key(&u)
    }

    /// 간격·아래 여백 포함 높이 (레이아웃함)
    fn outer(&mut self, u: usize) -> f32 {
        self.ensure(u);
        self.gap_px(u) + self.heights[u] + self.bottom_px(u)
    }

    fn outer_est(&self, u: usize) -> f32 {
        self.gap_px(u) + self.heights[u] + self.bottom_px(u)
    }

    // ───── 스크롤 ─────

    pub fn position(&self) -> (usize, f32) {
        (self.anchor, self.off)
    }

    pub fn restore(&mut self, unit: usize, off: f32) {
        self.anchor = unit.min(self.len().saturating_sub(1));
        self.off = off.max(0.0);
    }

    pub fn scroll_by(&mut self, d: f32) {
        self.off += d;
        self.rebalance();
        self.clamp_bottom();
    }

    fn rebalance(&mut self) {
        if self.is_empty() {
            self.anchor = 0;
            self.off = 0.0;
            return;
        }
        self.anchor = self.anchor.min(self.len() - 1);
        while self.off < 0.0 {
            if self.anchor == 0 {
                self.off = 0.0;
                break;
            }
            self.anchor -= 1;
            self.off += self.outer(self.anchor);
        }
        loop {
            let h = self.outer(self.anchor);
            if self.off < h {
                break;
            }
            if self.anchor + 1 < self.len() {
                self.off -= h;
                self.anchor += 1;
            } else {
                self.off = (h - 1.0).max(0.0);
                break;
            }
        }
    }

    /// 문서 끝에서는 마지막 단위가 화면 아래에 붙게
    fn clamp_bottom(&mut self) {
        if self.view_h <= 0.0 || self.is_empty() {
            return;
        }
        let mut sum = 0.0;
        let mut u = self.anchor;
        while u < self.len() {
            sum += self.outer(u);
            if sum - self.off >= self.view_h {
                return;
            }
            u += 1;
        }
        let gap = self.view_h - (sum - self.off);
        if gap > 0.0 {
            self.off -= gap;
            while self.off < 0.0 {
                if self.anchor == 0 {
                    self.off = 0.0;
                    break;
                }
                self.anchor -= 1;
                self.off += self.outer(self.anchor);
            }
        }
    }

    pub fn home(&mut self) {
        self.anchor = 0;
        self.off = 0.0;
    }

    pub fn end(&mut self) {
        self.anchor = self.len().saturating_sub(1);
        self.off = 0.0;
        self.scroll_by(f32::MAX / 4.0);
    }

    /// (문서 전체 높이, 현재 위치) — 화면 밖 단위는 추정 높이
    pub fn metrics(&self) -> (f32, f32) {
        let total: f32 = (0..self.len()).map(|u| self.outer_est(u)).sum();
        let pos: f32 = (0..self.anchor.min(self.len()))
            .map(|u| self.outer_est(u))
            .sum::<f32>()
            + self.off;
        (total, pos)
    }

    /// 스크롤바 끌기: 추정 높이 합으로 위치를 찾는다
    pub fn scroll_to(&mut self, y: f32) {
        let mut y = y.max(0.0);
        let mut u = 0;
        while u + 1 < self.len() {
            let h = self.outer_est(u);
            if y < h {
                break;
            }
            y -= h;
            u += 1;
        }
        self.anchor = u;
        self.off = y;
        self.rebalance();
        self.clamp_bottom();
    }

    /// 단위가 화면 맨 위 근처에 오도록 (목차·앵커 이동)
    pub fn go_unit(&mut self, u: usize) {
        if u >= self.len() {
            return;
        }
        self.anchor = u;
        self.off = (self.gap_px(u) - 0.5 * self.em()).max(0.0);
        self.rebalance();
        self.clamp_bottom();
    }

    pub fn at_end(&mut self) -> bool {
        let mut sum = -self.off;
        let mut u = self.anchor;
        while u < self.len() {
            sum += self.outer(u);
            if sum > self.view_h + 0.5 {
                return false;
            }
            u += 1;
        }
        true
    }

    /// 화면에 보이는 단위들과 각 단위 안쪽 위 y(화면 기준, 간격 제외). 보이는 단위만 레이아웃한다.
    pub fn visible(&mut self) -> Vec<(usize, f32)> {
        self.layouts_made = 0;
        self.rebalance();
        self.clamp_bottom();
        let mut out = Vec::new();
        let mut y = -self.off;
        let mut u = self.anchor;
        while u < self.len() && y < self.view_h {
            let h = self.outer(u);
            out.push((u, y + self.gap_px(u)));
            y += h;
            u += 1;
        }
        self.trim(
            out.first().map_or(0, |v| v.0),
            out.last().map_or(0, |v| v.0),
        );
        out
    }

    /// 화면 ±1 화면 범위 밖의 레이아웃은 버린다
    fn trim(&mut self, first: usize, last: usize) {
        let (mut lo, mut hi) = (first, last);
        let mut acc = 0.0;
        while lo > 0 && acc < self.view_h {
            lo -= 1;
            acc += self.outer_est(lo);
        }
        acc = 0.0;
        while hi + 1 < self.len() && acc < self.view_h {
            hi += 1;
            acc += self.outer_est(hi);
        }
        self.cache.retain(|&u, _| u >= lo && u <= hi);
    }

    /// 화면 위에서 `y`(DIP) 지점의 단위
    pub fn unit_at(&mut self, y: f32) -> usize {
        let mut acc = -self.off;
        let mut u = self.anchor;
        while u + 1 < self.len() {
            acc += self.outer(u);
            if acc > y {
                break;
            }
            u += 1;
        }
        u
    }

    /// 문단 안 글자가 화면에 없으면 화면 위쪽 1/3에 오도록 스크롤. 바뀌었으면 true.
    pub fn reveal(&mut self, para: ParaId, idx: u32) -> bool {
        let Some(&u) = self.c.para_unit.get(para as usize) else {
            return false;
        };
        let u = u as usize;
        self.ensure(u);
        let Some((iy, cy, ch)) = self.cache.get(&u).and_then(|b| {
            b.items.iter().find_map(|it| match it {
                Item::Text {
                    para: p, y, handle, ..
                } if *p == para => {
                    let (_, cy, ch) = self.m.caret(handle, idx);
                    Some((*y, cy, ch))
                }
                _ => None,
            })
        }) else {
            return false;
        };
        let in_unit = self.gap_px(u) + iy + cy;
        // 지금 화면 위치
        if u >= self.anchor {
            let mut top = -self.off;
            let mut k = self.anchor;
            while k < u && top < self.view_h {
                top += self.outer(k);
                k += 1;
            }
            let y = top + in_unit;
            if k == u && y >= 0.0 && y + ch <= self.view_h {
                return false;
            }
        }
        self.anchor = u;
        self.off = in_unit - self.view_h / 3.0;
        self.rebalance();
        self.clamp_bottom();
        true
    }
}

impl<M: Measurer> Ctx<'_, M> {
    fn obj_sizes(&self, p: &Para, width: f32) -> Vec<(f32, f32)> {
        p.objects
            .iter()
            .map(|o| match self.images.get(o.image as usize) {
                Some(ImgSize::Known(w, h)) => fit(*w, *h, width),
                _ => (self.em, self.em),
            })
            .collect()
    }

    fn text(
        &mut self,
        para: ParaId,
        x: f32,
        y: f32,
        w: f32,
        out: &mut Vec<Item<M::Handle>>,
    ) -> f32 {
        let p = &self.c.paras[para as usize];
        let objs = self.obj_sizes(p, w);
        let m = self.m.layout(p, w, &objs);
        for (o, size) in p.objects.iter().zip(&objs) {
            let (ox, oy) = self.m.object_pos(&m.handle, o.pos, *size);
            out.push(Item::Image {
                image: o.image,
                rect: RectF::new(x + ox, y + oy, x + ox + size.0, y + oy + size.1),
                link: None,
                inline: true,
            });
        }
        out.push(Item::Text {
            para,
            x,
            y,
            w,
            h: m.h,
            handle: m.handle,
        });
        m.h
    }

    fn fill(out: &mut Vec<Item<M::Handle>>, kind: FillKind, l: f32, t: f32, r: f32, b: f32) {
        out.push(Item::Fill {
            kind,
            rect: RectF::new(l, t, r, b),
        });
    }

    /// 노드를 (x, y)에서 폭 w로 배치하고 높이를 돌려준다
    fn layout(&mut self, n: &Node, x: f32, y: f32, w: f32, out: &mut Vec<Item<M::Handle>>) -> f32 {
        let em = self.em;
        let w = w.max(em);
        match n {
            Node::Para(p) => self.text(*p, x, y, w, out),
            Node::Heading { para, level } => {
                let h = self.text(*para, x, y, w, out);
                if *level <= 2 {
                    let ly = y + h + HEADING_RULE_PAD * em;
                    Self::fill(out, FillKind::HeadingRule, x, ly, x + w, ly + 1.0);
                    h + HEADING_RULE_PAD * em + 1.0
                } else {
                    h
                }
            }
            Node::Stack(v) => {
                let mut cy = y;
                for (g, c) in v {
                    cy += g * em;
                    cy += self.layout(c, x, cy, w, out);
                }
                cy - y
            }
            Node::Item { marker, child } => {
                let gutter = self.gutter(*marker);
                let mh = self.text(*marker, x, y, (gutter - MARKER_GAP * em).max(em * 0.5), out);
                let ch = self.layout(child, x + gutter, y, w - gutter, out);
                mh.max(ch)
            }
            Node::Quote(c) => {
                let h = self.layout(c, x + QUOTE_INDENT * em, y, w - QUOTE_INDENT * em, out);
                Self::fill(out, FillKind::QuoteBar, x, y, x + QUOTE_BAR * em, y + h);
                h
            }
            Node::Code { para, first, last } => {
                let pad = CODE_PAD * em;
                let top = if *first { pad } else { 0.0 };
                let bottom = if *last { pad } else { 0.0 };
                // 배경을 글보다 먼저 그리도록 자리를 잡아 둔다
                let bg = out.len();
                Self::fill(out, FillKind::CodeBg, x, y, x + w, y);
                let th = self.text(*para, x + pad, y + top, w - 2.0 * pad, out);
                let h = top + th + bottom;
                out[bg] = Item::Fill {
                    kind: FillKind::CodeBg,
                    rect: RectF::new(x, y, x + w, y + h),
                };
                h
            }
            Node::Table {
                table,
                head,
                rows,
                row0,
                first,
                last: _,
            } => self.table(*table, head.as_deref(), rows, *row0, *first, x, y, w, out),
            Node::Rule => {
                let ly = y + (RULE_H - RULE_THICK) * 0.5 * em;
                Self::fill(out, FillKind::Rule, x, ly, x + w, ly + RULE_THICK * em);
                RULE_H * em
            }
            Node::Image { image, link } => {
                let (iw, ih) = self.block_image_size(*image, w);
                out.push(Item::Image {
                    image: *image,
                    rect: RectF::new(x, y, x + iw, y + ih),
                    link: link.clone(),
                    inline: false,
                });
                ih
            }
        }
    }

    fn block_image_size(&self, image: ImageId, w: f32) -> (f32, f32) {
        match self.images.get(image as usize) {
            Some(ImgSize::Known(iw, ih)) => fit(*iw, *ih, w),
            _ => ((PLACEHOLDER_W * self.em).min(w), PLACEHOLDER_H * self.em),
        }
    }

    fn gutter(&mut self, marker: ParaId) -> f32 {
        let em = self.em;
        let p = &self.c.paras[marker as usize];
        // 짧은 표식은 측정하지 않고 고정 폭 (번호가 길면 넓힌다)
        if p.text.len() <= 3 {
            LIST_GUTTER * em
        } else {
            let (_, max) = self.m.intrinsic(p);
            (max + MARKER_GAP * em).max(LIST_GUTTER * em)
        }
    }

    fn table_cols(&mut self, table: u32, w: f32) -> Vec<f32> {
        if let Some((cw, cols)) = self.table_cols.get(&table)
            && (*cw - w).abs() < 0.25
        {
            return cols.clone();
        }
        let info = &self.c.tables[table as usize];
        let pad = 2.0 * CELL_PAD_X * self.em;
        let mut mins = vec![0.0f32; info.cols];
        let mut maxs = vec![0.0f32; info.cols];
        for row in &info.sample {
            for (ci, &p) in row.iter().enumerate().take(info.cols) {
                let (mn, mx) = self.m.intrinsic(&self.c.paras[p as usize]);
                mins[ci] = mins[ci].max(mn + pad);
                maxs[ci] = maxs[ci].max(mx + pad);
            }
        }
        let cols = distribute(&mins, &maxs, w);
        self.table_cols.insert(table, (w, cols.clone()));
        cols
    }

    #[allow(clippy::too_many_arguments)]
    fn table(
        &mut self,
        table: u32,
        head: Option<&[ParaId]>,
        rows: &[Vec<ParaId>],
        row0: usize,
        first: bool,
        x: f32,
        y: f32,
        w: f32,
        out: &mut Vec<Item<M::Handle>>,
    ) -> f32 {
        let em = self.em;
        let cols = self.table_cols(table, w);
        let tw: f32 = cols.iter().sum();
        let (px, py) = (CELL_PAD_X * em, CELL_PAD_Y * em);
        let mut cy = y;
        if first {
            Self::fill(out, FillKind::TableBorder, x, cy, x + tw, cy + 1.0);
        }
        let all_rows = head
            .into_iter()
            .map(|h| (true, h))
            .chain(rows.iter().map(|r| (false, r.as_slice())));
        let mut body_i = row0;
        for (is_head, row) in all_rows {
            let bg = out.len();
            Self::fill(out, FillKind::TableStripe, x, cy, x, cy);
            let mut cx = x;
            let mut rh: f32 = 0.0;
            for (ci, &p) in row.iter().enumerate() {
                let cw = cols.get(ci).copied().unwrap_or(0.0);
                rh = rh.max(self.text(p, cx + px, cy + py, (cw - 2.0 * px).max(1.0), out));
                cx += cw;
            }
            let rh = rh + 2.0 * py;
            let kind = if is_head {
                Some(FillKind::TableHead)
            } else if body_i % 2 == 1 {
                Some(FillKind::TableStripe)
            } else {
                None
            };
            out[bg] = Item::Fill {
                kind: kind.unwrap_or(FillKind::TableStripe),
                rect: if kind.is_some() {
                    RectF::new(x, cy, x + tw, cy + rh)
                } else {
                    RectF::default()
                },
            };
            if !is_head {
                body_i += 1;
            }
            cy += rh;
            Self::fill(out, FillKind::TableBorder, x, cy, x + tw, cy + 1.0);
        }
        let h = cy - y + 1.0;
        // 세로 선
        let mut vx = x;
        for (i, cw) in cols.iter().enumerate() {
            if i == 0 {
                Self::fill(out, FillKind::TableBorder, vx, y, vx + 1.0, y + h);
            }
            vx += cw;
            Self::fill(out, FillKind::TableBorder, vx - 1.0, y, vx, y + h);
        }
        h
    }

    // ───── 높이 추정 (레이아웃 없이) ─────

    fn est_para(&self, p: ParaId, w: f32) -> f32 {
        let p = &self.c.paras[p as usize];
        let fs = font_size(p.kind, 1.0) / base_px(1.0) * self.em;
        let line_h = fs * 1.35;
        let w = w.max(fs);
        if p.kind == ParaKind::Code {
            // 줄 수 기준 (긴 줄은 접힌다고 보고 한 줄 더)
            let lines = p.text.iter().filter(|&&u| u == b'\n' as u16).count() + 1;
            let per_line = (p.em * fs / lines as f32 / w).ceil().max(1.0);
            return lines as f32 * per_line * line_h;
        }
        let breaks = p
            .text
            .iter()
            .filter(|&&u| u == crate::content::LINE_SEP)
            .count() as f32;
        let mut lines = (p.em * fs / w).ceil().max(1.0) + breaks;
        let mut extra = 0.0;
        for o in &p.objects {
            if let Some(ImgSize::Known(iw, ih)) = self.images.get(o.image as usize) {
                let (_, h) = fit(*iw, *ih, w);
                if h > line_h {
                    extra += h - line_h;
                    lines = lines.max(1.0);
                }
            }
        }
        lines * line_h + extra
    }

    fn estimate(&mut self, n: &Node, w: f32) -> f32 {
        let em = self.em;
        let w = w.max(em);
        match n {
            Node::Para(p) => self.est_para(*p, w),
            Node::Heading { para, level } => {
                self.est_para(*para, w)
                    + if *level <= 2 {
                        HEADING_RULE_PAD * em + 1.0
                    } else {
                        0.0
                    }
            }
            Node::Stack(v) => v.iter().map(|(g, c)| g * em + self.estimate(c, w)).sum(),
            Node::Item { marker, child } => {
                let g = LIST_GUTTER * em;
                self.est_para(*marker, g).max(self.estimate(child, w - g))
            }
            Node::Quote(c) => self.estimate(c, w - QUOTE_INDENT * em),
            Node::Code { para, first, last } => {
                let pad = CODE_PAD * em;
                self.est_para(*para, w - 2.0 * pad)
                    + if *first { pad } else { 0.0 }
                    + if *last { pad } else { 0.0 }
            }
            Node::Table { head, rows, .. } => {
                let cols = head.as_ref().or(rows.first()).map_or(1, Vec::len).max(1);
                let cw = w / cols as f32;
                let row_h = |r: &Vec<ParaId>| {
                    r.iter()
                        .map(|&p| self.est_para(p, cw - 2.0 * CELL_PAD_X * em))
                        .fold(0.0, f32::max)
                        + 2.0 * CELL_PAD_Y * em
                };
                head.iter().chain(rows.iter()).map(row_h).sum::<f32>() + 1.0
            }
            Node::Rule => RULE_H * em,
            Node::Image { image, .. } => self.block_image_size(*image, w).1,
        }
    }
}

/// 폭 `w` 안에 들어가게 비율 유지 축소 (확대하지 않는다)
fn fit(iw: f32, ih: f32, w: f32) -> (f32, f32) {
    if iw <= 0.0 || ih <= 0.0 {
        return (1.0, 1.0);
    }
    if iw <= w { (iw, ih) } else { (w, ih * w / iw) }
}

/// 자동 표 레이아웃. 최대 폭(줄바꿈 없는 폭)이 다 들어가면 그대로 쓴다. 아니면 최대 폭이 공평한
/// 몫 이하인 좁은 열에 최대 폭을 먼저 주고(한글 머리글이 한 글자씩 접히지 않게), 남은 폭은 넓은
/// 열들에 최대 폭 비율로 나눈 뒤, 최소 폭(가장 긴 단어)보다 좁아진 열은 최소 폭으로 올리고 그만큼
/// 여유 있는 열에서 뺀다. 최소 폭 합도 넘치면 최소 폭 비율로 줄인다.
pub fn distribute(mins: &[f32], maxs: &[f32], avail: f32) -> Vec<f32> {
    let n = mins.len();
    let smin: f32 = mins.iter().sum();
    let smax: f32 = maxs.iter().sum();
    if smax <= avail {
        return maxs.to_vec();
    }
    if smin >= avail {
        let k = if smin > 0.0 { avail / smin } else { 0.0 };
        return mins.iter().map(|m| m * k).collect();
    }
    let mut fixed = vec![false; n];
    let (mut rem, mut cnt) = (avail, n);
    loop {
        let share = rem / cnt.max(1) as f32;
        let mut changed = false;
        for i in 0..n {
            if !fixed[i] && maxs[i] <= share {
                fixed[i] = true;
                rem -= maxs[i];
                cnt -= 1;
                changed = true;
            }
        }
        if !changed || cnt == 0 {
            break;
        }
    }
    let rest: f32 = (0..n).filter(|&i| !fixed[i]).map(|i| maxs[i]).sum();
    let mut out: Vec<f32> = (0..n)
        .map(|i| {
            if fixed[i] {
                maxs[i]
            } else {
                rem * maxs[i] / rest.max(1e-3)
            }
        })
        .collect();
    let deficit: f32 = (0..n).map(|i| (mins[i] - out[i]).max(0.0)).sum();
    if deficit > 0.0 {
        let spare: f32 = (0..n).map(|i| (out[i] - mins[i]).max(0.0)).sum();
        let k = (deficit / spare.max(1e-3)).min(1.0);
        for i in 0..n {
            out[i] = if out[i] < mins[i] {
                mins[i]
            } else {
                out[i] - (out[i] - mins[i]) * k
            };
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::Content;
    use leaf_doc::flow::{Block, FlowDoc, Inline, ListItem};

    /// 고정폭 가짜 측정기: 글자 하나 = 글자 크기 × 0.5, 줄 높이 = 크기 × 1.25, 글자 단위 줄바꿈
    #[derive(Default)]
    struct Fake {
        zoom: f32,
        calls: usize,
    }

    #[derive(Clone)]
    struct FH {
        cw: f32,
        lh: f32,
        per_line: usize,
    }

    impl Measurer for Fake {
        type Handle = FH;
        fn set_zoom(&mut self, z: f32) {
            self.zoom = z;
        }
        fn layout(&mut self, p: &Para, width: f32, _o: &[(f32, f32)]) -> Measured<FH> {
            self.calls += 1;
            let fs = font_size(p.kind, self.zoom);
            let cw = fs * 0.5;
            let per_line = ((width / cw).floor() as usize).max(1);
            let lines = p.text.len().div_ceil(per_line).max(1);
            Measured {
                handle: FH {
                    cw,
                    lh: fs * 1.25,
                    per_line,
                },
                h: lines as f32 * fs * 1.25,
            }
        }
        fn intrinsic(&mut self, p: &Para) -> (f32, f32) {
            let cw = font_size(p.kind, self.zoom) * 0.5;
            let longest = String::from_utf16_lossy(&p.text)
                .split(' ')
                .map(|w| w.chars().count())
                .max()
                .unwrap_or(0);
            (longest as f32 * cw, p.text.len() as f32 * cw)
        }
        fn caret(&self, h: &FH, idx: u32) -> (f32, f32, f32) {
            let i = idx as usize;
            (
                (i % h.per_line) as f32 * h.cw,
                (i / h.per_line) as f32 * h.lh,
                h.lh,
            )
        }
        fn object_pos(&self, h: &FH, pos: u32, _s: (f32, f32)) -> (f32, f32) {
            let (x, y, _) = self.caret(h, pos);
            (x, y)
        }
    }

    fn doc(n: usize) -> FlowDoc {
        FlowDoc {
            blocks: (0..n)
                .map(|i| {
                    if i % 10 == 0 {
                        Block::Heading {
                            level: 2,
                            inlines: vec![Inline::Text(format!("H{i}"))],
                            anchor: format!("h{i}"),
                        }
                    } else {
                        Block::Paragraph(vec![Inline::Text(format!("{i} ").repeat(30))])
                    }
                })
                .collect(),
            ..Default::default()
        }
    }

    fn flow(n: usize) -> FlowLayout<Fake> {
        let mut f = FlowLayout::new(Content::build(&doc(n)), Fake::default(), 1.0);
        f.set_viewport(600.0, 400.0);
        f
    }

    #[test]
    fn only_visible_units_are_laid_out() {
        let mut f = flow(5000);
        let vis = f.visible();
        assert!(!vis.is_empty() && vis.len() < 30, "{}", vis.len());
        assert_eq!(f.layouts_made, vis.len());
        assert!(f.cache.len() <= vis.len() + 1);
        // 끝으로 가도 중간 단위는 레이아웃하지 않는다
        f.end();
        let vis = f.visible();
        assert_eq!(vis.last().unwrap().0, 4999);
        assert!(f.m.calls < 200, "calls={}", f.m.calls);
        assert!(f.at_end());
    }

    #[test]
    fn scroll_is_anchored_and_clamped() {
        let mut f = flow(300);
        f.visible();
        f.scroll_by(1000.0);
        let before = f.visible();
        let (u0, y0) = before[0];
        // 폭이 바뀌어 위쪽 단위 높이가 달라져도 화면 맨 위 단위는 그대로
        f.set_viewport(300.0, 400.0);
        let after = f.visible();
        assert_eq!(after[0].0, u0);
        assert!((after[0].1 - y0).abs() < 1e-3 || after[0].1 <= 0.0);
        // 위로 많이 올리면 0에서 멈춘다
        f.scroll_by(-1e9);
        assert_eq!(f.position(), (0, 0.0));
        // 아래로 많이 내리면 마지막 단위가 화면 아래에 붙는다
        f.scroll_by(1e9);
        let vis = f.visible();
        let (last, y) = *vis.last().unwrap();
        assert_eq!(last, 299);
        let bottom = y + f.block(last).unwrap().h + BOTTOM_PAD * base_px(1.0);
        assert!((bottom - 400.0).abs() < 0.5, "{bottom}");
    }

    #[test]
    fn metrics_and_scroll_to_roundtrip() {
        let mut f = flow(200);
        f.visible();
        let (total, pos) = f.metrics();
        assert!(total > 400.0 && pos == 0.0);
        f.scroll_to(total * 0.5);
        let (_, p2) = f.metrics();
        assert!(
            (p2 - total * 0.5).abs() / total < 0.05,
            "{p2} vs {}",
            total * 0.5
        );
    }

    #[test]
    fn reveal_and_go_unit() {
        let mut f = flow(500);
        f.visible();
        let target = f.c.first_para_of(321).unwrap();
        assert!(f.reveal(target, 5));
        let vis = f.visible();
        assert!(vis.iter().any(|(u, _)| *u == 321));
        // 이미 보이면 움직이지 않는다
        assert!(!f.reveal(target, 5));
        f.go_unit(f.c.anchors["h100"]);
        assert_eq!(f.visible()[0].0, 100);
    }

    #[test]
    fn zoom_scales_position() {
        let mut f = flow(100);
        f.visible();
        f.scroll_by(150.0);
        let (u, off) = f.position();
        f.set_zoom(2.0);
        assert_eq!(f.position().0, u);
        assert!((f.position().1 - off * 2.0).abs() < 1e-3);
    }

    #[test]
    fn table_distribution() {
        assert_eq!(
            distribute(&[10.0, 10.0], &[50.0, 30.0], 100.0),
            vec![50.0, 30.0]
        );
        assert_eq!(
            distribute(&[60.0, 40.0], &[100.0, 100.0], 50.0),
            vec![30.0, 20.0]
        );
        let d = distribute(&[10.0, 10.0], &[110.0, 30.0], 70.0);
        assert!((d.iter().sum::<f32>() - 70.0).abs() < 1e-3);
        assert_eq!(d, vec![40.0, 30.0]);
        // 짧은 머리글 열은 최대 폭, 긴 설명 열이 나머지
        let d = distribute(
            &[30.0, 20.0, 30.0, 60.0],
            &[90.0, 60.0, 60.0, 2000.0],
            900.0,
        );
        assert_eq!(d, vec![90.0, 60.0, 60.0, 690.0]);
        // 비례로 나눈 몫이 최소 폭보다 작으면 최소 폭을 지킨다
        let d = distribute(&[10.0, 300.0], &[1000.0, 400.0], 500.0);
        assert!(
            d[1] >= 300.0 - 1e-3 && (d.iter().sum::<f32>() - 500.0).abs() < 1e-2,
            "{d:?}"
        );
    }

    #[test]
    fn table_and_list_layout_items() {
        let d = FlowDoc {
            blocks: vec![
                Block::Table {
                    align: vec![],
                    head: vec![
                        vec![Inline::Text("a".into())],
                        vec![Inline::Text("bb".into())],
                    ],
                    rows: vec![vec![
                        vec![Inline::Text("1".into())],
                        vec![Inline::Text("2".into())],
                    ]],
                },
                Block::List {
                    start: None,
                    tight: true,
                    items: vec![ListItem {
                        task: None,
                        blocks: vec![Block::Paragraph(vec![Inline::Text("x".into())])],
                    }],
                },
                Block::Paragraph(vec![Inline::Image {
                    src: "a.png".into(),
                    alt: "".into(),
                }]),
            ],
            ..Default::default()
        };
        let mut f = FlowLayout::new(Content::build(&d), Fake::default(), 1.0);
        f.set_viewport(500.0, 800.0);
        f.visible();
        let t = f.block(0).unwrap();
        let texts = t
            .items
            .iter()
            .filter(|i| matches!(i, Item::Text { .. }))
            .count();
        assert_eq!(texts, 4);
        assert!(t.items.iter().any(|i| matches!(
            i,
            Item::Fill {
                kind: FillKind::TableHead,
                ..
            }
        )));
        // 목록: 표식은 왼쪽, 내용은 들여쓰기
        let l = f.block(1).unwrap();
        let xs: Vec<f32> = l
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Text { x, .. } => Some(*x),
                _ => None,
            })
            .collect();
        assert_eq!(xs.len(), 2);
        assert!(xs[1] > xs[0]);
        // 이미지: 크기를 모르면 자리표시자, 알면 원본 크기(폭에 맞춰 축소)
        let h0 = f.block(2).unwrap().h;
        f.set_image_size(0, ImgSize::Known(1000.0, 500.0));
        f.visible();
        let Item::Image { rect, .. } = &f.block(2).unwrap().items[0] else {
            panic!()
        };
        assert_eq!((rect.w(), rect.h()), (500.0, 250.0));
        assert!(h0 != 250.0);
    }
}

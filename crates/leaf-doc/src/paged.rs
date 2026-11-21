//! 고정 페이지 문서(PDF)의 연속 세로 레이아웃과 타일 계획 (FR-P01, FR-P02, SPEC §4.5).
//!
//! 단위: 쪽 크기는 pt(1/72인치), 화면 배치는 DIP(1/96인치). 배율 1.0 = 실제 크기(100%).

use std::ops::Range;

use crate::RectF;

pub const PT_TO_DIP: f32 = 96.0 / 72.0;
pub const MARGIN: f32 = 16.0;
pub const GAP: f32 = 12.0;
pub const ZOOM_MIN: f32 = 0.1;
pub const ZOOM_MAX: f32 = 8.0;

/// 이 픽셀 수 이하면 쪽 전체를 한 비트맵으로, 넘으면 타일로 렌더한다 (≈ 24MB).
pub const WHOLE_PAGE_MAX_PX: u64 = 6_000_000;
/// 고배율 타일 한 변 (SPEC §4.5).
pub const TILE_PX: u32 = 512;
/// 점진 표시용 저해상도 쪽 폭(px). 배율과 무관하게 고정해 배율을 바꿔도 즉시 재사용된다 (FR-P04).
pub const LOW_RES_W: u32 = 256;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ZoomMode {
    FitWidth,
    FitPage,
    /// 1.0 = 100%
    Custom(f32),
}

/// 화면의 한 점을 쪽 기준으로 기억해, 재배치 후에도 같은 내용이 같은 자리에 오게 한다.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anchor {
    pub page: usize,
    pub x_pt: f32,
    pub y_pt: f32,
}

#[derive(Debug, Clone)]
pub struct PageLayout {
    sizes: Vec<(f32, f32)>,
    max_w_pt: f32,
    zoom: f32,
    content_w: f32,
    tops: Vec<f32>,
    total_h: f32,
}

impl PageLayout {
    pub fn new(sizes_pt: Vec<(f32, f32)>) -> Self {
        let sizes: Vec<(f32, f32)> = sizes_pt
            .into_iter()
            .map(|(w, h)| (w.max(1.0), h.max(1.0)))
            .collect();
        let max_w_pt = sizes.iter().map(|s| s.0).fold(1.0, f32::max);
        let mut l = Self {
            sizes,
            max_w_pt,
            zoom: 1.0,
            content_w: 0.0,
            tops: Vec::new(),
            total_h: 0.0,
        };
        l.set(1.0, 0.0);
        l
    }

    pub fn page_count(&self) -> usize {
        self.sizes.len()
    }

    pub fn page_size_pt(&self, page: usize) -> (f32, f32) {
        self.sizes.get(page).copied().unwrap_or((612.0, 792.0))
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// DIP per pt
    pub fn scale(&self) -> f32 {
        self.zoom * PT_TO_DIP
    }

    pub fn resolve_zoom(&self, mode: ZoomMode, view_w: f32, view_h: f32, page: usize) -> f32 {
        let fit_w = (view_w - 2.0 * MARGIN).max(10.0) / (self.max_w_pt * PT_TO_DIP);
        let z = match mode {
            ZoomMode::FitWidth => fit_w,
            ZoomMode::FitPage => {
                let (w, h) = self.page_size_pt(page);
                let zw = (view_w - 2.0 * MARGIN).max(10.0) / (w * PT_TO_DIP);
                let zh = (view_h - 2.0 * MARGIN).max(10.0) / (h * PT_TO_DIP);
                zw.min(zh)
            }
            ZoomMode::Custom(z) => z,
        };
        z.clamp(ZOOM_MIN, ZOOM_MAX)
    }

    /// 배율과 보기 폭으로 다시 배치한다.
    pub fn set(&mut self, zoom: f32, view_w: f32) {
        self.zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        let k = self.scale();
        self.content_w = view_w.max(self.max_w_pt * k + 2.0 * MARGIN);
        self.tops.clear();
        let mut y = MARGIN;
        for &(_, h) in &self.sizes {
            self.tops.push(y);
            y += h * k + GAP;
        }
        self.total_h = if self.sizes.is_empty() {
            0.0
        } else {
            y - GAP + MARGIN
        };
    }

    pub fn total_size(&self) -> (f32, f32) {
        (self.content_w, self.total_h)
    }

    /// 쪽 사각형 (DIP, 문서 좌표). 가로는 가운데 정렬.
    pub fn page_rect(&self, page: usize) -> RectF {
        let (w, h) = self.page_size_pt(page);
        let k = self.scale();
        let (w, h) = (w * k, h * k);
        let x = ((self.content_w - w) * 0.5).max(MARGIN);
        let y = self.tops.get(page).copied().unwrap_or(0.0);
        RectF::new(x, y, x + w, y + h)
    }

    /// 문서 y 구간 [y0, y1)과 겹치는 쪽 범위.
    pub fn visible(&self, y0: f32, y1: f32) -> Range<usize> {
        let k = self.scale();
        let start = self
            .tops
            .iter()
            .zip(&self.sizes)
            .position(|(t, s)| t + s.1 * k > y0)
            .unwrap_or(self.sizes.len());
        let end = self.tops.partition_point(|&t| t < y1);
        start..end.max(start)
    }

    /// y에 있는(또는 그 아래 가장 가까운) 쪽. 쪽 사이 간격은 위쪽 쪽에 속한다.
    pub fn page_at(&self, y: f32) -> usize {
        self.tops.partition_point(|&t| t <= y).saturating_sub(1)
    }

    pub fn to_page_pt(&self, page: usize, x: f32, y: f32) -> (f32, f32) {
        let r = self.page_rect(page);
        let k = self.scale();
        ((x - r.l) / k, (y - r.t) / k)
    }

    pub fn to_doc(&self, page: usize, x_pt: f32, y_pt: f32) -> (f32, f32) {
        let r = self.page_rect(page);
        let k = self.scale();
        (r.l + x_pt * k, r.t + y_pt * k)
    }

    pub fn anchor(&self, x: f32, y: f32) -> Anchor {
        let page = self.page_at(y);
        let (x_pt, y_pt) = self.to_page_pt(page, x, y);
        Anchor { page, x_pt, y_pt }
    }

    pub fn doc_pos(&self, a: Anchor) -> (f32, f32) {
        self.to_doc(a.page, a.x_pt, a.y_pt)
    }

    /// 쪽 하나의 렌더 픽셀 크기 (현재 배율 + DPI).
    pub fn page_px(&self, page: usize, dpi: f32) -> (u32, u32) {
        let (w, h) = self.page_size_pt(page);
        let k = self.scale() * dpi / 96.0;
        let wp = (w * k).round().max(1.0);
        // 높이는 폭과 같은 비율로 구해 PDF 스레드와 정확히 맞춘다
        let hp = (wp * h / w).round().max(1.0);
        (wp as u32, hp as u32)
    }
}

/// 렌더 단위. `page_w_px`가 배율 키 역할을 한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub page: u32,
    pub page_w_px: u32,
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileSpec {
    pub key: TileKey,
    pub page_h_px: u32,
    pub w: u32,
    pub h: u32,
}

/// 저해상도 쪽 크기
pub fn low_res_px(w_pt: f32, h_pt: f32) -> (u32, u32) {
    let w = LOW_RES_W;
    let h = ((w as f32) * h_pt / w_pt.max(1.0)).round().max(1.0) as u32;
    (w, h)
}

/// 쪽(px 크기 `pw`×`ph`)에서 보이는 픽셀 구간 `vis`(쪽 기준 px, l/t/r/b)를 덮는 타일 목록.
/// 작은 쪽은 쪽 전체 한 장, 큰 쪽은 `TILE_PX` 격자.
pub fn plan_tiles(page: u32, pw: u32, ph: u32, vis: RectF) -> Vec<TileSpec> {
    if u64::from(pw) * u64::from(ph) <= WHOLE_PAGE_MAX_PX {
        return vec![TileSpec {
            key: TileKey {
                page,
                page_w_px: pw,
                x: 0,
                y: 0,
            },
            page_h_px: ph,
            w: pw,
            h: ph,
        }];
    }
    let clamp = |v: f32, max: u32| v.max(0.0).min(max as f32);
    let (x0, x1) = (clamp(vis.l, pw), clamp(vis.r, pw));
    let (y0, y1) = (clamp(vis.t, ph), clamp(vis.b, ph));
    if x1 <= x0 || y1 <= y0 {
        return Vec::new();
    }
    let t = TILE_PX;
    let (cx0, cx1) = (x0 as u32 / t, (x1.ceil() as u32).div_ceil(t));
    let (cy0, cy1) = (y0 as u32 / t, (y1.ceil() as u32).div_ceil(t));
    let mut out = Vec::new();
    for cy in cy0..cy1 {
        for cx in cx0..cx1 {
            let (x, y) = (cx * t, cy * t);
            out.push(TileSpec {
                key: TileKey {
                    page,
                    page_w_px: pw,
                    x,
                    y,
                },
                page_h_px: ph,
                w: t.min(pw - x),
                h: t.min(ph - y),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn letter(n: usize) -> PageLayout {
        PageLayout::new(vec![(612.0, 792.0); n])
    }

    #[test]
    fn stacks_pages_with_gap() {
        let mut l = letter(3);
        l.set(1.0, 2000.0);
        let k = PT_TO_DIP;
        let r0 = l.page_rect(0);
        let r1 = l.page_rect(1);
        assert_eq!(r0.t, MARGIN);
        assert!((r1.t - (MARGIN + 792.0 * k + GAP)).abs() < 1e-3);
        assert!((r0.w() - 612.0 * k).abs() < 1e-3);
        // 가운데 정렬
        assert!(((r0.l + r0.r) / 2.0 - 1000.0).abs() < 1e-3);
        assert!((l.total_size().1 - (MARGIN * 2.0 + 3.0 * 792.0 * k + 2.0 * GAP)).abs() < 1e-2);
    }

    #[test]
    fn fit_width_and_page() {
        let l = letter(2);
        let z = l.resolve_zoom(ZoomMode::FitWidth, 1000.0, 600.0, 0);
        assert!(((612.0 * PT_TO_DIP * z) - (1000.0 - 2.0 * MARGIN)).abs() < 1e-2);
        let zp = l.resolve_zoom(ZoomMode::FitPage, 1000.0, 600.0, 0);
        assert!(((792.0 * PT_TO_DIP * zp) - (600.0 - 2.0 * MARGIN)).abs() < 1e-2);
        assert_eq!(
            l.resolve_zoom(ZoomMode::Custom(100.0), 1.0, 1.0, 0),
            ZOOM_MAX
        );
    }

    #[test]
    fn visible_range_and_page_at() {
        let mut l = letter(10);
        l.set(1.0, 1000.0);
        let h = 792.0 * PT_TO_DIP + GAP;
        assert_eq!(l.visible(0.0, 10.0), 0..0); // 첫 쪽 위 여백
        assert_eq!(l.visible(0.0, 30.0), 0..1);
        assert_eq!(l.visible(h * 2.5, h * 4.2), 2..5);
        assert_eq!(l.page_at(h * 3.5), 3);
        assert_eq!(l.page_at(-5.0), 0);
        assert_eq!(l.page_at(1e9), 9);
        assert_eq!(l.visible(1e9, 2e9), 10..10);
    }

    #[test]
    fn anchor_survives_zoom() {
        let mut l = letter(5);
        l.set(1.0, 1000.0);
        let a = l.anchor(500.0, 3000.0);
        l.set(2.5, 1000.0);
        let (x, y) = l.doc_pos(a);
        let a2 = l.anchor(x, y);
        assert_eq!(a.page, a2.page);
        assert!((a.y_pt - a2.y_pt).abs() < 1e-2);
        assert!((a.x_pt - a2.x_pt).abs() < 1e-2);
    }

    #[test]
    fn whole_page_when_small_tiles_when_large() {
        let t = plan_tiles(0, 1000, 1300, RectF::new(0.0, 0.0, 1000.0, 600.0));
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].w, t[0].h), (1000, 1300));

        // 4000×5000 = 20M px → 타일. 보이는 영역 [600,1500)×[1000,1600)
        let t = plan_tiles(2, 4000, 5000, RectF::new(600.0, 1000.0, 1500.0, 1600.0));
        let xs: Vec<u32> = t.iter().map(|s| s.key.x).collect();
        assert!(xs.contains(&512) && xs.contains(&1024));
        assert_eq!(t.len(), 2 * 3); // x: 512,1024 · y: 512,1024,1536
        // 가장자리 타일은 잘린다
        let t = plan_tiles(0, 4000, 5000, RectF::new(3900.0, 4900.0, 4000.0, 5000.0));
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].w, t[0].h), (4000 - 3584, 5000 - 4608));
    }

    #[test]
    fn page_px_keeps_aspect() {
        let mut l = letter(1);
        l.set(1.0, 800.0);
        let (w, h) = l.page_px(0, 144.0);
        assert_eq!(w, 1224);
        assert_eq!(h, 1584);
    }
}

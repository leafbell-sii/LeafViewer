//! 이미지 한 장의 확대·스크롤 배치. 단위: 화면 배치는 DIP, 이미지는 (방향을 적용한) 픽셀.
//!
//! 배율 1.0 = 이미지 1px이 화면 1px (실제 픽셀). DPI가 높으면 DIP로는 더 작다.
//! 맞춤(`Fit`)은 창보다 큰 이미지만 줄이고, 작은 이미지는 실제 크기로 가운데에 둔다.

/// 이미지 둘레 여백(DIP)
pub const PAD: f32 = 12.0;
pub const ZOOM_MAX: f32 = 32.0;
/// 맞춤 배율이 이보다 작으면 맞춤 배율까지만 줄일 수 있다
pub const ZOOM_MIN: f32 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Zoom {
    /// 창에 맞춤(줄이기만)
    Fit,
    /// 1.0 = 실제 픽셀
    Custom(f32),
}

#[derive(Debug, Clone)]
pub struct ImageLayout {
    iw: f32,
    ih: f32,
    vw: f32,
    vh: f32,
    dpi: f32,
    mode: Zoom,
    zoom: f32,
    sx: f32,
    sy: f32,
}

impl ImageLayout {
    /// `iw`·`ih`: 방향을 적용한 이미지 크기(px)
    pub fn new(iw: u32, ih: u32) -> Self {
        let mut l = Self {
            iw: iw.max(1) as f32,
            ih: ih.max(1) as f32,
            vw: 800.0,
            vh: 600.0,
            dpi: 96.0,
            mode: Zoom::Fit,
            zoom: 1.0,
            sx: 0.0,
            sy: 0.0,
        };
        l.zoom = l.fit_zoom();
        l
    }

    pub fn image_size(&self) -> (u32, u32) {
        (self.iw as u32, self.ih as u32)
    }

    pub fn mode(&self) -> Zoom {
        self.mode
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    pub fn view_size(&self) -> (f32, f32) {
        (self.vw, self.vh)
    }

    /// 이미지 1px의 DIP 크기
    fn k(&self) -> f32 {
        self.zoom * 96.0 / self.dpi
    }

    pub fn fit_zoom(&self) -> f32 {
        let aw = (self.vw - 2.0 * PAD).max(1.0);
        let ah = (self.vh - 2.0 * PAD).max(1.0);
        let px = 96.0 / self.dpi;
        (aw / (self.iw * px)).min(ah / (self.ih * px)).min(1.0)
    }

    fn clamp_zoom(&self, z: f32) -> f32 {
        z.clamp(self.fit_zoom().min(ZOOM_MIN), ZOOM_MAX)
    }

    /// 화면에 그릴 이미지 크기(DIP)
    pub fn image_dip(&self) -> (f32, f32) {
        (self.iw * self.k(), self.ih * self.k())
    }

    /// 화면 픽셀로 본 이미지 전체 크기 = 이 배율에서 디코딩할 크기
    pub fn needed_px(&self) -> (u32, u32) {
        (
            (self.iw * self.zoom).round().max(1.0) as u32,
            (self.ih * self.zoom).round().max(1.0) as u32,
        )
    }

    fn total(&self) -> (f32, f32) {
        let (w, h) = self.image_dip();
        ((w + 2.0 * PAD).max(self.vw), (h + 2.0 * PAD).max(self.vh))
    }

    /// 이미지 왼쪽 위의 화면 좌표(DIP). 창보다 작은 축은 가운데
    pub fn origin(&self) -> (f32, f32) {
        let (w, h) = self.image_dip();
        let ox = if w + 2.0 * PAD <= self.vw {
            ((self.vw - w) / 2.0).round()
        } else {
            PAD - self.sx
        };
        let oy = if h + 2.0 * PAD <= self.vh {
            ((self.vh - h) / 2.0).round()
        } else {
            PAD - self.sy
        };
        (ox, oy)
    }

    /// 화면에 보이는 부분을 `needed_px()` 크기 이미지의 픽셀 사각형 `(x, y, w, h)`로
    pub fn visible_px(&self) -> (u32, u32, u32, u32) {
        let (ox, oy) = self.origin();
        let s = self.dpi / 96.0;
        let (nw, nh) = self.needed_px();
        let x0 = ((-ox) * s).floor().clamp(0.0, nw as f32) as u32;
        let y0 = ((-oy) * s).floor().clamp(0.0, nh as f32) as u32;
        let x1 = ((self.vw - ox) * s).ceil().clamp(0.0, nw as f32) as u32;
        let y1 = ((self.vh - oy) * s).ceil().clamp(0.0, nh as f32) as u32;
        (x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
    }

    /// ((전체 폭, 창 폭, 가로 위치), (전체 높이, 창 높이, 세로 위치))
    pub fn scroll_metrics(&self) -> ((f32, f32, f32), (f32, f32, f32)) {
        let (tw, th) = self.total();
        ((tw, self.vw, self.sx), (th, self.vh, self.sy))
    }

    fn clamp_scroll(&mut self) {
        let (tw, th) = self.total();
        self.sx = self.sx.clamp(0.0, (tw - self.vw).max(0.0));
        self.sy = self.sy.clamp(0.0, (th - self.vh).max(0.0));
    }

    pub fn scroll_to(&mut self, x: Option<f32>, y: Option<f32>) {
        if let Some(x) = x {
            self.sx = x;
        }
        if let Some(y) = y {
            self.sy = y;
        }
        self.clamp_scroll();
    }

    pub fn scroll_by(&mut self, dx: f32, dy: f32) {
        self.scroll_to(Some(self.sx + dx), Some(self.sy + dy));
    }

    pub fn scroll_pos(&self) -> (f32, f32) {
        (self.sx, self.sy)
    }

    pub fn at_end(&self) -> bool {
        let (_, th) = self.total();
        self.sy >= th - self.vh - 0.5
    }

    /// 화면 점 아래의 이미지 좌표(px, 소수)
    fn image_at(&self, x: f32, y: f32) -> (f32, f32) {
        let (ox, oy) = self.origin();
        ((x - ox) / self.k(), (y - oy) / self.k())
    }

    /// 이미지 좌표 `(u, v)`가 화면 점 `(x, y)`에 오도록 스크롤한다
    fn put_image_at(&mut self, u: f32, v: f32, x: f32, y: f32) {
        let k = self.k();
        self.sx = PAD + u * k - x;
        self.sy = PAD + v * k - y;
        self.clamp_scroll();
    }

    /// 창 크기·DPI가 바뀌면: 맞춤이면 배율을 다시 구하고, 화면 가운데 내용을 유지한다
    pub fn set_view(&mut self, vw: f32, vh: f32, dpi: f32) {
        let (vw, vh, dpi) = (vw.max(1.0), vh.max(1.0), dpi.max(1.0));
        if (vw, vh, dpi) == (self.vw, self.vh, self.dpi) {
            return;
        }
        let (u, v) = self.image_at(self.vw / 2.0, self.vh / 2.0);
        self.vw = vw;
        self.vh = vh;
        self.dpi = dpi;
        self.zoom = match self.mode {
            Zoom::Fit => self.fit_zoom(),
            Zoom::Custom(z) => self.clamp_zoom(z),
        };
        self.put_image_at(u, v, vw / 2.0, vh / 2.0);
    }

    /// `at`: 고정할 화면 점(DIP). 없으면 창 가운데
    pub fn set_mode(&mut self, mode: Zoom, at: Option<(f32, f32)>) {
        let (x, y) = at.unwrap_or((self.vw / 2.0, self.vh / 2.0));
        let (u, v) = self.image_at(x, y);
        let (u, v) = (u.clamp(0.0, self.iw), v.clamp(0.0, self.ih));
        self.zoom = match mode {
            Zoom::Fit => self.fit_zoom(),
            Zoom::Custom(z) => self.clamp_zoom(z),
        };
        self.mode = match mode {
            Zoom::Fit => Zoom::Fit,
            Zoom::Custom(_) => Zoom::Custom(self.zoom),
        };
        self.put_image_at(u, v, x, y);
    }

    pub fn zoom_by(&mut self, factor: f32, at: Option<(f32, f32)>) {
        self.set_mode(Zoom::Custom(self.zoom * factor), at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn fit_shrinks_only() {
        let mut l = ImageLayout::new(4000, 2000);
        l.set_view(1000.0 + 2.0 * PAD, 800.0, 96.0);
        assert!(close(l.zoom(), 0.25));
        assert_eq!(l.scroll_metrics().0, (1024.0, 1024.0, 0.0));
        // 작은 이미지는 실제 크기로 가운데
        let mut s = ImageLayout::new(100, 50);
        s.set_view(500.0, 400.0, 96.0);
        assert_eq!(s.zoom(), 1.0);
        assert_eq!(s.origin(), (200.0, 175.0));
        // DPI 2배면 실제 픽셀 = 0.5 DIP
        s.set_view(500.0, 400.0, 192.0);
        assert_eq!(s.image_dip(), (50.0, 25.0));
        assert_eq!(s.needed_px(), (100, 50));
    }

    #[test]
    fn zoom_keeps_point_under_cursor() {
        let mut l = ImageLayout::new(1000, 1000);
        l.set_view(400.0, 300.0, 96.0);
        l.set_mode(Zoom::Custom(1.0), None);
        l.scroll_to(Some(200.0), Some(300.0));
        let before = l.image_at(100.0, 50.0);
        l.zoom_by(2.0, Some((100.0, 50.0)));
        assert!(close(l.zoom(), 2.0));
        let after = l.image_at(100.0, 50.0);
        assert!(close(before.0, after.0) && close(before.1, after.1));
        assert_eq!(l.mode(), Zoom::Custom(2.0));
    }

    #[test]
    fn scroll_clamped_and_visible_rect() {
        let mut l = ImageLayout::new(1000, 1000);
        l.set_view(400.0, 300.0, 96.0);
        l.set_mode(Zoom::Custom(1.0), Some((0.0, 0.0)));
        l.scroll_to(Some(-50.0), Some(1e9));
        let ((tw, vw, sx), (th, vh, sy)) = l.scroll_metrics();
        assert_eq!((tw, vw, sx), (1024.0, 400.0, 0.0));
        assert_eq!((th, vh, sy), (1024.0, 300.0, 724.0));
        assert!(l.at_end());
        // 보이는 부분: 가로 0..388 (여백 12), 세로 끝 300px 중 아래 여백 12 제외
        assert_eq!(l.visible_px(), (0, 712, 388, 288));
    }

    #[test]
    fn zoom_limits() {
        let mut l = ImageLayout::new(100_000, 100);
        l.set_view(500.0, 400.0, 96.0);
        let fit = l.fit_zoom();
        assert!(fit < ZOOM_MIN);
        l.zoom_by(0.001, None);
        assert!(close(l.zoom(), fit));
        l.zoom_by(1e6, None);
        assert_eq!(l.zoom(), ZOOM_MAX);
        l.set_mode(Zoom::Fit, None);
        assert_eq!(l.mode(), Zoom::Fit);
        assert!(close(l.zoom(), fit));
    }
}

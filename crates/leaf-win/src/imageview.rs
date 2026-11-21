//! 이미지 보기 (M5 확장성 검증 — `CustomFormat`으로만 붙는다).
//!
//! - 디코딩은 워커에서 WIC로, **화면에 필요한 크기로만** 한다(원본 해상도로 보관하지 않음, G1).
//! - 비트맵 두 장: 문서 전체를 덮는 **기본**(열 때의 맞춤 크기) + 확대했을 때의 **세부**.
//!   세부는 그 배율의 이미지 전체가 예산 안이면 전체, 넘으면 화면 근처 영역만 디코딩한다.
//!   예산 = 설정 `[pdf] bitmap_cache_mb`(기본 64MB). 원본보다 크게는 디코딩하지 않는다(그 이상은 D2D가 확대).
//! - EXIF 방향(사진 회전)을 적용한다. GIF는 첫 프레임만.

use std::any::Any;
use std::path::{Path, PathBuf};
use std::time::Instant;

use leaf_core::session::FilePos;
use leaf_doc::format::FormatInfo;
use leaf_fmt_image::{ImageLayout, Orientation, Zoom};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
use windows::Win32::System::Variant::VT_UI2;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::core::w;

use crate::custom::{CustomCmd, CustomFormat, CustomView, OpenCtx};
use crate::mdload::{self, ComInit};
use crate::pdfview::CursorKind;
use crate::render::{self, Target};

pub struct ImageFormat;
pub static IMAGE: ImageFormat = ImageFormat;

/// 기본 비트맵은 예산의 이 비율까지만 (나머지는 세부용)
const BASE_SHARE: usize = 4;
const LINE: f32 = 40.0;

// ───── 디코딩 (워커) ─────

/// 디코딩 결과. 좌표는 모두 방향을 적용한 뒤 기준
struct Decoded {
    /// 원본 크기(px)
    iw: u32,
    ih: u32,
    /// 이 디코딩의 이미지 전체 크기(px). `rect`는 그 안의 영역
    dw: u32,
    rect: (u32, u32, u32, u32),
    /// 미리 곱한 알파 BGRA, stride = rect.w × 4
    bgra: Vec<u8>,
}

struct Loaded {
    first: Decoded,
    bytes: u64,
    frames: u32,
    load_ms: f64,
}

/// 워커 결과
enum Res {
    Base(Decoded),
    Detail(Decoded),
    Failed(String),
}

fn wic_err(e: windows::core::Error) -> String {
    if e.message().is_empty() {
        format!("이미지를 읽을 수 없습니다 ({:?})", e.code())
    } else {
        e.message()
    }
}

/// EXIF 방향. JPEG는 `/app1/ifd`, TIFF는 `/ifd` 아래에 있다. 없으면 그대로
fn orientation(frame: &IWICBitmapFrameDecode) -> Orientation {
    unsafe {
        let Ok(q) = frame.GetMetadataQueryReader() else {
            return Orientation::NORMAL;
        };
        for name in [w!("/app1/ifd/{ushort=274}"), w!("/ifd/{ushort=274}")] {
            let mut pv = PROPVARIANT::default();
            if q.GetMetadataByName(name, &mut pv).is_ok() {
                let inner = &pv.Anonymous.Anonymous;
                let v = (inner.vt == VT_UI2).then(|| inner.Anonymous.uiVal);
                let _ = PropVariantClear(&mut pv);
                if let Some(v) = v {
                    return Orientation::from_exif(v);
                }
            }
        }
    }
    Orientation::NORMAL
}

/// `target_w(원본 폭, 원본 높이)`: 방향을 적용한 이미지 전체를 이 폭으로(원본보다 크게는 안 함).
/// `region`: 그 크기 기준의 영역. 없으면 전체. 두 번째 값은 프레임 수
fn decode(
    path: &Path,
    target_w: impl FnOnce(u32, u32) -> u32,
    region: Option<(u32, u32, u32, u32)>,
) -> Result<(Decoded, u32), String> {
    let _com = ComInit::new();
    unsafe {
        let f = mdload::factory().map_err(wic_err)?;
        let wp = crate::util::wide(path);
        let stream = windows::Win32::UI::Shell::SHCreateStreamOnFileEx(
            windows::core::PCWSTR(wp.as_ptr()),
            (windows::Win32::System::Com::STGM_READ
                | windows::Win32::System::Com::STGM_SHARE_DENY_NONE)
                .0,
            0,
            false,
            None,
        )
        .map_err(wic_err)?;
        let dec = f
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .map_err(wic_err)?;
        let frames = dec.GetFrameCount().unwrap_or(1);
        let frame = dec.GetFrame(0).map_err(wic_err)?;
        let (mut sw, mut sh) = (0u32, 0u32);
        frame.GetSize(&mut sw, &mut sh).map_err(wic_err)?;
        if sw == 0 || sh == 0 {
            return Err("크기가 0인 이미지입니다".into());
        }
        let o = orientation(&frame);
        let (iw, ih) = o.display_size(sw, sh);
        let dw = target_w(iw, ih).clamp(1, iw);
        let dh = ((ih as u64 * dw as u64) / iw as u64).max(1) as u32;
        // 회전 전 기준의 디코딩 크기
        let (ssw, ssh) = if o.swaps_axes() { (dh, dw) } else { (dw, dh) };
        let source: IWICBitmapSource = if ssw < sw {
            let s = f.CreateBitmapScaler().map_err(wic_err)?;
            s.Initialize(&frame, ssw, ssh, WICBitmapInterpolationModeHighQualityCubic)
                .map_err(wic_err)?;
            s.into()
        } else {
            frame.into()
        };
        let conv = f.CreateFormatConverter().map_err(wic_err)?;
        conv.Initialize(
            &source,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeMedianCut,
        )
        .map_err(wic_err)?;
        let rect = match region {
            Some((x, y, w, h)) => {
                let x = x.min(dw - 1);
                let y = y.min(dh - 1);
                (x, y, w.clamp(1, dw - x), h.clamp(1, dh - y))
            }
            None => (0, 0, dw, dh),
        };
        let (u, v, w, h) = o.src_rect(rect, dw, dh);
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        let wr = WICRect {
            X: u as i32,
            Y: v as i32,
            Width: w as i32,
            Height: h as i32,
        };
        conv.CopyPixels(&wr, w * 4, &mut buf).map_err(wic_err)?;
        let bgra = if o.is_identity() {
            buf
        } else {
            o.apply(&buf, w, h)
        };
        Ok((
            Decoded {
                iw,
                ih,
                dw,
                rect,
                bgra,
            },
            frames,
        ))
    }
}

/// 기본 비트맵 폭(px) = 창에 맞춘 크기. 예산 몫(1/4)을 넘지 않게 줄인다
fn base_width(iw: u32, ih: u32, view: (f32, f32), dpi: f32, budget: usize) -> u32 {
    let mut l = ImageLayout::new(iw, ih);
    l.set_view(view.0, view.1, dpi);
    let (w, h) = l.needed_px();
    let cap = budget / BASE_SHARE / 4;
    let k = ((cap as f64) / (w as f64 * h as f64)).sqrt().min(1.0);
    ((w as f64 * k) as u32).max(1)
}

impl CustomFormat for ImageFormat {
    fn info(&self) -> &'static FormatInfo {
        &leaf_fmt_image::FORMAT
    }

    fn load(&self, path: &Path, ctx: &OpenCtx) -> Result<Box<dyn Any + Send>, String> {
        let t = Instant::now();
        let bytes = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
        let (view, dpi, budget) = (ctx.view, ctx.dpi, ctx.bitmap_budget);
        let (first, frames) = decode(path, |iw, ih| base_width(iw, ih, view, dpi, budget), None)?;
        Ok(Box::new(Loaded {
            first,
            bytes,
            frames,
            load_ms: t.elapsed().as_secs_f64() * 1000.0,
        }))
    }

    fn create(
        &self,
        generation: u64,
        path: PathBuf,
        loaded: Box<dyn Any + Send>,
        ctx: &OpenCtx,
    ) -> Box<dyn CustomView> {
        let l = *loaded.downcast::<Loaded>().expect("이미지 로드 결과");
        let mut layout = ImageLayout::new(l.first.iw, l.first.ih);
        layout.set_view(ctx.view.0, ctx.view.1, ctx.dpi);
        Box::new(ImageView {
            generation,
            path,
            layout,
            bytes: l.bytes,
            frames: l.frames,
            load_ms: l.load_ms,
            budget: ctx.bitmap_budget,
            base: None,
            detail: None,
            incoming_base: Some(l.first),
            pending: None,
            epoch: u64::MAX,
            dpi: ctx.dpi,
            error: None,
            drag: None,
            first_paint: false,
        })
    }
}

// ───── 보기 (UI 스레드) ─────

struct Bmp {
    bmp: ID2D1Bitmap,
    dw: u32,
    rect: (u32, u32, u32, u32),
}

impl Bmp {
    fn bytes(&self) -> usize {
        self.rect.2 as usize * self.rect.3 as usize * 4
    }
}

/// 픽셀 사각형 (x, y, w, h)
type PxRect = (u32, u32, u32, u32);
/// 진행 중인 디코딩: (기본인지, 전체 폭, 영역)
type Req = (bool, u32, Option<PxRect>);

pub struct ImageView {
    generation: u64,
    path: PathBuf,
    layout: ImageLayout,
    bytes: u64,
    frames: u32,
    load_ms: f64,
    budget: usize,
    base: Option<Bmp>,
    detail: Option<Bmp>,
    /// 아직 비트맵으로 만들지 않은 기본 디코딩 결과 (열 때·다시 디코딩 때)
    incoming_base: Option<Decoded>,
    pending: Option<Req>,
    epoch: u64,
    dpi: f32,
    error: Option<String>,
    /// 끌어서 이동: 누른 점
    drag: Option<(f32, f32)>,
    first_paint: bool,
}

/// `inner`가 `outer` 안에 있는지 (x, y, w, h)
fn contains(outer: (u32, u32, u32, u32), inner: (u32, u32, u32, u32)) -> bool {
    inner.0 >= outer.0
        && inner.1 >= outer.1
        && inner.0 + inner.2 <= outer.0 + outer.2
        && inner.1 + inner.3 <= outer.1 + outer.3
}

impl ImageView {
    /// 지금 배율에서 원하는 디코딩 폭 (원본보다 크게는 안 함)
    fn want_w(&self) -> u32 {
        let (iw, _) = self.layout.image_size();
        self.layout.needed_px().0.min(iw)
    }

    /// 세부 비트맵이 필요하면 (전체 폭, 영역, 보이는 영역)
    fn want_detail(&self) -> Option<(u32, Option<PxRect>, PxRect)> {
        let dw = self.want_w();
        if self
            .base
            .as_ref()
            .is_some_and(|b| b.dw as f32 >= dw as f32 * 0.98)
        {
            return None;
        }
        let (iw, ih) = self.layout.image_size();
        let dh = ((ih as u64 * dw as u64) / iw as u64).max(1) as u32;
        // 보이는 영역(배율 크기 기준) → 디코딩 크기 기준
        let (nw, _) = self.layout.needed_px();
        let s = dw as f64 / nw as f64;
        let (x, y, w, h) = self.layout.visible_px();
        let vis = (
            (x as f64 * s).floor() as u32,
            (y as f64 * s).floor() as u32,
            ((w as f64 * s).ceil() as u32).max(1),
            ((h as f64 * s).ceil() as u32).max(1),
        );
        let vis = (
            vis.0.min(dw - 1),
            vis.1.min(dh - 1),
            vis.2.min(dw - vis.0.min(dw - 1)),
            vis.3.min(dh - vis.1.min(dh - 1)),
        );
        let cap = self.budget - self.budget / BASE_SHARE;
        if dw as usize * dh as usize * 4 <= cap {
            return Some((dw, None, vis));
        }
        // 화면 둘레로 반 화면씩 넓혀 조금 스크롤해도 다시 디코딩하지 않게
        let (mx, my) = (vis.2 / 2, vis.3 / 2);
        let x0 = vis.0.saturating_sub(mx);
        let y0 = vis.1.saturating_sub(my);
        let x1 = (vis.0 + vis.2 + mx).min(dw);
        let y1 = (vis.1 + vis.3 + my).min(dh);
        Some((dw, Some((x0, y0, x1 - x0, y1 - y0)), vis))
    }

    fn spawn(&mut self, req: Req) -> CustomCmd {
        self.pending = Some(req);
        let path = self.path.clone();
        CustomCmd::Spawn(Box::new(move || {
            let r = match decode(&path, |_, _| req.1, req.2) {
                Ok((d, _)) if req.0 => Res::Base(d),
                Ok((d, _)) => Res::Detail(d),
                Err(e) => Res::Failed(e),
            };
            Box::new(r) as Box<dyn Any + Send>
        }))
    }

    fn make_bmp(t: &Target, d: &Decoded) -> Option<Bmp> {
        render::create_bitmap_premul(t, d.rect.2, d.rect.3, &d.bgra)
            .ok()
            .map(|bmp| Bmp {
                bmp,
                dw: d.dw,
                rect: d.rect,
            })
    }

    fn draw(&self, t: &Target, b: &Bmp) {
        let (ox, oy) = self.layout.origin();
        let (w, _) = self.layout.image_dip();
        let k = w / b.dw as f32; // DIP per 디코딩 px
        let r = render::rect(
            ox + b.rect.0 as f32 * k,
            oy + b.rect.1 as f32 * k,
            ox + (b.rect.0 + b.rect.2) as f32 * k,
            oy + (b.rect.1 + b.rect.3) as f32 * k,
        );
        // 디코딩 1px이 화면 2px 이상이면 픽셀을 그대로 (도트 그림·확대 확인)
        let device_k = self.layout.needed_px().0 as f32 / b.dw as f32;
        let interp = if device_k >= 2.0 {
            D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR
        } else {
            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR
        };
        unsafe { t.rt.DrawBitmap(&b.bmp, Some(&r), 1.0, interp, None) };
    }
}

impl CustomView for ImageView {
    fn path(&self) -> &Path {
        &self.path
    }

    fn generation(&self) -> u64 {
        self.generation
    }

    fn view_key(&self) -> &'static str {
        "image"
    }

    fn info(&self) -> String {
        let (iw, ih) = self.layout.image_size();
        let ext = self
            .path
            .extension()
            .map(|e| e.to_string_lossy().to_uppercase())
            .unwrap_or_else(|| "이미지".into());
        let mut s = format!(
            "{} · {ext} · {iw}×{ih} · {}",
            self.path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default(),
            crate::util::human_size(self.bytes)
        );
        if self.frames > 1 {
            s.push_str(&format!(" · 프레임 {}개 중 첫 프레임", self.frames));
        }
        s
    }

    fn status(&self) -> String {
        if let Some(e) = &self.error {
            return e.clone();
        }
        let pct = (self.layout.zoom() * 100.0).round();
        let mode = match self.layout.mode() {
            Zoom::Fit if pct >= 100.0 => "실제 크기".to_string(),
            Zoom::Fit => format!("맞춤 {pct}%"),
            Zoom::Custom(_) => format!("{pct}%"),
        };
        format!(
            "{mode} · 열기 {:.0}ms · Ctrl+0 맞춤 · Ctrl+1 실제 크기",
            self.load_ms
        )
    }

    fn set_view(&mut self, vw: f32, vh: f32, dpi: f32) {
        self.dpi = dpi;
        self.layout.set_view(vw, vh, dpi);
    }

    fn paint(&mut self, t: &Target) -> Vec<CustomCmd> {
        let mut cmds = Vec::new();
        if self.epoch != t.epoch {
            // 타깃이 바뀌었다(디바이스 손실·최소화 뒤 복원·테마) → 비트맵은 다시
            self.epoch = t.epoch;
            self.base = None;
            self.detail = None;
        }
        if let Some(d) = self.incoming_base.take() {
            self.base = Self::make_bmp(t, &d);
        }
        // 디코딩이 실패했으면 다시 요청하지 않는다 (실패 → 다시 그리기 → 재요청 반복 방지).
        // 파일이 바뀌면 파일 감시가 문서를 새로 연다
        if self.error.is_none() {
            if self.base.is_none() && self.pending.is_none() {
                let (iw, ih) = self.layout.image_size();
                let w = base_width(iw, ih, self.layout.view_size(), self.dpi, self.budget);
                cmds.push(self.spawn((true, w, None)));
            }
            match self.want_detail() {
                None => self.detail = None,
                Some((dw, region, vis)) => {
                    let ok = self
                        .detail
                        .as_ref()
                        .is_some_and(|d| d.dw == dw && contains(d.rect, vis));
                    if !ok && self.pending.is_none() {
                        cmds.push(self.spawn((false, dw, region)));
                    }
                }
            }
        }

        let (ox, oy) = self.layout.origin();
        let (w, h) = self.layout.image_dip();
        match &self.base {
            Some(b) => self.draw(t, b),
            None => render::fill(
                t,
                leaf_doc::RectF {
                    l: ox,
                    t: oy,
                    r: ox + w,
                    b: oy + h,
                },
                &t.placeholder,
            ),
        }
        if let Some(d) = &self.detail
            && self.base.as_ref().is_none_or(|b| d.dw > b.dw)
        {
            self.draw(t, d);
        }
        self.first_paint |= self.base.is_some();
        cmds
    }

    fn on_result(&mut self, t: &Target, result: Box<dyn Any + Send>) {
        self.pending = None;
        let r = match result.downcast::<Res>() {
            Ok(r) => r,
            // 작업이 패닉했다 (앱이 문구로 바꿔 넘긴다)
            Err(other) => Box::new(Res::Failed(
                other
                    .downcast::<String>()
                    .map(|s| *s)
                    .unwrap_or_else(|_| "내부 오류".into()),
            )),
        };
        match *r {
            Res::Base(d) => self.base = Self::make_bmp(t, &d),
            Res::Detail(d) => {
                // 그 사이 배율이 바뀌었으면 버린다 (다음 그리기가 새로 요청)
                if d.dw == self.want_w() {
                    self.detail = Self::make_bmp(t, &d);
                }
            }
            Res::Failed(e) => self.error = Some(e),
        }
    }

    fn first_paint_done(&self) -> bool {
        self.first_paint
    }

    fn scroll_metrics(&self) -> ((f32, f32, f32), (f32, f32, f32)) {
        self.layout.scroll_metrics()
    }

    fn scroll_to(&mut self, x: Option<f32>, y: Option<f32>) {
        self.layout.scroll_to(x, y);
    }

    fn scroll_by(&mut self, dx: f32, dy: f32) {
        self.layout.scroll_by(dx, dy);
    }

    fn at_end(&self) -> bool {
        self.layout.at_end()
    }

    fn zoom_by(&mut self, factor: f32, at: Option<(f32, f32)>) {
        self.layout.zoom_by(factor, at);
    }

    fn on_key(&mut self, vk: u16, ctrl: bool, _shift: bool) -> bool {
        let (_, vh) = self.layout.view_size();
        let vk = VIRTUAL_KEY(vk);
        match vk {
            VK_OEM_PLUS | VK_ADD if ctrl => self.layout.zoom_by(1.25, None),
            VK_OEM_MINUS | VK_SUBTRACT if ctrl => self.layout.zoom_by(0.8, None),
            _ if ctrl && (vk.0 == b'0' as u16 || vk == VK_NUMPAD0) => {
                self.layout.set_mode(Zoom::Fit, None)
            }
            _ if ctrl && (vk.0 == b'1' as u16 || vk == VK_NUMPAD1) => {
                self.layout.set_mode(Zoom::Custom(1.0), None)
            }
            VK_UP => self.layout.scroll_by(0.0, -LINE),
            VK_DOWN => self.layout.scroll_by(0.0, LINE),
            VK_LEFT => self.layout.scroll_by(-LINE, 0.0),
            VK_RIGHT => self.layout.scroll_by(LINE, 0.0),
            VK_PRIOR => self.layout.scroll_by(0.0, -vh * 0.9),
            VK_NEXT | VK_SPACE => self.layout.scroll_by(0.0, vh * 0.9),
            VK_HOME => self.layout.scroll_to(Some(0.0), Some(0.0)),
            VK_END => self.layout.scroll_to(None, Some(f32::MAX)),
            _ => return false,
        }
        true
    }

    fn on_mouse_down(&mut self, x: f32, y: f32) {
        self.drag = Some((x, y));
    }

    fn on_mouse_move(&mut self, x: f32, y: f32, dragging: bool) -> CursorKind {
        let ((tw, vw, _), (th, vh, _)) = self.layout.scroll_metrics();
        let pannable = tw > vw || th > vh;
        if dragging && let Some((px, py)) = self.drag {
            self.layout.scroll_by(px - x, py - y);
            self.drag = Some((x, y));
        }
        if pannable {
            CursorKind::Hand
        } else {
            CursorKind::Arrow
        }
    }

    fn on_mouse_up(&mut self) {
        self.drag = None;
    }

    fn position(&self) -> FilePos {
        let (sx, sy) = self.layout.scroll_pos();
        FilePos {
            path: self.path.clone(),
            page: 0,
            y_pt: sy,
            zoom: match self.layout.mode() {
                Zoom::Fit => "fit".into(),
                Zoom::Custom(z) => format!("{z:.4}"),
            },
            // 가로 스크롤 위치(DIP)
            offset: sx.max(0.0) as u64,
            view: "image".into(),
            wrap: None,
        }
    }

    fn restore(&mut self, pos: &FilePos) {
        if pos.view != "image" {
            return;
        }
        let mode = pos.zoom.parse().map(Zoom::Custom).unwrap_or(Zoom::Fit);
        self.layout.set_mode(mode, None);
        self.layout
            .scroll_to(Some(pos.offset as f32), Some(pos.y_pt));
    }

    fn release(&mut self) {
        self.base = None;
        self.detail = None;
        self.incoming_base = None;
    }

    fn bitmap_stats(&self) -> (u32, usize) {
        let b = [&self.base, &self.detail];
        (
            b.iter().filter(|x| x.is_some()).count() as u32,
            b.iter().flat_map(|x| x.as_ref()).map(|x| x.bytes()).sum(),
        )
    }
}

#[cfg(test)]
mod tests {
    //! WIC 디코딩을 실제 파일로 확인한다. `python testdata/gen_img.py`로 만든 파일이 없으면 건너뛴다.
    use super::*;

    fn sample(name: &str) -> Option<PathBuf> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/out/img")
            .join(name);
        if p.exists() {
            Some(p)
        } else {
            eprintln!("건너뜀: {} 없음 (python testdata/gen_img.py)", p.display());
            None
        }
    }

    /// (B, G, R, A)
    fn px(d: &Decoded, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * d.rect.2 + x) * 4) as usize;
        d.bgra[i..i + 4].try_into().unwrap()
    }

    fn is_green(p: [u8; 4]) -> bool {
        p[1] > 100 && p[0] < 140 && p[2] < 100
    }

    fn is_white(p: [u8; 4]) -> bool {
        p.iter().all(|&c| c > 220)
    }

    #[test]
    fn exif_rotation_applied() {
        let Some(p) = sample("rot6.jpg") else { return };
        let (d, frames) = decode(&p, |w, _| w, None).unwrap();
        assert_eq!(frames, 1);
        // 저장은 300×200, 방향 6 → 보이는 크기 200×300
        assert_eq!((d.iw, d.ih, d.dw), (200, 300, 200));
        // 화살표 끝이 위쪽 가운데, 아래 모서리는 흰색
        assert!(is_green(px(&d, 100, 40)), "{:?}", px(&d, 100, 40));
        assert!(is_white(px(&d, 5, 295)));
        assert!(is_white(px(&d, 195, 295)));
        // 부분 디코딩도 같은 픽셀
        let (r, _) = decode(&p, |w, _| w, Some((90, 30, 20, 20))).unwrap();
        assert_eq!(r.rect, (90, 30, 20, 20));
        assert_eq!(px(&r, 10, 10), px(&d, 100, 40));
    }

    #[test]
    fn region_matches_full_resolution() {
        let Some(p) = sample("big.png") else { return };
        // 원본 크기 그대로 한 영역만 (96MB 전체를 디코딩하지 않는다)
        let (r, _) = decode(&p, |w, _| w, Some((1210, 760, 40, 30))).unwrap();
        assert_eq!((r.iw, r.ih, r.dw), (6000, 4000, 6000));
        assert_eq!(r.bgra.len(), 40 * 30 * 4);
        // gen_img.py의 격자 색: 칸 (bx, by) = (x/50, y/50)
        let (x, y) = (1210 + 5, 760 + 5);
        let (bx, by) = (x / 50, y / 50);
        let want = [
            (((bx + by) * 29) % 200 + 40) as u8,
            ((by * 53) % 200 + 40) as u8,
            ((bx * 37) % 200 + 40) as u8,
            255,
        ];
        assert_eq!(px(&r, 5, 5), want);
        // 줄여서 전체: 폭 600이면 높이 400
        let (s, _) = decode(&p, |_, _| 600, None).unwrap();
        assert_eq!((s.dw, s.rect), (600, (0, 0, 600, 400)));
    }

    #[test]
    fn errors_and_first_frame() {
        // 잘린 PNG: WIC는 읽은 데까지만 채우고 성공하기도 한다 — 패닉 없이 크기만 맞으면 된다
        if let Some(p) = sample("broken.png")
            && let Ok((d, _)) = decode(&p, |w, _| w, None)
        {
            assert_eq!(d.bgra.len(), (d.rect.2 * d.rect.3 * 4) as usize);
        }
        // 이미지가 아닌 파일은 오류 문구
        let junk = std::env::temp_dir().join(format!("leaf-img-junk-{}.png", std::process::id()));
        std::fs::write(&junk, b"this is not an image").unwrap();
        let e = decode(&junk, |w, _| w, None).err();
        std::fs::remove_file(&junk).unwrap();
        assert!(e.is_some_and(|m| !m.is_empty()));
        if let Some(p) = sample("anim.gif") {
            let (d, frames) = decode(&p, |w, _| w, None).unwrap();
            assert_eq!((frames, d.iw, d.ih), (2, 32, 32));
        }
    }
}

//! Direct2D 1.1 + DirectWrite 렌더러 (SPEC §3.1).
//! 디바이스 리소스(렌더 타깃·브러시·비트맵)는 지연 생성하고 D2DERR_RECREATE_TARGET 시 버린다.

use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::core::{Result, w};
use windows_numerics::Vector2;

use crate::theme::{self, Palette};

pub fn rgb(hex: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a: 1.0,
    }
}

pub fn rgba(hex: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { a, ..rgb(hex) }
}

pub struct Target {
    pub rt: ID2D1HwndRenderTarget,
    /// 이 타깃을 만들 때의 테마 색 (테마가 바뀌면 타깃을 다시 만든다)
    pub pal: Palette,
    pub text: ID2D1SolidColorBrush,
    pub dim: ID2D1SolidColorBrush,
    pub placeholder: ID2D1SolidColorBrush,
    pub page_border: ID2D1SolidColorBrush,
    /// 찾기 일치 항목 / 현재 항목 / 선택 영역 (반투명)
    pub hit: ID2D1SolidColorBrush,
    pub hit_current: ID2D1SolidColorBrush,
    pub selection: ID2D1SolidColorBrush,
    /// MD: 링크 글자, 코드 블록·표 머리글 배경, 구분선·테두리, 인라인 코드 배경
    pub link: ID2D1SolidColorBrush,
    pub code_bg: ID2D1SolidColorBrush,
    pub border: ID2D1SolidColorBrush,
    pub inline_code: ID2D1SolidColorBrush,
    /// 타깃이 다시 만들어질 때마다 증가 → 이 타깃에서 만든 비트맵이 아직 유효한지 판별
    pub epoch: u64,
}

pub struct Renderer {
    d2d: ID2D1Factory1,
    pub dwrite: IDWriteFactory,
    pub body: IDWriteTextFormat,
    pub title: IDWriteTextFormat,
    target: Option<Target>,
    epoch: u64,
    dpi: f32,
    /// SPEC §12 Q8: 기본은 소프트웨어 렌더 타깃. 설정 `[general] gpu = true`면 하드웨어
    gpu: bool,
}

fn text_format(
    dw: &IDWriteFactory,
    family: windows::core::PCWSTR,
    size_pt: f32,
    weight: DWRITE_FONT_WEIGHT,
) -> Result<IDWriteTextFormat> {
    unsafe {
        dw.CreateTextFormat(
            family,
            None,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size_pt * 96.0 / 72.0, // DIP
            w!("ko-KR"),           // 한글 대체 폰트 선택(맑은 고딕)에 영향
        )
    }
}

impl Renderer {
    pub fn new(dpi: u32, gpu: bool) -> Result<Self> {
        unsafe {
            let d2d: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let body = text_format(&dwrite, w!("Segoe UI"), 11.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            let title = text_format(&dwrite, w!("Segoe UI"), 20.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            Ok(Self {
                d2d,
                dwrite,
                body,
                title,
                target: None,
                epoch: 0,
                dpi: dpi as f32,
                gpu: gpu || std::env::var_os("LEAF_D2D_GPU").is_some_and(|v| v == "1"),
            })
        }
    }

    pub fn dpi(&self) -> f32 {
        self.dpi
    }

    pub fn set_dpi(&mut self, dpi: u32) {
        self.dpi = dpi as f32;
        if let Some(t) = &self.target {
            unsafe { t.rt.SetDpi(self.dpi, self.dpi) };
        }
    }

    pub fn resize(&mut self, width_px: u32, height_px: u32) {
        if let Some(t) = &self.target {
            let size = D2D_SIZE_U {
                width: width_px,
                height: height_px,
            };
            if unsafe { t.rt.Resize(&size) }.is_err() {
                self.target = None;
            }
        }
    }

    pub fn set_gpu(&mut self, gpu: bool) {
        if gpu != self.gpu {
            self.gpu = gpu;
            self.target = None;
        }
    }

    /// 창 최소화·테마 변경 등에서 디바이스 리소스를 비운다 (SPEC §4.5).
    pub fn discard_target(&mut self) {
        self.target = None;
    }

    pub fn target(&mut self, hwnd: HWND) -> Result<&Target> {
        if self.target.is_none() {
            let mut rc = RECT::default();
            unsafe { GetClientRect(hwnd, &mut rc)? };
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: self.dpi,
                dpiY: self.dpi,
                // SPEC §12 Q8: 소프트웨어가 기본 (GPU 드라이버 DLL 로딩 비용 회피, G1).
                // GPU는 설정 `[general] gpu = true`(또는 환경 변수 LEAF_D2D_GPU=1)로 켠다.
                r#type: if self.gpu {
                    D2D1_RENDER_TARGET_TYPE_DEFAULT
                } else {
                    D2D1_RENDER_TARGET_TYPE_SOFTWARE
                },
                ..Default::default()
            };
            let hprops = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U {
                    width: (rc.right - rc.left).max(1) as u32,
                    height: (rc.bottom - rc.top).max(1) as u32,
                },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            unsafe {
                let rt = self.d2d.CreateHwndRenderTarget(&props, &hprops)?;
                let p = theme::current();
                let solid = |c: u32| rt.CreateSolidColorBrush(&rgb(c), None);
                let alpha = |(c, a): (u32, f32)| rt.CreateSolidColorBrush(&rgba(c, a), None);
                let text = solid(p.text)?;
                let dim = solid(p.dim)?;
                let placeholder = solid(p.placeholder)?;
                let page_border = solid(p.page_border)?;
                let hit = alpha(p.hit)?;
                let hit_current = alpha(p.hit_current)?;
                let selection = alpha(p.selection)?;
                let link = solid(p.link)?;
                let code_bg = solid(p.code_bg)?;
                let border = solid(p.border)?;
                let inline_code = alpha(p.inline_code)?;
                self.epoch += 1;
                self.target = Some(Target {
                    rt,
                    pal: p,
                    text,
                    dim,
                    placeholder,
                    page_border,
                    hit,
                    hit_current,
                    selection,
                    link,
                    code_bg,
                    border,
                    inline_code,
                    epoch: self.epoch,
                });
            }
        }
        Ok(self.target.as_ref().unwrap())
    }

    /// EndDraw 결과 처리: 디바이스 손실이면 타깃을 버리고 다음 그리기에서 다시 만든다.
    pub fn end_draw(&mut self) -> Result<()> {
        let Some(t) = &self.target else { return Ok(()) };
        match unsafe { t.rt.EndDraw(None, None) } {
            Err(e) if e.code() == D2DERR_RECREATE_TARGET => {
                self.target = None;
                Ok(())
            }
            r => r,
        }
    }

    pub fn layout(
        &self,
        text: &[u16],
        format: &IDWriteTextFormat,
        max_w: f32,
    ) -> Result<(IDWriteTextLayout, f32)> {
        unsafe {
            let layout = self
                .dwrite
                .CreateTextLayout(text, format, max_w.max(1.0), f32::MAX)?;
            let mut m = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut m)?;
            Ok((layout, m.height))
        }
    }
}

pub fn draw_layout(
    t: &Target,
    layout: &IDWriteTextLayout,
    x: f32,
    y: f32,
    brush: &ID2D1SolidColorBrush,
) {
    unsafe {
        t.rt.DrawTextLayout(
            Vector2 { X: x, Y: y },
            layout,
            brush,
            D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
        )
    };
}

pub fn fill(t: &Target, r: leaf_doc::RectF, brush: &ID2D1SolidColorBrush) {
    unsafe { t.rt.FillRectangle(&rect(r.l, r.t, r.r, r.b), brush) };
}

pub fn rect(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}

/// PDFium BGRA(불투명) 버퍼 → D2D 비트맵. 비트맵 DPI를 96으로 두면 1px = 1DIP 이므로
/// 그릴 때는 목적지 사각형(DIP)을 직접 지정한다.
pub fn create_bitmap(t: &Target, w: u32, h: u32, stride: u32, bgra: &[u8]) -> Result<ID2D1Bitmap> {
    create_bitmap_with(t, w, h, stride, bgra, D2D1_ALPHA_MODE_IGNORE)
}

/// 투명도가 있는 이미지(WIC 32bppPBGRA, 미리 곱한 알파) → D2D 비트맵 (MD 이미지)
pub fn create_bitmap_premul(t: &Target, w: u32, h: u32, bgra: &[u8]) -> Result<ID2D1Bitmap> {
    create_bitmap_with(t, w, h, w * 4, bgra, D2D1_ALPHA_MODE_PREMULTIPLIED)
}

fn create_bitmap_with(
    t: &Target,
    w: u32,
    h: u32,
    stride: u32,
    bgra: &[u8],
    alpha: D2D1_ALPHA_MODE,
) -> Result<ID2D1Bitmap> {
    let props = D2D1_BITMAP_PROPERTIES {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: alpha,
        },
        dpiX: 96.0,
        dpiY: 96.0,
    };
    unsafe {
        t.rt.CreateBitmap(
            D2D_SIZE_U {
                width: w,
                height: h,
            },
            Some(bgra.as_ptr().cast()),
            stride,
            &props,
        )
    }
}

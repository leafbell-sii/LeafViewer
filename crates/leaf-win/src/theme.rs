//! 라이트/다크 테마 (FR-A03). 색은 `Palette` 한 곳에 모으고, 문서 영역(D2D)과 창 장식(GDI)이 같이 쓴다.
//! UI 스레드 전용(thread_local).

use std::cell::{Cell, RefCell};
use std::ffi::c_void;

use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, HBRUSH};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Controls::SetWindowTheme;
use windows::core::{BOOL, PCSTR, w};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    // ── 문서 영역 (D2D) ──
    pub bg: u32,
    pub text: u32,
    pub dim: u32,
    pub pdf_bg: u32,
    pub placeholder: u32,
    pub page_border: u32,
    pub link: u32,
    pub code_bg: u32,
    pub border: u32,
    /// (색, 불투명도)
    pub inline_code: (u32, f32),
    pub selection: (u32, f32),
    pub hit: (u32, f32),
    pub hit_current: (u32, f32),
    // ── 창 장식 (GDI) ──
    pub side_bg: u32,
    pub side_text: u32,
    pub side_dim: u32,
    pub sel_bg: u32,
    pub sel_bg_inactive: u32,
    pub hover_bg: u32,
    pub accent: u32,
    pub status_bg: u32,
    pub status_text: u32,
    pub splitter: u32,
    pub bar_bg: u32,
    pub edit_bg: u32,
    /// 편집 중 문서 배경 (보기와 구분되는 엷은 노랑, FR-E05)
    pub editing_bg: u32,
}

pub const LIGHT: Palette = Palette {
    dark: false,
    bg: 0xFFFFFF,
    text: 0x1F1F1F,
    dim: 0x6B6B6B,
    pdf_bg: 0xE6E6E6,
    placeholder: 0xF7F7F7,
    page_border: 0xC8C8C8,
    link: 0x0969DA,
    code_bg: 0xF6F8FA,
    border: 0xD0D7DE,
    inline_code: (0x818B98, 0.18),
    selection: (0x2F7BFF, 0.30),
    hit: (0xFFD400, 0.40),
    hit_current: (0xFF7A00, 0.55),
    side_bg: 0xF7F7F7,
    side_text: 0x1F1F1F,
    side_dim: 0x707070,
    sel_bg: 0xCCE4F7,
    sel_bg_inactive: 0xE3E3E3,
    hover_bg: 0xEAEAEA,
    accent: 0x0067C0,
    status_bg: 0xF0F0F0,
    status_text: 0x3B3B3B,
    splitter: 0xE0E0E0,
    bar_bg: 0xF3F3F3,
    edit_bg: 0xFFFFFF,
    editing_bg: 0xFFFDF3,
};

pub const DARK: Palette = Palette {
    dark: true,
    bg: 0x1F1F1F,
    text: 0xE3E3E3,
    dim: 0x9D9D9D,
    pdf_bg: 0x2B2B2B,
    placeholder: 0x3A3A3A,
    page_border: 0x4A4A4A,
    link: 0x4C9EFF,
    code_bg: 0x2A2C30,
    border: 0x3F454D,
    inline_code: (0x8B949E, 0.25),
    selection: (0x3D8BFF, 0.38),
    hit: (0xE0B400, 0.40),
    hit_current: (0xFF8A1F, 0.55),
    side_bg: 0x252526,
    side_text: 0xDCDCDC,
    side_dim: 0x9A9A9A,
    sel_bg: 0x094771,
    sel_bg_inactive: 0x37373D,
    hover_bg: 0x2E2E30,
    accent: 0x4CC2FF,
    status_bg: 0x1B1B1C,
    status_text: 0xC8C8C8,
    splitter: 0x333333,
    bar_bg: 0x2B2B2B,
    edit_bg: 0x1F1F1F,
    editing_bg: 0x26241F,
};

thread_local! {
    static CUR: Cell<Palette> = const { Cell::new(LIGHT) };
    static BRUSHES: RefCell<Vec<(u32, HBRUSH)>> = const { RefCell::new(Vec::new()) };
}

pub fn current() -> Palette {
    CUR.with(Cell::get)
}

pub fn set(p: Palette) {
    CUR.with(|c| c.set(p));
}

/// 0xRRGGBB → GDI COLORREF(0x00BBGGRR)
pub fn colorref(rgb: u32) -> COLORREF {
    COLORREF(((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF))
}

/// 색별로 한 번 만든 GDI 브러시 (테마 색 몇 개뿐이라 해제하지 않는다)
pub fn brush(rgb: u32) -> HBRUSH {
    BRUSHES.with(|b| {
        let mut b = b.borrow_mut();
        if let Some((_, h)) = b.iter().find(|(c, _)| *c == rgb) {
            return *h;
        }
        let h = unsafe { CreateSolidBrush(colorref(rgb)) };
        b.push((rgb, h));
        h
    })
}

/// Windows "앱 모드" 설정이 다크인지 (HKCU\…\Themes\Personalize\AppsUseLightTheme = 0)
pub fn system_dark() -> bool {
    let mut v: u32 = 1;
    let mut size = size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    };
    r.is_ok() && v == 0
}

/// 제목 표시줄 다크 모드 (Windows 11 문서화된 DWM 특성)
pub fn apply_title_bar(hwnd: HWND, dark: bool) {
    let v = BOOL::from(dark);
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &v as *const BOOL as *const c_void,
            size_of::<BOOL>() as u32,
        );
    }
}

/// 스크롤바 등 표준 부품의 테마. 다크는 Explorer의 다크 테마 클래스를 빌린다(비공식이지만 널리 쓰임, §3.4).
pub fn apply_scrollbar_theme(hwnd: HWND, dark: bool) {
    unsafe {
        let _ = SetWindowTheme(
            hwnd,
            if dark {
                w!("DarkMode_Explorer")
            } else {
                w!("Explorer")
            },
            None,
        );
    }
}

/// EDIT 컨트롤 테마 (찾기 막대)
pub fn apply_edit_theme(hwnd: HWND, dark: bool) {
    unsafe {
        let _ = SetWindowTheme(
            hwnd,
            if dark {
                w!("DarkMode_CFD")
            } else {
                w!("Explorer")
            },
            None,
        );
    }
}

/// 우클릭 메뉴를 테마에 맞춘다. uxtheme의 서수 전용 함수(SetPreferredAppMode #135,
/// FlushMenuThemes #136)라 없으면 조용히 넘어간다.
pub fn apply_menu_theme(dark: bool) {
    unsafe {
        let Ok(h) = LoadLibraryW(w!("uxtheme.dll")) else {
            return;
        };
        // 0 Default · 1 AllowDark · 2 ForceDark · 3 ForceLight
        if let Some(f) = GetProcAddress(h, PCSTR(135usize as *const u8)) {
            let f: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            f(if dark { 2 } else { 3 });
        }
        if let Some(f) = GetProcAddress(h, PCSTR(136usize as *const u8)) {
            let f: extern "system" fn() = std::mem::transmute(f);
            f();
        }
    }
}

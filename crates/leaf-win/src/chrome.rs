//! 사이드바 탭 줄과 상태 표시줄. 공용 컨트롤(Tab·StatusBar)은 다크 테마를 그릴 수 없어 직접 그린다 (FR-A03).

use std::cell::RefCell;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};

use crate::theme;
use crate::util::{loword, scale};

const STRIP_CLASS: PCWSTR = w!("LeafViewer.Strip");
/// 탭을 눌렀을 때 부모에 보내는 메시지 (wParam = 탭 번호)
pub const WM_APP_TAB: u32 = WM_APP + 3;
/// 상태 표시줄 오른쪽 칸 폭(DIP)
const STATUS_RIGHT_DIP: i32 = 460;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StripKind {
    Tabs,
    Status,
}

struct State {
    kind: StripKind,
    items: Vec<Vec<u16>>,
    active: usize,
    hover: Option<usize>,
    tracking: bool,
    font: HFONT,
    dpi: u32,
}

pub struct Strip {
    pub hwnd: HWND,
    state: Box<RefCell<State>>,
}

pub fn register(hinst: HINSTANCE) -> Result<()> {
    unsafe {
        RegisterClassExW(&WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(strip_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: STRIP_CLASS,
            ..Default::default()
        });
    }
    Ok(())
}

impl Strip {
    pub fn create(parent: HWND, hinst: HINSTANCE, kind: StripKind, items: &[&str]) -> Result<Self> {
        let state = Box::new(RefCell::new(State {
            kind,
            items: items.iter().map(|s| s.encode_utf16().collect()).collect(),
            active: 0,
            hover: None,
            tracking: false,
            font: HFONT::default(),
            dpi: 96,
        }));
        unsafe {
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                STRIP_CLASS,
                None,
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                0,
                0,
                Some(parent),
                None,
                Some(hinst),
                None,
            )?;
            SetWindowLongPtrW(
                hwnd,
                GWLP_USERDATA,
                &*state as *const RefCell<State> as isize,
            );
            Ok(Self { hwnd, state })
        }
    }

    pub fn set_font(&self, font: HFONT, dpi: u32) {
        {
            let mut s = self.state.borrow_mut();
            s.font = font;
            s.dpi = dpi;
        }
        self.invalidate();
    }

    pub fn set_text(&self, i: usize, text: &str) {
        {
            let mut s = self.state.borrow_mut();
            if s.items.len() <= i {
                s.items.resize(i + 1, Vec::new());
            }
            let t: Vec<u16> = text.encode_utf16().collect();
            if s.items[i] == t {
                return;
            }
            s.items[i] = t;
        }
        self.invalidate();
    }

    pub fn set_active(&self, i: usize) {
        self.state.borrow_mut().active = i;
        self.invalidate();
    }

    pub fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }
}

/// 탭 i의 x 범위
fn tab_span(s: &State, hdc: HDC, i: usize) -> (i32, i32) {
    let pad = scale(14, s.dpi);
    let mut x = scale(6, s.dpi);
    for (k, t) in s.items.iter().enumerate() {
        let mut sz = SIZE::default();
        unsafe {
            let _ = GetTextExtentPoint32W(hdc, t, &mut sz);
        }
        let w = sz.cx + 2 * pad;
        if k == i {
            return (x, x + w);
        }
        x += w;
    }
    (x, x)
}

fn hit_tab(hwnd: HWND, s: &State, x: i32) -> Option<usize> {
    unsafe {
        let hdc = GetDC(Some(hwnd));
        let old = SelectObject(hdc, s.font.into());
        let r = (0..s.items.len()).find(|&i| {
            let (a, b) = tab_span(s, hdc, i);
            x >= a && x < b
        });
        SelectObject(hdc, old);
        ReleaseDC(Some(hwnd), hdc);
        r
    }
}

fn paint(hwnd: HWND, s: &State) {
    let p = theme::current();
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let (w, h) = (rc.right.max(1), rc.bottom.max(1));
        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        let old_bmp = SelectObject(mem, bmp.into());
        let old_font = SelectObject(mem, s.font.into());
        SetBkMode(mem, TRANSPARENT);
        let line = scale(1, s.dpi).max(1);
        match s.kind {
            StripKind::Tabs => {
                FillRect(mem, &rc, theme::brush(p.side_bg));
                for (i, t) in s.items.iter().enumerate() {
                    let (a, b) = tab_span(s, mem, i);
                    let active = i == s.active;
                    if !active && s.hover == Some(i) {
                        let r = RECT {
                            left: a,
                            top: scale(3, s.dpi),
                            right: b,
                            bottom: h - scale(3, s.dpi),
                        };
                        FillRect(mem, &r, theme::brush(p.hover_bg));
                    }
                    SetTextColor(
                        mem,
                        theme::colorref(if active { p.side_text } else { p.side_dim }),
                    );
                    let mut r = RECT {
                        left: a,
                        top: 0,
                        right: b,
                        bottom: h,
                    };
                    let mut t = t.clone();
                    draw_text(
                        mem,
                        &mut t,
                        &mut r,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                    if active {
                        let pad = scale(10, s.dpi);
                        let u = RECT {
                            left: a + pad,
                            top: h - scale(3, s.dpi),
                            right: b - pad,
                            bottom: h - line,
                        };
                        FillRect(mem, &u, theme::brush(p.accent));
                    }
                }
                let bottom = RECT {
                    left: 0,
                    top: h - line,
                    right: w,
                    bottom: h,
                };
                FillRect(mem, &bottom, theme::brush(p.splitter));
            }
            StripKind::Status => {
                FillRect(mem, &rc, theme::brush(p.status_bg));
                let top = RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: line,
                };
                FillRect(mem, &top, theme::brush(p.splitter));
                SetTextColor(mem, theme::colorref(p.status_text));
                let pad = scale(8, s.dpi);
                let split = (w - scale(STATUS_RIGHT_DIP, s.dpi)).max(w / 3);
                if let Some(t) = s.items.first() {
                    let mut r = RECT {
                        left: pad,
                        top: line,
                        right: split - pad,
                        bottom: h,
                    };
                    let mut t = t.clone();
                    draw_text(
                        mem,
                        &mut t,
                        &mut r,
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
                    );
                }
                if let Some(t) = s.items.get(1) {
                    let mut r = RECT {
                        left: split,
                        top: line,
                        right: w - pad,
                        bottom: h,
                    };
                    let mut t = t.clone();
                    draw_text(
                        mem,
                        &mut t,
                        &mut r,
                        DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
                    );
                }
            }
        }
        let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old_font);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

extern "system" fn strip_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        if p.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let cell = &*p;
        match msg {
            WM_PAINT => {
                paint(hwnd, &cell.borrow());
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_SIZE => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let x = loword(lparam.0 as usize) as u16 as i16 as i32;
                let hit = {
                    let s = cell.borrow();
                    if s.kind != StripKind::Tabs {
                        None
                    } else {
                        hit_tab(hwnd, &s, x)
                    }
                };
                if let Some(i) = hit
                    && let Ok(parent) = GetParent(hwnd)
                {
                    let _ = PostMessageW(Some(parent), WM_APP_TAB, WPARAM(i), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let x = loword(lparam.0 as usize) as u16 as i16 as i32;
                let mut s = cell.borrow_mut();
                if s.kind == StripKind::Tabs {
                    if !s.tracking {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        let _ = TrackMouseEvent(&mut tme);
                        s.tracking = true;
                    }
                    let h = hit_tab(hwnd, &s, x);
                    if h != s.hover {
                        s.hover = h;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                let mut s = cell.borrow_mut();
                s.tracking = false;
                if s.hover.take().is_some() {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 빈 문자열은 그리지 않는다: 빈 Vec의 포인터는 가짜(dangling)라 user32가 읽다 죽는다
fn draw_text(hdc: HDC, text: &mut [u16], rc: &mut RECT, fmt: DRAW_TEXT_FORMAT) {
    if !text.is_empty() {
        unsafe {
            DrawTextW(hdc, text, rc, fmt);
        }
    }
}

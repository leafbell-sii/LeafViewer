//! 문서 영역 위의 입력 막대: 찾기(Ctrl+F) / 쪽 이동(Ctrl+G).
//! EDIT 컨트롤을 서브클래싱해 Enter·Shift+Enter·Esc·F3를 가로챈다.

use std::ffi::c_void;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    FillRect, HDC, HFONT, InvalidateRect, SetBkColor, SetTextColor,
};
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};

use crate::theme;
use crate::util::{scale, wide};

pub const BAR_CLASS: PCWSTR = w!("LeafViewer.Bar");
pub const BAR_HEIGHT_DIP: i32 = 36;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BarMode {
    Find,
    /// PDF 쪽 이동
    Goto,
    /// TXT 줄 이동
    GotoLine,
}

/// 막대에서 앱으로 올라가는 이벤트. 막대의 부모(메인 창)에 WM_APP_BAR로 보낸다.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum BarEvent {
    Changed = 1,
    Enter = 2,
    ShiftEnter = 3,
    Escape = 4,
}

pub const WM_APP_BAR: u32 = WM_APP + 2;
/// STATIC 컨트롤 세로 가운데 정렬
const SS_CENTERIMAGE: u32 = 0x0200;

pub struct Bar {
    pub hwnd: HWND,
    pub label: HWND,
    pub edit: HWND,
    pub info: HWND,
}

impl Bar {
    pub fn create(parent: HWND, hinst: HINSTANCE) -> Result<Self> {
        unsafe {
            let cls = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(bar_proc),
                hInstance: hinst,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                lpszClassName: BAR_CLASS,
                ..Default::default()
            };
            RegisterClassExW(&cls);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                BAR_CLASS,
                None,
                WS_CHILD | WS_CLIPCHILDREN,
                0,
                0,
                0,
                0,
                Some(parent),
                None,
                Some(hinst),
                None,
            )?;
            let label = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                WC_STATICW,
                w!("찾기"),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(SS_CENTERIMAGE),
                0,
                0,
                0,
                0,
                Some(hwnd),
                None,
                Some(hinst),
                None,
            )?;
            let edit = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                WC_EDITW,
                None,
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                0,
                0,
                0,
                0,
                Some(hwnd),
                None,
                Some(hinst),
                None,
            )?;
            let info = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                WC_STATICW,
                None,
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(SS_CENTERIMAGE),
                0,
                0,
                0,
                0,
                Some(hwnd),
                None,
                Some(hinst),
                None,
            )?;
            let _ = SetWindowSubclass(edit, Some(edit_subclass), 1, parent.0 as usize);
            Ok(Self {
                hwnd,
                label,
                edit,
                info,
            })
        }
    }

    pub fn set_font(&self, font: HFONT) {
        for h in [self.label, self.edit, self.info] {
            unsafe {
                SendMessageW(
                    h,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
        }
    }

    pub fn layout(&self, w: i32, dpi: u32) {
        let h = scale(BAR_HEIGHT_DIP, dpi);
        let pad = scale(6, dpi);
        let label_w = scale(56, dpi);
        let edit_w = scale(280, dpi).min((w - label_w - pad * 3).max(40));
        unsafe {
            let _ = MoveWindow(self.label, pad, pad, label_w, h - pad * 2, true);
            let _ = MoveWindow(self.edit, pad + label_w, pad, edit_w, h - pad * 2, true);
            let x = pad * 2 + label_w + edit_w;
            let _ = MoveWindow(self.info, x, pad, (w - x - pad).max(0), h - pad * 2, true);
        }
    }

    pub fn set_mode(&self, mode: BarMode) {
        let (label, cue) = match mode {
            BarMode::Find => ("찾기", "문서에서 찾기 (Enter 다음, Shift+Enter 이전)"),
            BarMode::Goto => ("쪽 이동", "쪽 번호 입력 후 Enter"),
            BarMode::GotoLine => ("줄 이동", "줄 번호 입력 후 Enter"),
        };
        let l = wide(label);
        let c = wide(cue);
        unsafe {
            let _ = SetWindowTextW(self.label, PCWSTR(l.as_ptr()));
            SendMessageW(
                self.edit,
                EM_SETCUEBANNER,
                Some(WPARAM(1)),
                Some(LPARAM(c.as_ptr() as isize)),
            );
        }
    }

    pub fn text(&self) -> String {
        unsafe {
            let n = GetWindowTextLengthW(self.edit);
            let mut buf = vec![0u16; n as usize + 1];
            let got = GetWindowTextW(self.edit, &mut buf);
            String::from_utf16_lossy(&buf[..got.max(0) as usize])
        }
    }

    pub fn set_text(&self, s: &str) {
        let ws = wide(s);
        unsafe {
            let _ = SetWindowTextW(self.edit, PCWSTR(ws.as_ptr()));
        }
    }

    pub fn set_info(&self, s: &str) {
        let ws = wide(s);
        unsafe {
            let _ = SetWindowTextW(self.info, PCWSTR(ws.as_ptr()));
        }
    }

    pub fn apply_theme(&self, dark: bool) {
        theme::apply_edit_theme(self.edit, dark);
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, true);
        }
    }

    pub fn focus_select_all(&self) {
        unsafe {
            let _ = SetFocus(Some(self.edit));
            SendMessageW(self.edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        }
    }
}

extern "system" fn bar_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let p = theme::current();
        match msg {
            WM_ERASEBKGND => {
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                FillRect(HDC(wparam.0 as *mut c_void), &rc, theme::brush(p.bar_bg));
                return LRESULT(1);
            }
            // 테마 색 (FR-A03)
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
                let hdc = HDC(wparam.0 as *mut c_void);
                let edit = msg == WM_CTLCOLOREDIT;
                SetTextColor(
                    hdc,
                    theme::colorref(if edit { p.text } else { p.side_text }),
                );
                let bg = if edit { p.edit_bg } else { p.bar_bg };
                SetBkColor(hdc, theme::colorref(bg));
                return LRESULT(theme::brush(bg).0 as isize);
            }
            _ => {}
        }
        if msg == WM_COMMAND && (wparam.0 >> 16) as u32 == EN_CHANGE {
            if let Ok(parent) = GetParent(hwnd) {
                let _ = PostMessageW(
                    Some(parent),
                    WM_APP_BAR,
                    WPARAM(BarEvent::Changed as usize),
                    LPARAM(0),
                );
            }
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

extern "system" fn edit_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    main: usize,
) -> LRESULT {
    unsafe {
        let post = |e: BarEvent| {
            let _ = PostMessageW(
                Some(HWND(main as *mut c_void)),
                WM_APP_BAR,
                WPARAM(e as usize),
                LPARAM(0),
            );
        };
        match msg {
            WM_KEYDOWN if wparam.0 == VK_RETURN.0 as usize => {
                post(if GetKeyState(VK_SHIFT.0 as i32) < 0 {
                    BarEvent::ShiftEnter
                } else {
                    BarEvent::Enter
                });
                return LRESULT(0);
            }
            WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
                post(BarEvent::Escape);
                return LRESULT(0);
            }
            // Enter/Esc의 WM_CHAR는 삼켜 경고음을 막는다
            WM_CHAR if wparam.0 == 13 || wparam.0 == 27 => return LRESULT(0),
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wparam, lparam)
    }
}

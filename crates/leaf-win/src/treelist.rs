//! 사이드바의 가상 트리 목록 (파일 트리·목차). TreeView 대신 보이는 행만 GDI로 그린다 (SPEC Q9).
//! 행 목록은 앱이 모델에서 만들어 통째로 넘긴다(`set_rows`). 이 모듈은 Win32 메시지를 받아
//! 상태를 바꾸고 `ListEvent`를 돌려줄 뿐 앱 상태를 모른다.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::{SetScrollInfo, WM_MOUSELEAVE};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::theme;
use crate::util::{hiword, loword, scale};

pub const LIST_CLASS: PCWSTR = windows::core::w!("LeafViewer.List");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    None,
    Folder,
    File,
    Warning,
}

#[derive(Clone, Debug)]
pub struct Row {
    /// 모델 번호. 음수면 선택할 수 없는 안내 행("불러오는 중…")
    pub key: isize,
    pub depth: u16,
    pub text: Vec<u16>,
    pub icon: Icon,
    pub expandable: bool,
    pub expanded: bool,
    pub dim: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ListEvent {
    /// 선택이 바뀜(키보드·클릭)
    Select(isize),
    /// Enter·더블클릭
    Activate(isize),
    /// 펼치기(true)/접기(false)
    Toggle(isize, bool),
    /// 우클릭·메뉴 키 (화면 좌표)
    Menu(isize, POINT),
}

const ROW_DIP: i32 = 24;
const INDENT_DIP: i32 = 16;
const PAD_DIP: i32 = 6;
const TYPEAHEAD: Duration = Duration::from_millis(900);

// Segoe Fluent Icons 글리프
const GLYPH_RIGHT: u16 = 0xE76C;
const GLYPH_DOWN: u16 = 0xE70D;
const GLYPH_FOLDER: u16 = 0xE8B7;
const GLYPH_FILE: u16 = 0xE8A5;
const GLYPH_WARN: u16 = 0xE7BA;
const FOLDER_COLOR: u32 = 0xDCA94A;

pub struct ListState {
    pub hwnd: HWND,
    pub rows: Vec<Row>,
    sel: Option<usize>,
    top: usize,
    hover: Option<usize>,
    focused: bool,
    tracking: bool,
    typed: Vec<u16>,
    typed_at: Option<Instant>,
    dpi: u32,
    wheel_rest: i32,
}

pub struct Fonts {
    pub text: HFONT,
    pub icon: HFONT,
}

impl ListState {
    pub fn new(hwnd: HWND) -> Self {
        Self {
            hwnd,
            rows: Vec::new(),
            sel: None,
            top: 0,
            hover: None,
            focused: false,
            tracking: false,
            typed: Vec::new(),
            typed_at: None,
            dpi: 96,
            wheel_rest: 0,
        }
    }

    pub fn set_dpi(&mut self, dpi: u32) {
        self.dpi = dpi;
        self.update_scroll();
    }

    fn row_h(&self) -> i32 {
        scale(ROW_DIP, self.dpi)
    }

    fn page_rows(&self) -> usize {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        ((rc.bottom - rc.top) / self.row_h().max(1)).max(1) as usize
    }

    /// 행 목록 교체. 선택·맨 위 행은 같은 key를 따라간다.
    pub fn set_rows(&mut self, rows: Vec<Row>) {
        let sel_key = self.selected_key();
        let top_key = self.rows.get(self.top).map(|r| r.key);
        let old_sel = self.sel;
        self.rows = rows;
        self.sel = match sel_key {
            Some(k) => self
                .index_of(k)
                .or(old_sel.map(|i| i.min(self.rows.len().saturating_sub(1)))),
            None => None,
        };
        if let Some(k) = top_key
            && let Some(i) = self.index_of(k).filter(|_| k >= 0)
        {
            self.top = i;
        }
        self.hover = None;
        self.clamp_top();
        self.update_scroll();
        self.invalidate();
    }

    fn index_of(&self, key: isize) -> Option<usize> {
        self.rows.iter().position(|r| r.key == key)
    }

    pub fn selected_key(&self) -> Option<isize> {
        self.sel.and_then(|i| self.rows.get(i)).map(|r| r.key)
    }

    /// 프로그램이 선택을 바꾼다 (이벤트 없음). 없는 key면 선택 해제.
    pub fn select_key(&mut self, key: Option<isize>, ensure_visible: bool) {
        self.sel = key.and_then(|k| self.index_of(k));
        if ensure_visible && let Some(i) = self.sel {
            self.ensure_visible(i);
        }
        self.invalidate();
    }

    /// 선택한 행을 화면 맨 위 근처에 둔다 (목차 처음 표시 등)
    pub fn scroll_to_top(&mut self) {
        self.top = 0;
        self.update_scroll();
        self.invalidate();
    }

    fn ensure_visible(&mut self, i: usize) {
        let page = self.page_rows();
        if i < self.top {
            self.top = i;
        } else if i >= self.top + page {
            self.top = i + 1 - page;
        }
        self.clamp_top();
        self.update_scroll();
    }

    fn clamp_top(&mut self) {
        let max = self.rows.len().saturating_sub(self.page_rows());
        self.top = self.top.min(max);
    }

    pub fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn update_scroll(&self) {
        let si = SCROLLINFO {
            cbSize: size_of::<SCROLLINFO>() as u32,
            fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
            nMin: 0,
            nMax: self.rows.len().saturating_sub(1) as i32,
            nPage: self.page_rows() as u32,
            nPos: self.top as i32,
            nTrackPos: 0,
        };
        unsafe {
            SetScrollInfo(self.hwnd, SB_VERT, &si, true);
        }
    }

    fn scroll_rows(&mut self, d: isize) {
        let t = (self.top as isize + d).max(0) as usize;
        if t != self.top {
            self.top = t;
            self.clamp_top();
            self.update_scroll();
            self.invalidate();
        }
    }

    fn row_at(&self, y: i32) -> Option<usize> {
        if y < 0 {
            return None;
        }
        let i = self.top + (y / self.row_h().max(1)) as usize;
        (i < self.rows.len()).then_some(i)
    }

    /// 펼침 표시(▸) 영역의 x 범위
    fn chevron_x(&self, r: &Row) -> (i32, i32) {
        let x = scale(PAD_DIP, self.dpi) + r.depth as i32 * scale(INDENT_DIP, self.dpi);
        (x, x + scale(INDENT_DIP, self.dpi))
    }

    fn move_sel(&mut self, to: usize, out: &mut Vec<ListEvent>) {
        if self.rows.is_empty() {
            return;
        }
        let to = to.min(self.rows.len() - 1);
        // 안내 행은 건너뛰지 않고 선택만 막는다
        if self.rows[to].key < 0 {
            return;
        }
        let changed = self.sel != Some(to);
        self.sel = Some(to);
        self.ensure_visible(to);
        self.invalidate();
        if changed {
            out.push(ListEvent::Select(self.rows[to].key));
        }
    }

    /// 메시지 처리. 처리했으면 Some(결과)
    pub fn on_msg(
        &mut self,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        fonts: &Fonts,
        out: &mut Vec<ListEvent>,
    ) -> Option<LRESULT> {
        let x = loword(lparam.0 as usize) as u16 as i16 as i32;
        let y = hiword(lparam.0 as usize) as u16 as i16 as i32;
        match msg {
            WM_PAINT => {
                self.paint(fonts);
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_SIZE => {
                self.clamp_top();
                self.update_scroll();
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_SETFOCUS | WM_KILLFOCUS => {
                self.focused = msg == WM_SETFOCUS;
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_GETDLGCODE => Some(LRESULT((DLGC_WANTARROWS | DLGC_WANTCHARS) as isize)),
            WM_VSCROLL => {
                let page = self.page_rows() as isize;
                match SCROLLBAR_COMMAND(loword(wparam.0) as i32) {
                    SB_LINEUP => self.scroll_rows(-1),
                    SB_LINEDOWN => self.scroll_rows(1),
                    SB_PAGEUP => self.scroll_rows(-page),
                    SB_PAGEDOWN => self.scroll_rows(page),
                    SB_TOP => self.scroll_rows(-(self.top as isize)),
                    SB_BOTTOM => self.scroll_rows(self.rows.len() as isize),
                    SB_THUMBTRACK | SB_THUMBPOSITION => {
                        let mut si = SCROLLINFO {
                            cbSize: size_of::<SCROLLINFO>() as u32,
                            fMask: SIF_TRACKPOS,
                            ..Default::default()
                        };
                        unsafe {
                            let _ = GetScrollInfo(self.hwnd, SB_VERT, &mut si);
                        }
                        self.scroll_rows(si.nTrackPos as isize - self.top as isize);
                    }
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let mut lines: u32 = 3;
                unsafe {
                    let _ = SystemParametersInfoW(
                        SPI_GETWHEELSCROLLLINES,
                        0,
                        Some(&mut lines as *mut u32 as *mut _),
                        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                    );
                }
                self.wheel_rest += hiword(wparam.0) as u16 as i16 as i32;
                let notches = self.wheel_rest / 120;
                self.wheel_rest %= 120;
                self.scroll_rows(-(notches as isize) * lines.max(1) as isize);
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                unsafe {
                    let _ = SetFocus(Some(self.hwnd));
                }
                if let Some(i) = self.row_at(y) {
                    let r = &self.rows[i];
                    let (cx0, cx1) = self.chevron_x(r);
                    if r.expandable && x >= cx0 && x < cx1 {
                        out.push(ListEvent::Toggle(r.key, !r.expanded));
                    } else {
                        self.move_sel(i, out);
                    }
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDBLCLK => {
                if let Some(i) = self.row_at(y) {
                    let r = &self.rows[i];
                    let (_, cx1) = self.chevron_x(r);
                    if r.key >= 0 && x >= cx1 {
                        out.push(if r.expandable {
                            ListEvent::Toggle(r.key, !r.expanded)
                        } else {
                            ListEvent::Activate(r.key)
                        });
                    }
                }
                Some(LRESULT(0))
            }
            WM_RBUTTONDOWN => {
                unsafe {
                    let _ = SetFocus(Some(self.hwnd));
                }
                if let Some(i) = self.row_at(y) {
                    self.move_sel(i, out);
                }
                Some(LRESULT(0))
            }
            WM_RBUTTONUP => {
                if let Some(i) = self.row_at(y)
                    && self.rows[i].key >= 0
                {
                    let mut pt = POINT { x, y };
                    unsafe {
                        let _ = ClientToScreen(self.hwnd, &mut pt);
                    }
                    out.push(ListEvent::Menu(self.rows[i].key, pt));
                }
                Some(LRESULT(0))
            }
            WM_CONTEXTMENU if lparam.0 == -1 => {
                // 메뉴 키·Shift+F10: 선택한 행 위치에
                if let Some(i) = self.sel {
                    self.ensure_visible(i);
                    let r = &self.rows[i];
                    let mut pt = POINT {
                        x: self.chevron_x(r).1,
                        y: (i - self.top) as i32 * self.row_h() + self.row_h(),
                    };
                    unsafe {
                        let _ = ClientToScreen(self.hwnd, &mut pt);
                    }
                    out.push(ListEvent::Menu(r.key, pt));
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                if !self.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.hwnd,
                        dwHoverTime: 0,
                    };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                    self.tracking = true;
                }
                let h = self.row_at(y).filter(|&i| self.rows[i].key >= 0);
                if h != self.hover {
                    self.hover = h;
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                self.tracking = false;
                if self.hover.take().is_some() {
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                self.on_key(VIRTUAL_KEY(wparam.0 as u16), out);
                Some(LRESULT(0))
            }
            WM_CHAR => {
                self.on_char(wparam.0 as u16, out);
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    fn on_key(&mut self, vk: VIRTUAL_KEY, out: &mut Vec<ListEvent>) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let cur = self.sel;
        let page = self.page_rows().saturating_sub(1).max(1);
        // 위/아래로 움직일 때 안내 행(key < 0)은 건너뛴다
        let step = |from: usize, down: bool, rows: &[Row]| -> usize {
            let mut i = from;
            loop {
                let next = if down {
                    if i + 1 >= rows.len() {
                        return from;
                    }
                    i + 1
                } else {
                    if i == 0 {
                        return from;
                    }
                    i - 1
                };
                if rows[next].key >= 0 {
                    return next;
                }
                i = next;
            }
        };
        match vk {
            VK_DOWN => {
                let to = cur.map_or(0, |c| step(c, true, &self.rows));
                self.move_sel(to, out);
            }
            VK_UP => {
                let to = cur.map_or(0, |c| step(c, false, &self.rows));
                self.move_sel(to, out);
            }
            VK_NEXT => self.move_sel(cur.map_or(0, |c| (c + page).min(n - 1)), out),
            VK_PRIOR => self.move_sel(cur.map_or(0, |c| c.saturating_sub(page)), out),
            VK_HOME => self.move_sel(0, out),
            VK_END => self.move_sel(n - 1, out),
            VK_LEFT => {
                let Some(c) = cur else { return };
                let r = &self.rows[c];
                if r.expandable && r.expanded {
                    out.push(ListEvent::Toggle(r.key, false));
                } else if r.depth > 0 {
                    let d = r.depth;
                    if let Some(p) = (0..c).rev().find(|&i| self.rows[i].depth < d) {
                        self.move_sel(p, out);
                    }
                }
            }
            VK_RIGHT => {
                let Some(c) = cur else { return };
                let r = &self.rows[c];
                if r.expandable && !r.expanded {
                    out.push(ListEvent::Toggle(r.key, true));
                } else if r.expandable && c + 1 < n && self.rows[c + 1].depth > r.depth {
                    self.move_sel(c + 1, out);
                }
            }
            VK_RETURN => {
                if let Some(c) = cur {
                    out.push(ListEvent::Activate(self.rows[c].key));
                }
            }
            _ => {}
        }
    }

    /// 입력한 글자로 시작하는 행으로 이동 (탐색기와 같은 타자 검색)
    fn on_char(&mut self, ch: u16, out: &mut Vec<ListEvent>) {
        if ch < 0x20 || self.rows.is_empty() {
            return;
        }
        let now = Instant::now();
        let fresh = self.typed_at.is_none_or(|t| now - t > TYPEAHEAD);
        if fresh {
            self.typed.clear();
        }
        self.typed_at = Some(now);
        self.typed.push(ch);
        let q = String::from_utf16_lossy(&self.typed).to_lowercase();
        let n = self.rows.len();
        // 같은 글자를 반복해 누르면 다음 항목으로
        let repeat = self.typed.len() > 1 && self.typed.iter().all(|&c| c == self.typed[0]);
        let (q, start) = if repeat || self.typed.len() == 1 {
            (
                String::from_utf16_lossy(&[ch]).to_lowercase(),
                self.sel.map_or(0, |s| s + 1),
            )
        } else {
            (q, self.sel.unwrap_or(0))
        };
        for k in 0..n {
            let i = (start + k) % n;
            let r = &self.rows[i];
            if r.key >= 0
                && String::from_utf16_lossy(&r.text)
                    .to_lowercase()
                    .starts_with(&q)
            {
                self.move_sel(i, out);
                return;
            }
        }
    }

    fn paint(&mut self, fonts: &Fonts) {
        let p = theme::current();
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(self.hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rc);
            let (w, h) = (rc.right.max(1), rc.bottom.max(1));
            let mem = CreateCompatibleDC(Some(hdc));
            let bmp = CreateCompatibleBitmap(hdc, w, h);
            let old_bmp = SelectObject(mem, bmp.into());
            FillRect(mem, &rc, theme::brush(p.side_bg));
            SetBkMode(mem, TRANSPARENT);
            let old_font = SelectObject(mem, fonts.text.into());
            let rh = self.row_h();
            let icon_w = scale(20, self.dpi);
            let end = (self.top + (h / rh.max(1)) as usize + 1).min(self.rows.len());
            for i in self.top..end {
                let r = &self.rows[i];
                let y = (i - self.top) as i32 * rh;
                let row_rc = RECT {
                    left: 0,
                    top: y,
                    right: w,
                    bottom: y + rh,
                };
                if self.sel == Some(i) {
                    let c = if self.focused {
                        p.sel_bg
                    } else {
                        p.sel_bg_inactive
                    };
                    FillRect(mem, &row_rc, theme::brush(c));
                } else if self.hover == Some(i) {
                    FillRect(mem, &row_rc, theme::brush(p.hover_bg));
                }
                let (cx0, cx1) = self.chevron_x(r);
                let mut x = cx1;
                if r.expandable {
                    SelectObject(mem, fonts.icon.into());
                    SetTextColor(mem, theme::colorref(p.side_dim));
                    let mut g = [if r.expanded { GLYPH_DOWN } else { GLYPH_RIGHT }];
                    let mut grc = RECT {
                        left: cx0,
                        top: y,
                        right: cx1,
                        bottom: y + rh,
                    };
                    draw_text(
                        mem,
                        &mut g,
                        &mut grc,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                    SelectObject(mem, fonts.text.into());
                }
                let glyph = match r.icon {
                    Icon::Folder => Some((GLYPH_FOLDER, FOLDER_COLOR)),
                    Icon::File => Some((GLYPH_FILE, p.side_dim)),
                    Icon::Warning => Some((GLYPH_WARN, 0xD08000)),
                    Icon::None => None,
                };
                if let Some((g, color)) = glyph {
                    SelectObject(mem, fonts.icon.into());
                    SetTextColor(mem, theme::colorref(color));
                    let mut g = [g];
                    let mut grc = RECT {
                        left: x,
                        top: y,
                        right: x + icon_w,
                        bottom: y + rh,
                    };
                    draw_text(
                        mem,
                        &mut g,
                        &mut grc,
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                    SelectObject(mem, fonts.text.into());
                    x += icon_w;
                }
                SetTextColor(
                    mem,
                    theme::colorref(if r.dim { p.side_dim } else { p.side_text }),
                );
                let mut text = r.text.clone();
                let mut trc = RECT {
                    left: x,
                    top: y,
                    right: w - scale(4, self.dpi),
                    bottom: y + rh,
                };
                draw_text(
                    mem,
                    &mut text,
                    &mut trc,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
                );
            }
            let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
            SelectObject(mem, old_font);
            SelectObject(mem, old_bmp);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            let _ = EndPaint(self.hwnd, &ps);
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

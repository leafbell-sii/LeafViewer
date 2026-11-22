//! TXT·MD 편집: 편집하는 동안만 표준 EDIT 컨트롤을 문서 영역에 띄운다.
//!
//! 메모리를 가장 적게 쓰는 방법으로 고른 것이다(추가 DLL 없음, 끝내면 컨트롤을 파괴해 0으로 돌아감).
//! 본문은 EDIT 컨트롤 안에만 UTF-16으로 한 벌 있다. 실행 취소는 EDIT의 1단계뿐이다.
//! 인코딩·BOM·줄바꿈은 열 때 기억해 두었다가 저장할 때 그대로 되돌린다(`leaf_fmt_text::edit`).

use std::cell::Cell;
use std::path::{Path, PathBuf};

use leaf_core::config;
use leaf_fmt_text::TextEncoding;
use leaf_fmt_text::edit::{self, Eol};
use windows::Win32::Foundation::*;
use windows::Win32::Globalization::GetACP;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::theme;
use crate::util::{scale, wide};

/// 편집할 수 있는 파일 크기 상한. EDIT 컨트롤이 쾌적한 범위
pub const EDIT_LIMIT: u64 = 4 * 1024 * 1024;

pub enum SaveError {
    /// 원래 인코딩으로 표현할 수 없는 글자가 있다
    Unmappable,
    Io(String),
}

pub struct Editor {
    pub hwnd: HWND,
    pub path: PathBuf,
    enc: Cell<TextEncoding>,
    eol: Eol,
    font: Cell<HFONT>,
    /// 상태 표시줄에 마지막으로 알린 수정 여부
    pub shown_dirty: Cell<bool>,
    /// 한 번이라도 저장했다
    pub saved: Cell<bool>,
}

/// 편집 대상인 포맷인지 (텍스트·MD)
pub fn is_editable(p: &Path) -> bool {
    leaf_core::format::registry()
        .by_path(p)
        .is_some_and(|f| f.id == "text" || f.id == "markdown")
}

impl Editor {
    /// 파일을 읽어 EDIT 컨트롤을 만든다. 실패하면 사용자에게 보일 사유.
    /// 디코딩 오류가 있으면 `confirm_lossy`에 물어 계속할지 정한다.
    pub fn open(
        parent: HWND,
        path: &Path,
        cfg: &config::Text,
        dpi: u32,
        confirm_lossy: impl FnOnce() -> bool,
    ) -> Result<Editor, String> {
        let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
        if meta.len() > EDIT_LIMIT {
            return Err(format!(
                "{}MB보다 큰 파일은 편집할 수 없습니다. 외부 편집기(Ctrl+Shift+E)를 쓰세요.",
                EDIT_LIMIT / 1024 / 1024
            ));
        }
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let t = edit::decode(&bytes, unsafe { GetACP() });
        drop(bytes);
        if t.has_nul {
            return Err("NUL 글자가 들어 있어 편집할 수 없습니다.".into());
        }
        if t.had_errors && !confirm_lossy() {
            return Err("편집을 취소했습니다.".into());
        }
        let style = WS_CHILD
            | WS_VSCROLL
            | WS_TABSTOP
            | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL | ES_NOHIDESEL | ES_WANTRETURN) as u32)
            | if cfg.word_wrap {
                WINDOW_STYLE(0)
            } else {
                WS_HSCROLL | WINDOW_STYLE(ES_AUTOHSCROLL as u32)
            };
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                WC_EDITW,
                None,
                style,
                0,
                0,
                0,
                0,
                Some(parent),
                None,
                None,
                None,
            )
            .map_err(|e| e.to_string())?
        };
        let ed = Editor {
            hwnd,
            path: path.to_path_buf(),
            enc: Cell::new(t.enc),
            eol: t.eol,
            font: Cell::new(HFONT::default()),
            shown_dirty: Cell::new(false),
            saved: Cell::new(false),
        };
        unsafe {
            // 기본 상한(32K 글자)을 없앤다. 4MB 파일이면 UTF-16으로 최대 8MB
            SendMessageW(hwnd, EM_SETLIMITTEXT, Some(WPARAM(0x7FFF_FFFE)), None);
            ed.set_font(cfg, dpi);
            let w = wide(&t.text);
            drop(t);
            let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
            SendMessageW(hwnd, EM_SETMODIFY, Some(WPARAM(0)), None);
            SendMessageW(hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(0)));
        }
        Ok(ed)
    }

    /// 글꼴·탭 폭·여백 (설정 `[text]`, DPI)
    pub fn set_font(&self, cfg: &config::Text, dpi: u32) {
        let face = wide(&cfg.font);
        let mut lf = LOGFONTW {
            lfHeight: -((cfg.font_size.clamp(4.0, 96.0) * dpi as f32 / 72.0).round() as i32),
            lfCharSet: DEFAULT_CHARSET,
            lfQuality: CLEARTYPE_QUALITY,
            lfPitchAndFamily: FIXED_PITCH.0 | FF_MODERN.0,
            ..Default::default()
        };
        for (d, s) in lf.lfFaceName.iter_mut().zip(face.iter().take(31)) {
            *d = *s;
        }
        unsafe {
            let font = CreateFontIndirectW(&lf);
            SendMessageW(
                self.hwnd,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(1)),
            );
            let old = self.font.replace(font);
            if !old.is_invalid() {
                let _ = DeleteObject(old.into());
            }
            // 탭 폭: 대화 상자 단위(평균 글자 폭의 1/4)
            let tabs = [(cfg.tab_width.clamp(1, 16) * 4) as i32];
            SendMessageW(
                self.hwnd,
                EM_SETTABSTOPS,
                Some(WPARAM(1)),
                Some(LPARAM(tabs.as_ptr() as isize)),
            );
            let m = scale(16, dpi) as usize;
            SendMessageW(
                self.hwnd,
                EM_SETMARGINS,
                Some(WPARAM((EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize)),
                Some(LPARAM((m | (m << 16)) as isize)),
            );
        }
    }

    pub fn apply_theme(&self, dark: bool) {
        theme::apply_scrollbar_theme(self.hwnd, dark);
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, true);
        }
    }

    pub fn is_modified(&self) -> bool {
        unsafe { SendMessageW(self.hwnd, EM_GETMODIFY, None, None).0 != 0 }
    }

    pub fn select_all(&self) {
        unsafe {
            SendMessageW(self.hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        }
    }

    pub fn encoding_label(&self) -> &'static str {
        self.enc.get().label
    }

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn text(&self) -> String {
        unsafe {
            let n = GetWindowTextLengthW(self.hwnd).max(0) as usize;
            let mut buf = vec![0u16; n + 1];
            let got = GetWindowTextW(self.hwnd, &mut buf).max(0) as usize;
            String::from_utf16_lossy(&buf[..got.min(n)])
        }
    }

    /// 저장. `to_utf8`이면 인코딩을 UTF-8(BOM 없음)로 바꿔 저장하고 이후로도 UTF-8로 둔다.
    pub fn save(&self, to_utf8: bool) -> Result<(), SaveError> {
        if to_utf8 {
            self.enc.set(TextEncoding::new(encoding_rs::UTF_8, 0));
        }
        let bytes =
            edit::encode(&self.enc.get(), &self.text(), self.eol).ok_or(SaveError::Unmappable)?;
        std::fs::write(&self.path, bytes).map_err(|e| SaveError::Io(e.to_string()))?;
        unsafe {
            SendMessageW(self.hwnd, EM_SETMODIFY, Some(WPARAM(0)), None);
        }
        self.saved.set(true);
        Ok(())
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            let f = self.font.get();
            if !f.is_invalid() {
                let _ = DeleteObject(f.into());
            }
        }
    }
}

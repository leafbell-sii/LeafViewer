//! 메인 창, 사이드바(파일 트리 / 목차 탭), 찾기 막대, 문서 영역(D2D 자식 창).
//!
//! 재진입 규칙: 창 프로시저는 컨트롤에 메시지를 보내는 동안 다시 호출될 수 있다(WM_NOTIFY 등).
//! 그래서 `App`의 상태는 필드별 `Cell`/`RefCell`로 나누고, **컨트롤에 SendMessage 하는 동안에는
//! 어떤 RefCell 빌림도 들고 있지 않는다.** PDF 보기는 요청을 `Cmd`로 돌려주고, 빌림을 놓은 뒤 처리한다.

use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, unbounded};
use leaf_core::fs_tree::{self, Entry, ListOptions};
use leaf_core::perf;
use leaf_core::session::{self, Session, WindowState};
use leaf_core::worker::WorkerPool;
use leaf_doc::Dest;
use leaf_doc::paged::ZoomMode;
use leaf_fmt_pdf::{PdfEngine, PdfEvent};
use leaf_fmt_text::search::Hit;
use leaf_fmt_text::{LineIndex, TextDoc};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR, Result, w};

use crate::StartOptions;
use crate::bar::{BAR_HEIGHT_DIP, Bar, BarEvent, BarMode, WM_APP_BAR};
use crate::pdfview::{self, Cmd, CursorKind, PdfView};
use crate::render::{self, Renderer, rgb};
use crate::textload;
use crate::textview::{self, CopyJob, SearchJob, TextView};
use crate::tree::{LoadState, Node, TreeModel};
use crate::util::*;

const MAIN_CLASS: PCWSTR = w!("LeafViewer.Main");
const DOC_CLASS: PCWSTR = w!("LeafViewer.Doc");

const WM_APP_WAKE: u32 = WM_APP + 1;
const ID_TREE: usize = 100;
const ID_STATUS: usize = 101;
const ID_TABS: usize = 102;
const ID_OUTLINE: usize = 103;
const ID_OPEN_FOLDER: usize = 200;
const TIMER_SELECT: usize = 1;
const TIMER_FIND: usize = 2;
const TIMER_BENCH: usize = 3;

const SIDEBAR_DIP: i32 = 280;
const TABS_DIP: i32 = 30;
const MARGIN: f32 = 16.0;
const LPSTR_TEXTCALLBACKW: PWSTR = PWSTR(-1isize as *mut u16);
const NO_NODE: isize = -1;
const WHEEL_STEP: f32 = 60.0;
/// 스크롤 벤치마크: 틱마다 내리는 양(DIP)과 최대 틱 수
const BENCH_STEP: f32 = 40.0;
const BENCH_MAX_TICKS: usize = 600;

// ───────────────────────── 워커 → UI 메시지 ─────────────────────────

enum UiMsg {
    DirListed {
        generation: u64,
        node: usize,
        result: std::result::Result<Vec<Entry>, String>,
    },
    TextLoaded {
        generation: u64,
        result: std::result::Result<LoadedText, String>,
    },
    TextIndexProgress {
        generation: u64,
        frac: f32,
    },
    TextIndexed {
        generation: u64,
        index: LineIndex,
        ms: f64,
    },
    TextHits {
        generation: u64,
        id: u64,
        hits: Vec<Hit>,
        done_bytes: usize,
        finished: bool,
        capped: bool,
    },
    TextCopied {
        generation: u64,
        result: std::result::Result<String, String>,
    },
    Pdf(PdfEvent),
}

struct LoadedText {
    doc: Arc<TextDoc>,
    size: u64,
    mapped: bool,
    /// 작은 파일은 읽는 김에 줄 색인까지 만든다
    index: Option<LineIndex>,
    load_ms: f64,
}

/// 다른 스레드에서 UI 스레드를 깨운다. HWND는 Send가 아니므로 정수로 보관.
#[derive(Clone)]
struct Waker {
    tx: Sender<UiMsg>,
    hwnd: usize,
}

impl Waker {
    fn send(&self, msg: UiMsg) {
        if self.tx.send(msg).is_ok() {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(self.hwnd as *mut c_void)),
                    WM_APP_WAKE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}

// ───────────────────────── 문서 영역 상태 ─────────────────────────

enum Content {
    Welcome,
    Message(String),
    Text(Box<TextView>),
    Pdf(Box<PdfView>),
}

struct DocView {
    renderer: Renderer,
    content: Content,
    /// 열기 진행 중인 요청 (세대, 경로, 시작 시각). 로딩 중엔 이전 문서를 유지한다 (FR-V01).
    loading: Option<(u64, PathBuf, Instant)>,
    info: String,
}

#[derive(Default)]
struct PaintOutcome {
    /// 문서를 연 뒤 첫 그리기면 열기 → 첫 화면 시간 (NFR-P03/P04)
    text_first_paint_ms: Option<f64>,
    cmds: Vec<Cmd>,
    generation: u64,
    status: Option<String>,
}

type ScrollMetrics = ((f64, f64, f64), (f64, f64, f64));
/// 마지막으로 설정한 스크롤바 값 (가로 전체·페이지·위치, 세로 전체·페이지·위치)
type ScrollKey = (i32, i32, i32, i32, i32, i32);

#[derive(Default)]
struct Bench {
    running: bool,
    ticks: usize,
    paints: Vec<f64>,
    /// 고해상도가 아직 없는 쪽이 보였던 프레임 수 (저해상도·자리표시자로 그림)
    incomplete: usize,
    started: Option<Instant>,
}

// ───────────────────────── 앱 ─────────────────────────

struct App {
    hwnd: HWND,
    tabs: HWND,
    tree: HWND,
    outline: HWND,
    status: HWND,
    doc: HWND,
    bar: Bar,
    bar_mode: Cell<Option<BarMode>>,
    font: Cell<HFONT>,
    model: RefCell<TreeModel>,
    outline_dests: RefCell<Vec<Option<Dest>>>,
    view: RefCell<DocView>,
    workers: WorkerPool,
    pdf: RefCell<PdfEngine>,
    waker: Waker,
    rx: Receiver<UiMsg>,
    doc_gen: Cell<u64>,
    /// 트리를 펼치며 찾아갈 경로: (지금 읽기를 기다리는 노드, 남은 경로 조각)
    pending_select: RefCell<Option<(usize, Vec<OsString>)>>,
    pending_open: Cell<Option<usize>>,
    first_paint_logged: Cell<bool>,
    perf_text: RefCell<String>,
    cursor: Cell<CursorKind>,
    scroll_cache: Cell<Option<ScrollKey>>,
    /// 스크롤바 값 축소 비율 (i32 범위를 넘는 바이트 단위용)
    scroll_scale: Cell<f64>,
    session: RefCell<Session>,
    session_path: Option<PathBuf>,
    bench: RefCell<Option<Bench>>,
}

fn app_of<'a>(hwnd: HWND) -> Option<&'a App> {
    let p = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const App;
    if p.is_null() {
        None
    } else {
        Some(unsafe { &*p })
    }
}

fn create_child(
    class: PCWSTR,
    style: WINDOW_STYLE,
    parent: HWND,
    id: usize,
    hinst: HINSTANCE,
) -> Result<HWND> {
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            None,
            WS_CHILD | style,
            0,
            0,
            0,
            0,
            Some(parent),
            Some(HMENU(id as *mut c_void)),
            Some(hinst),
            None,
        )
    }
}

fn tree_style() -> WINDOW_STYLE {
    WS_TABSTOP
        | WINDOW_STYLE(
            TVS_HASBUTTONS
                | TVS_LINESATROOT
                | TVS_SHOWSELALWAYS
                | TVS_FULLROWSELECT
                | TVS_TRACKSELECT,
        )
}

pub fn run(opts: StartOptions) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok()?;
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_TREEVIEW_CLASSES | ICC_BAR_CLASSES | ICC_TAB_CLASSES | ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);

        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        let cursor = LoadCursorW(None, IDC_ARROW)?;
        RegisterClassExW(&WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(main_proc),
            hInstance: hinst,
            hCursor: cursor,
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut c_void),
            lpszClassName: MAIN_CLASS,
            ..Default::default()
        });
        RegisterClassExW(&WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(doc_proc),
            hInstance: hinst,
            hCursor: HCURSOR::default(), // WM_SETCURSOR에서 정한다
            lpszClassName: DOC_CLASS,
            ..Default::default()
        });

        let hwnd = CreateWindowExW(
            WS_EX_ACCEPTFILES,
            MAIN_CLASS,
            w!("LeafViewer"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(hinst),
            None,
        )?;
        let dpi = GetDpiForWindow(hwnd).max(96);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            scale(1200, dpi),
            scale(800, dpi),
            SWP_NOMOVE | SWP_NOZORDER,
        );

        let tabs = create_child(
            WC_TABCONTROLW,
            WS_VISIBLE | WS_CLIPSIBLINGS,
            hwnd,
            ID_TABS,
            hinst,
        )?;
        for (i, label) in ["파일", "목차"].iter().enumerate() {
            let mut text = wide(label);
            let item = TCITEMW {
                mask: TCIF_TEXT,
                pszText: PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            SendMessageW(
                tabs,
                TCM_INSERTITEMW,
                Some(WPARAM(i)),
                Some(LPARAM(&item as *const _ as isize)),
            );
        }
        let tree = create_child(
            WC_TREEVIEWW,
            WS_VISIBLE | tree_style(),
            hwnd,
            ID_TREE,
            hinst,
        )?;
        let outline = create_child(WC_TREEVIEWW, tree_style(), hwnd, ID_OUTLINE, hinst)?;
        for t in [tree, outline] {
            let _ = SetWindowTheme(t, w!("Explorer"), None);
            SendMessageW(
                t,
                TVM_SETEXTENDEDSTYLE,
                Some(WPARAM(TVS_EX_DOUBLEBUFFER as usize)),
                Some(LPARAM(TVS_EX_DOUBLEBUFFER as isize)),
            );
        }
        let status = create_child(
            STATUSCLASSNAMEW,
            WS_VISIBLE | WINDOW_STYLE(SBARS_SIZEGRIP),
            hwnd,
            ID_STATUS,
            hinst,
        )?;
        let doc = create_child(DOC_CLASS, WS_VISIBLE | WS_TABSTOP, hwnd, 0, hinst)?;
        let bar = Bar::create(hwnd, hinst)?;

        let session_path = session::default_path();
        let session = session_path
            .as_deref()
            .map(Session::load)
            .unwrap_or_default();

        let (tx, rx) = unbounded();
        let waker = Waker {
            tx,
            hwnd: hwnd.0 as usize,
        };
        let pdf_waker = waker.clone();
        let app = Box::new(App {
            hwnd,
            tabs,
            tree,
            outline,
            status,
            doc,
            bar,
            bar_mode: Cell::new(None),
            font: Cell::new(HFONT::default()),
            model: RefCell::new(TreeModel::default()),
            outline_dests: RefCell::new(Vec::new()),
            view: RefCell::new(DocView {
                renderer: Renderer::new(dpi)?,
                content: Content::Welcome,
                loading: None,
                info: String::new(),
            }),
            // 폴더 읽기 · 줄 색인 · 찾기가 서로 막지 않도록 3개 (SPEC §4.2: 2–4)
            workers: WorkerPool::new(3),
            pdf: RefCell::new(PdfEngine::new(opts.pdfium_path, move |ev| {
                pdf_waker.send(UiMsg::Pdf(ev))
            })),
            waker,
            rx,
            doc_gen: Cell::new(0),
            pending_select: RefCell::new(None),
            pending_open: Cell::new(None),
            first_paint_logged: Cell::new(false),
            perf_text: RefCell::new(String::new()),
            cursor: Cell::new(CursorKind::Arrow),
            scroll_cache: Cell::new(None),
            scroll_scale: Cell::new(1.0),
            session: RefCell::new(session),
            session_path,
            bench: RefCell::new(opts.bench_scroll.then(Bench::default)),
        });
        let app_ptr = Box::into_raw(app);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_ptr as isize);
        SetWindowLongPtrW(doc, GWLP_USERDATA, app_ptr as isize);
        let app = &*app_ptr;

        app.apply_font(dpi);
        app.fill_outline(&[]);
        app.set_status(0, "폴더를 열려면 Ctrl+O 또는 끌어다 놓기");

        // 세션 복원: 창 위치 (FR-A02)
        let saved_window = app.session.borrow().window;
        let mut show = SW_SHOWDEFAULT;
        if let Some(ws) = saved_window {
            let rc = RECT {
                left: ws.x,
                top: ws.y,
                right: ws.x + ws.w,
                bottom: ws.y + ws.h,
            };
            if ws.w > 200 && ws.h > 150 && !MonitorFromRect(&rc, MONITOR_DEFAULTTONULL).is_invalid()
            {
                let _ = SetWindowPos(hwnd, None, ws.x, ws.y, ws.w, ws.h, SWP_NOZORDER);
                show = if ws.maximized {
                    SW_SHOWMAXIMIZED
                } else {
                    SW_SHOWNORMAL
                };
            }
        }
        app.layout();
        let _ = ShowWindow(hwnd, show);

        // 시작 경로: 명령줄 > 세션
        if let Some(p) = opts.path {
            app.open_path(&p);
        } else {
            let (root, file) = {
                let s = app.session.borrow();
                (s.root.clone(), s.open_file.clone())
            };
            if let Some(root) = root.filter(|r| r.is_dir()) {
                let comps = file
                    .as_deref()
                    .and_then(|f| f.strip_prefix(&root).ok())
                    .map(|rel| rel.iter().map(|c| c.to_os_string()).collect())
                    .unwrap_or_default();
                app.set_root(&root, comps);
            }
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if (msg.message == WM_KEYDOWN || msg.message == WM_SYSKEYDOWN)
                && app.accelerator(msg.wParam.0 as u16, msg.message == WM_SYSKEYDOWN)
            {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // 종료 시 App은 해제하지 않는다: 끊긴 네트워크 드라이브를 읽는 워커를 join하다 멈추지 않도록
        // 프로세스 종료에 맡긴다 (FR-F08).
        Ok(())
    }
}

fn ctrl_down() -> bool {
    unsafe { GetKeyState(VK_CONTROL.0 as i32) < 0 }
}

fn shift_down() -> bool {
    unsafe { GetKeyState(VK_SHIFT.0 as i32) < 0 }
}

fn set_clipboard(hwnd: HWND, s: &str) -> Result<()> {
    let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        OpenClipboard(Some(hwnd))?;
        let r = (|| {
            EmptyClipboard()?;
            let h = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2)?;
            let p = GlobalLock(h) as *mut u16;
            if p.is_null() {
                return Err(windows::core::Error::from_thread());
            }
            std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
            let _ = GlobalUnlock(h);
            SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(h.0)))?;
            Ok(())
        })();
        let _ = CloseClipboard();
        r
    }
}

impl App {
    fn dpi(&self) -> u32 {
        unsafe { GetDpiForWindow(self.hwnd).max(96) }
    }

    fn apply_font(&self, dpi: u32) {
        unsafe {
            let mut ncm = NONCLIENTMETRICSW {
                cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
                ..Default::default()
            };
            if SystemParametersInfoForDpi(
                SPI_GETNONCLIENTMETRICS.0,
                ncm.cbSize,
                Some(&mut ncm as *mut _ as *mut c_void),
                0,
                dpi,
            )
            .is_ok()
            {
                let font = CreateFontIndirectW(&ncm.lfMessageFont);
                for h in [self.tabs, self.tree, self.outline, self.status] {
                    SendMessageW(
                        h,
                        WM_SETFONT,
                        Some(WPARAM(font.0 as usize)),
                        Some(LPARAM(1)),
                    );
                }
                self.bar.set_font(font);
                let old = self.font.replace(font);
                if !old.is_invalid() {
                    let _ = DeleteObject(old.into());
                }
            }
        }
    }

    fn layout(&self) {
        unsafe {
            let dpi = self.dpi();
            SendMessageW(self.status, WM_SIZE, None, None);
            let mut rc = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rc);
            let mut src = RECT::default();
            let _ = GetWindowRect(self.status, &mut src);
            let status_h = src.bottom - src.top;
            let h = (rc.bottom - status_h).max(0);
            let side = scale(SIDEBAR_DIP, dpi).min(rc.right / 2);
            let tabs_h = scale(TABS_DIP, dpi);
            let _ = MoveWindow(self.tabs, 0, 0, side, tabs_h, true);
            for t in [self.tree, self.outline] {
                let _ = MoveWindow(t, 0, tabs_h, side, (h - tabs_h).max(0), true);
            }
            let doc_w = (rc.right - side).max(0);
            let bar_h = if self.bar_mode.get().is_some() {
                scale(BAR_HEIGHT_DIP, dpi)
            } else {
                0
            };
            let _ = MoveWindow(self.bar.hwnd, side, 0, doc_w, bar_h, true);
            self.bar.layout(doc_w, dpi);
            let _ = MoveWindow(self.doc, side, bar_h, doc_w, (h - bar_h).max(0), true);
            let parts = [(rc.right - scale(460, dpi)).max(100), -1];
            SendMessageW(
                self.status,
                SB_SETPARTS,
                Some(WPARAM(parts.len())),
                Some(LPARAM(parts.as_ptr() as isize)),
            );
        }
    }

    fn set_status(&self, part: usize, text: &str) {
        let wtext = wide(text);
        unsafe {
            SendMessageW(
                self.status,
                SB_SETTEXTW,
                Some(WPARAM(part)),
                Some(LPARAM(wtext.as_ptr() as isize)),
            );
        }
    }

    fn invalidate_doc(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.doc), None, false);
        }
    }

    fn select_tab(&self, i: usize) {
        unsafe {
            SendMessageW(self.tabs, TCM_SETCURSEL, Some(WPARAM(i)), None);
            let _ = ShowWindow(self.tree, if i == 0 { SW_SHOW } else { SW_HIDE });
            let _ = ShowWindow(self.outline, if i == 1 { SW_SHOW } else { SW_HIDE });
        }
    }

    // ───── 키보드 가속기 ─────

    /// 포커스 위치와 상관없이 동작하는 단축키. 처리했으면 true. `alt`: WM_SYSKEYDOWN
    fn accelerator(&self, vk: u16, alt: bool) -> bool {
        let ctrl = ctrl_down();
        let vk = VIRTUAL_KEY(vk);
        if alt {
            // Alt+Z: 자동 줄바꿈 토글 (FR-T03)
            if vk.0 == b'Z' as u16
                && let Content::Text(tv) = &mut self.view.borrow_mut().content
            {
                tv.toggle_wrap();
            } else {
                return false;
            }
            self.scroll_cache.set(None);
            self.invalidate_doc();
            return true;
        }
        match vk {
            _ if ctrl && vk.0 == b'O' as u16 => unsafe {
                let _ = PostMessageW(
                    Some(self.hwnd),
                    WM_COMMAND,
                    WPARAM(ID_OPEN_FOLDER),
                    LPARAM(0),
                );
            },
            _ if ctrl && vk.0 == b'F' as u16 => self.show_bar(BarMode::Find),
            _ if ctrl && vk.0 == b'G' as u16 => self.show_bar(BarMode::Goto),
            VK_F3 => {
                if self.doc_query().is_some() {
                    self.find_step(!shift_down());
                } else {
                    self.show_bar(BarMode::Find);
                }
            }
            _ => return false,
        }
        true
    }

    // ───── 찾기 / 쪽·줄 이동 막대 ─────

    fn show_bar(&self, mode: BarMode) {
        if !matches!(
            self.view.borrow().content,
            Content::Pdf(_) | Content::Text(_)
        ) {
            return;
        }
        let was = self.bar_mode.replace(Some(mode));
        self.bar.set_mode(mode);
        if was != Some(mode) {
            let q = if mode == BarMode::Find {
                self.doc_query().unwrap_or_default()
            } else {
                String::new()
            };
            self.bar.set_text(&q);
            self.bar.set_info("");
        }
        unsafe {
            let _ = ShowWindow(self.bar.hwnd, SW_SHOW);
        }
        self.layout();
        self.bar.focus_select_all();
    }

    fn hide_bar(&self) {
        if self.bar_mode.take().is_none() {
            return;
        }
        unsafe {
            let _ = ShowWindow(self.bar.hwnd, SW_HIDE);
            let _ = KillTimer(Some(self.hwnd), TIMER_FIND);
        }
        self.layout();
        unsafe {
            let _ = SetFocus(Some(self.doc));
        }
    }

    /// 현재 문서의 찾기 검색어
    fn doc_query(&self) -> Option<String> {
        match &self.view.borrow().content {
            Content::Pdf(pv) => pv.search_query().map(str::to_owned),
            Content::Text(tv) => tv.search_query().map(str::to_owned),
            _ => None,
        }
    }

    fn on_bar_event(&self, ev: BarEvent) {
        match (self.bar_mode.get(), ev) {
            (_, BarEvent::Escape) => {
                let cmd = match &mut self.view.borrow_mut().content {
                    Content::Pdf(pv) => pv.clear_search(),
                    Content::Text(tv) => {
                        tv.clear_search();
                        None
                    }
                    _ => None,
                };
                self.run_cmds(self.current_pdf_gen(), cmd.into_iter().collect());
                self.hide_bar();
                self.invalidate_doc();
            }
            (Some(BarMode::Find), BarEvent::Changed) => unsafe {
                SetTimer(Some(self.hwnd), TIMER_FIND, 250, None);
            },
            (Some(BarMode::Find), BarEvent::Enter | BarEvent::ShiftEnter) => {
                let text = self.bar.text();
                if self.doc_query().as_deref() != Some(text.trim()) {
                    self.start_search();
                } else {
                    self.find_step(ev == BarEvent::Enter);
                }
            }
            (Some(BarMode::Goto), BarEvent::Enter) => {
                let n: u64 = self.bar.text().trim().parse().unwrap_or(0);
                let mut msg = None;
                match &mut self.view.borrow_mut().content {
                    Content::Pdf(pv) if n >= 1 => pv.go_page(n as usize - 1),
                    Content::Text(tv) if n >= 1 => {
                        if !tv.go_line(n) {
                            msg = Some("줄 색인이 끝나면 줄 이동을 할 수 있습니다");
                        }
                    }
                    _ => {}
                }
                if let Some(m) = msg {
                    self.bar.set_info(m);
                    return;
                }
                self.hide_bar();
                self.invalidate_doc();
            }
            _ => {}
        }
    }

    fn start_search(&self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_FIND);
        }
        let q = self.bar.text();
        let mut job = None;
        let cmd = match &mut self.view.borrow_mut().content {
            Content::Pdf(pv) => Some(pv.start_search(&q)),
            Content::Text(tv) => {
                job = tv.start_search(&q).map(|j| (tv.generation, j));
                None
            }
            _ => None,
        };
        self.run_cmds(self.current_pdf_gen(), cmd.into_iter().collect());
        if let Some((generation, job)) = job {
            self.spawn_text_search(generation, job);
        }
        self.invalidate_doc();
    }

    fn find_step(&self, forward: bool) {
        match &mut self.view.borrow_mut().content {
            Content::Pdf(pv) => pv.find_step(forward),
            Content::Text(tv) => tv.find_step(forward),
            _ => {}
        }
        self.invalidate_doc();
    }

    fn current_pdf_gen(&self) -> u64 {
        match &self.view.borrow().content {
            Content::Pdf(pv) => pv.generation,
            _ => 0,
        }
    }

    // ───── TXT 워커 작업 ─────

    fn spawn_text_search(&self, generation: u64, job: SearchJob) {
        let waker = self.waker.clone();
        self.workers.spawn(move || {
            let SearchJob {
                id,
                doc,
                query,
                cancel,
            } = job;
            let t = Instant::now();
            let out = leaf_fmt_text::search::search(
                &doc,
                &query,
                textview::MAX_HITS,
                &cancel,
                |hits, done_bytes| {
                    waker.send(UiMsg::TextHits {
                        generation,
                        id,
                        hits,
                        done_bytes,
                        finished: false,
                        capped: false,
                    })
                },
            );
            if !out.cancelled {
                perf::record(
                    "txt_search_ms",
                    format!(
                        "{:.1} hits={} bytes={}",
                        t.elapsed().as_secs_f64() * 1000.0,
                        out.hits,
                        doc.len()
                    ),
                );
                waker.send(UiMsg::TextHits {
                    generation,
                    id,
                    hits: Vec::new(),
                    done_bytes: doc.len(),
                    finished: true,
                    capped: out.capped,
                });
            }
        });
    }

    fn spawn_text_index(&self, generation: u64, doc: Arc<TextDoc>, cancel: Arc<AtomicBool>) {
        let waker = self.waker.clone();
        self.workers.spawn(move || {
            let t = Instant::now();
            let len = doc.len().max(1) as f32;
            let index = LineIndex::build(&doc, &cancel, |done| {
                waker.send(UiMsg::TextIndexProgress {
                    generation,
                    frac: done as f32 / len,
                })
            });
            if let Some(index) = index {
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                waker.send(UiMsg::TextIndexed {
                    generation,
                    index,
                    ms,
                });
            }
        });
    }

    fn run_copy(&self, job: CopyJob) {
        match job {
            CopyJob::Ready(s) => self.copy_to_clipboard(&s),
            CopyJob::TooLarge(n) => self.set_status(
                1,
                &format!(
                    "선택이 너무 큽니다({}). 복사 상한은 {}입니다",
                    human_size(n as u64),
                    human_size(textview::COPY_MAX_BYTES as u64)
                ),
            ),
            CopyJob::Background { doc, a, b } => {
                self.set_status(1, "복사하는 중…");
                let generation = self.doc_gen.get();
                let waker = self.waker.clone();
                self.workers.spawn(move || {
                    let result = doc
                        .extract(a, b, textview::COPY_MAX_BYTES)
                        .map_err(|n| format!("선택이 너무 큽니다({})", human_size(n as u64)));
                    waker.send(UiMsg::TextCopied { generation, result });
                });
            }
        }
    }

    /// PDF 보기가 돌려준 명령 처리 (빌림 없이 호출할 것).
    fn run_cmds(&self, generation: u64, cmds: Vec<Cmd>) {
        for cmd in cmds {
            match cmd {
                Cmd::Render(tiles) => self.pdf.borrow_mut().render(generation, tiles),
                Cmd::PageInfo(pages) => self.pdf.borrow_mut().page_info(generation, pages),
                Cmd::Search { id, query, start } => {
                    self.pdf.borrow_mut().search(generation, id, &query, start)
                }
                Cmd::CancelSearch => self.pdf.borrow_mut().cancel_search(),
                Cmd::Copy { from, to } => {
                    self.set_status(1, "복사하는 중…");
                    self.pdf.borrow_mut().copy(generation, from, to);
                }
                Cmd::SetClipboard(s) => self.copy_to_clipboard(&s),
                Cmd::OpenUri(u) => {
                    // SPEC §6.3: http/https/mailto만, 사용자가 클릭했을 때만
                    if leaf_doc::is_allowed_uri(&u) {
                        let wu = wide(&u);
                        unsafe {
                            ShellExecuteW(
                                Some(self.hwnd),
                                w!("open"),
                                PCWSTR(wu.as_ptr()),
                                None,
                                None,
                                SW_SHOWNORMAL,
                            );
                        }
                    } else {
                        self.set_status(1, &format!("열지 않은 링크: {u}"));
                    }
                }
            }
        }
    }

    fn copy_to_clipboard(&self, s: &str) {
        match set_clipboard(self.hwnd, s) {
            Ok(()) => self.set_status(1, &format!("{}자 복사함", s.chars().count())),
            Err(e) => self.set_status(1, &format!("복사 실패: {e}")),
        }
    }

    // ───── 목차 (FR-P07) ─────

    fn fill_outline(&self, items: &[leaf_doc::OutlineItem]) {
        let flat = pdfview::flatten_outline(items);
        *self.outline_dests.borrow_mut() = flat.iter().map(|(_, _, d)| *d).collect();
        unsafe {
            SendMessageW(self.outline, WM_SETREDRAW, Some(WPARAM(0)), None);
            SendMessageW(self.outline, TVM_DELETEITEM, None, Some(LPARAM(TVI_ROOT.0)));
        }
        if flat.is_empty() {
            self.insert_plain(self.outline, TVI_ROOT, "(목차 없음)", NO_NODE);
        }
        let mut stack: Vec<HTREEITEM> = Vec::new();
        for (i, (depth, title, _)) in flat.iter().enumerate() {
            stack.truncate(*depth);
            let parent = stack.last().copied().unwrap_or(TVI_ROOT);
            let title = if title.is_empty() {
                "(제목 없음)"
            } else {
                title.as_str()
            };
            let item = self.insert_plain(self.outline, parent, title, i as isize);
            stack.push(item);
        }
        unsafe {
            SendMessageW(self.outline, WM_SETREDRAW, Some(WPARAM(1)), None);
            let _ = InvalidateRect(Some(self.outline), None, true);
        }
    }

    fn insert_plain(&self, tree: HWND, parent: HTREEITEM, text: &str, lparam: isize) -> HTREEITEM {
        let mut wtext = wide(text);
        let tvis = TVINSERTSTRUCTW {
            hParent: parent,
            hInsertAfter: TVI_LAST,
            Anonymous: TVINSERTSTRUCTW_0 {
                item: TVITEMW {
                    mask: TVIF_TEXT | TVIF_PARAM,
                    pszText: PWSTR(wtext.as_mut_ptr()),
                    lParam: LPARAM(lparam),
                    ..Default::default()
                },
            },
        };
        let r = unsafe {
            SendMessageW(
                tree,
                TVM_INSERTITEMW,
                None,
                Some(LPARAM(&tvis as *const _ as isize)),
            )
        };
        HTREEITEM(r.0)
    }

    fn on_outline_notify(&self, lparam: LPARAM) -> LRESULT {
        let hdr = unsafe { &*(lparam.0 as *const NMHDR) };
        if hdr.code == TVN_SELCHANGEDW {
            let nm = unsafe { &*(lparam.0 as *const NMTREEVIEWW) };
            // 프로그램이 바꾼 선택(TVC_UNKNOWN)은 무시
            if nm.action != TVC_UNKNOWN && nm.itemNew.lParam.0 >= 0 {
                let dest = self
                    .outline_dests
                    .borrow()
                    .get(nm.itemNew.lParam.0 as usize)
                    .copied()
                    .flatten();
                if let Some(d) = dest
                    && let Content::Pdf(pv) = &mut self.view.borrow_mut().content
                {
                    pv.go_to(d);
                }
                self.invalidate_doc();
            }
        }
        LRESULT(0)
    }

    // ───── 폴더 / 트리 ─────

    /// FR-A01: 폴더면 루트로, 파일이면 상위 폴더를 루트로 열고 그 파일을 선택.
    fn open_path(&self, p: &Path) {
        let p = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        if p.is_dir() {
            self.set_root(&p, Vec::new());
        } else if let (Some(parent), Some(name)) = (p.parent(), p.file_name()) {
            self.set_root(parent, vec![name.to_os_string()]);
        } else {
            self.set_status(0, &format!("열 수 없는 경로: {}", p.display()));
        }
    }

    /// `select`: 루트 아래 상대 경로 조각들. 폴더를 차례로 펼쳐 그 파일을 선택한다.
    fn set_root(&self, root: &Path, select: Vec<OsString>) {
        unsafe {
            SendMessageW(self.tree, TVM_DELETEITEM, None, Some(LPARAM(TVI_ROOT.0)));
        }
        let display = root
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| root.as_os_str().to_os_string());
        {
            let mut m = self.model.borrow_mut();
            m.generation += 1;
            m.nodes.clear();
            m.nodes.push(Node {
                path: root.to_path_buf(),
                name: wide(&display),
                is_dir: true,
                state: LoadState::NotLoaded,
                item: 0,
                placeholder: 0,
            });
        }
        *self.pending_select.borrow_mut() = (!select.is_empty()).then_some((0, select));
        let item = self.insert_item(TVI_ROOT, TVI_LAST, 0, true);
        self.model.borrow_mut().nodes[0].item = item.0;
        unsafe {
            SendMessageW(
                self.tree,
                TVM_EXPAND,
                Some(WPARAM(TVE_EXPAND.0 as usize)),
                Some(LPARAM(item.0)),
            );
            let title = wide(format!("{} — LeafViewer", root.display()));
            let _ = SetWindowTextW(self.hwnd, PCWSTR(title.as_ptr()));
        }
        self.set_status(0, &root.display().to_string());
    }

    /// `after`: 직전에 넣은 형제 항목 (TVI_LAST는 형제 목록을 끝까지 훑는다).
    fn insert_item(
        &self,
        parent: HTREEITEM,
        after: HTREEITEM,
        idx: usize,
        is_dir: bool,
    ) -> HTREEITEM {
        let tvis = TVINSERTSTRUCTW {
            hParent: parent,
            hInsertAfter: after,
            Anonymous: TVINSERTSTRUCTW_0 {
                item: TVITEMW {
                    mask: TVIF_TEXT | TVIF_CHILDREN | TVIF_PARAM,
                    pszText: LPSTR_TEXTCALLBACKW,
                    cChildren: if is_dir {
                        I_CHILDRENCALLBACK
                    } else {
                        TVITEMEXW_CHILDREN(0)
                    },
                    lParam: LPARAM(idx as isize),
                    ..Default::default()
                },
            },
        };
        let r = unsafe {
            SendMessageW(
                self.tree,
                TVM_INSERTITEMW,
                None,
                Some(LPARAM(&tvis as *const _ as isize)),
            )
        };
        HTREEITEM(r.0)
    }

    /// TVN_ITEMEXPANDING: 펼칠 때 그 폴더만 워커에서 읽는다 (FR-F02, FR-F08).
    fn on_expanding(&self, idx: usize, item: HTREEITEM) {
        let (path, generation) = {
            let mut m = self.model.borrow_mut();
            let generation = m.generation;
            let Some(n) = m.nodes.get_mut(idx) else {
                return;
            };
            if !n.is_dir || n.state != LoadState::NotLoaded {
                return;
            }
            n.state = LoadState::Loading;
            (n.path.clone(), generation)
        };
        let ph = self.insert_plain(self.tree, item, "불러오는 중…", NO_NODE);
        if let Some(n) = self.model.borrow_mut().nodes.get_mut(idx) {
            n.placeholder = ph.0;
        }
        let waker = self.waker.clone();
        let started = Instant::now();
        self.workers.spawn(move || {
            let result =
                fs_tree::list_dir(&path, ListOptions::default()).map_err(|e| e.to_string());
            if let Ok(v) = &result {
                perf::record(
                    "list_dir_ms",
                    format!(
                        "{:.2} entries={}",
                        started.elapsed().as_secs_f64() * 1000.0,
                        v.len()
                    ),
                );
            }
            waker.send(UiMsg::DirListed {
                generation,
                node: idx,
                result,
            });
        });
    }

    fn on_dir_listed(
        &self,
        generation: u64,
        idx: usize,
        result: std::result::Result<Vec<Entry>, String>,
    ) {
        let t = Instant::now();
        let (parent_item, placeholder, range, err) = {
            let mut m = self.model.borrow_mut();
            if m.generation != generation
                || m.nodes
                    .get(idx)
                    .is_none_or(|n| n.state != LoadState::Loading)
            {
                return;
            }
            let parent_path = m.nodes[idx].path.clone();
            let start = m.nodes.len();
            let err = match result {
                Ok(entries) => {
                    for e in entries {
                        m.nodes.push(Node {
                            path: parent_path.join(&e.name),
                            name: wide(&e.name),
                            is_dir: e.is_dir,
                            state: LoadState::NotLoaded,
                            item: 0,
                            placeholder: 0,
                        });
                    }
                    None
                }
                Err(e) => Some(e),
            };
            let end = m.nodes.len();
            let n = &mut m.nodes[idx];
            n.state = if err.is_some() {
                LoadState::Failed
            } else {
                LoadState::Loaded {
                    has_children: end > start,
                }
            };
            (
                HTREEITEM(n.item),
                HTREEITEM(std::mem::take(&mut n.placeholder)),
                start..end,
                err,
            )
        };

        unsafe {
            SendMessageW(self.tree, WM_SETREDRAW, Some(WPARAM(0)), None);
            if placeholder.0 != 0 {
                SendMessageW(self.tree, TVM_DELETEITEM, None, Some(LPARAM(placeholder.0)));
            }
        }
        let count = range.len();
        if let Some(e) = err {
            self.insert_plain(
                self.tree,
                parent_item,
                &format!("⚠ 읽을 수 없음: {e}"),
                NO_NODE,
            );
        } else {
            let mut after = TVI_FIRST;
            let items: Vec<(usize, HTREEITEM)> = range
                .map(|i| {
                    let is_dir = self.model.borrow().nodes[i].is_dir;
                    after = self.insert_item(parent_item, after, i, is_dir);
                    (i, after)
                })
                .collect();
            let mut m = self.model.borrow_mut();
            for (i, it) in items {
                m.nodes[i].item = it.0;
            }
        }
        if count == 0 {
            // 빈 폴더: 펼침 버튼 제거
            let item = TVITEMW {
                mask: TVIF_CHILDREN,
                hItem: parent_item,
                cChildren: TVITEMEXW_CHILDREN(0),
                ..Default::default()
            };
            unsafe {
                SendMessageW(
                    self.tree,
                    TVM_SETITEMW,
                    None,
                    Some(LPARAM(&item as *const _ as isize)),
                );
            }
        }
        unsafe {
            SendMessageW(self.tree, WM_SETREDRAW, Some(WPARAM(1)), None);
            let _ = InvalidateRect(Some(self.tree), None, true);
        }
        perf::record(
            "tree_insert_ms",
            format!("{:.2} entries={count}", t.elapsed().as_secs_f64() * 1000.0),
        );
        self.continue_pending_select(idx);
    }

    /// 명령줄·세션으로 받은 파일까지 폴더를 차례로 펼친다.
    fn continue_pending_select(&self, listed: usize) {
        let Some((want, comps)) = self.pending_select.borrow_mut().take() else {
            return;
        };
        if want != listed {
            *self.pending_select.borrow_mut() = Some((want, comps));
            return;
        }
        let found = {
            let m = self.model.borrow();
            let parent = &m.nodes[listed].path;
            m.nodes
                .iter()
                .enumerate()
                .find(|(_, n)| {
                    n.path.parent() == Some(parent.as_path())
                        && n.path.file_name() == Some(comps[0].as_os_str())
                })
                .map(|(i, n)| (i, n.item))
        };
        let Some((child, item)) = found else { return };
        unsafe {
            if comps.len() == 1 {
                SendMessageW(
                    self.tree,
                    TVM_SELECTITEM,
                    Some(WPARAM(TVGN_CARET as usize)),
                    Some(LPARAM(item)),
                );
            } else {
                *self.pending_select.borrow_mut() = Some((child, comps[1..].to_vec()));
                SendMessageW(
                    self.tree,
                    TVM_EXPAND,
                    Some(WPARAM(TVE_EXPAND.0 as usize)),
                    Some(LPARAM(item)),
                );
            }
        }
    }

    fn on_tree_notify(&self, lparam: LPARAM) -> LRESULT {
        let hdr = unsafe { &*(lparam.0 as *const NMHDR) };
        match hdr.code {
            TVN_GETDISPINFOW => {
                let di = unsafe { &mut *(lparam.0 as *mut NMTVDISPINFOW) };
                let idx = di.item.lParam.0;
                if idx < 0 {
                    return LRESULT(0);
                }
                let Ok(m) = self.model.try_borrow() else {
                    return LRESULT(0);
                };
                let Some(n) = m.get(idx as usize) else {
                    return LRESULT(0);
                };
                if di.item.mask.contains(TVIF_TEXT) {
                    di.item.pszText = PWSTR(n.name.as_ptr() as *mut u16);
                }
                if di.item.mask.contains(TVIF_CHILDREN) {
                    di.item.cChildren = TVITEMEXW_CHILDREN(m.has_children(idx as usize) as i32);
                }
            }
            TVN_ITEMEXPANDINGW => {
                let nm = unsafe { &*(lparam.0 as *const NMTREEVIEWW) };
                if nm.action == TVE_EXPAND && nm.itemNew.lParam.0 >= 0 {
                    self.on_expanding(nm.itemNew.lParam.0 as usize, nm.itemNew.hItem);
                }
            }
            TVN_SELCHANGEDW => {
                let nm = unsafe { &*(lparam.0 as *const NMTREEVIEWW) };
                let idx = nm.itemNew.lParam.0;
                if idx >= 0 {
                    // FR-F06: 100ms 디바운스 — 빠르게 넘기면 중간 파일은 열지 않는다
                    self.pending_open.set(Some(idx as usize));
                    unsafe { SetTimer(Some(self.hwnd), TIMER_SELECT, 100, None) };
                }
            }
            _ => {}
        }
        LRESULT(0)
    }

    fn pick_folder(&self) {
        let result: Result<Option<PathBuf>> = unsafe {
            (|| {
                let dlg: IFileOpenDialog =
                    CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
                dlg.SetOptions(dlg.GetOptions()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)?;
                if dlg.Show(Some(self.hwnd)).is_err() {
                    return Ok(None); // 취소
                }
                let item = dlg.GetResult()?;
                let p = item.GetDisplayName(SIGDN_FILESYSPATH)?;
                let s = p.to_string();
                CoTaskMemFree(Some(p.0 as *const c_void));
                Ok(s.ok().map(PathBuf::from))
            })()
        };
        match result {
            Ok(Some(p)) => self.set_root(&p, Vec::new()),
            Ok(None) => {}
            Err(e) => self.set_status(0, &format!("폴더 선택 실패: {e}")),
        }
    }

    // ───── 세션 (FR-A02, FR-V09) ─────

    fn remember_current(&self) {
        let pos = match &self.view.borrow().content {
            Content::Pdf(pv) => Some(pv.position()),
            Content::Text(tv) => Some(tv.position()),
            _ => None,
        };
        if let Some(pos) = pos {
            self.session.borrow_mut().remember(pos);
        }
    }

    fn save_session(&self) {
        self.remember_current();
        let mut s = self.session.borrow_mut();
        s.root = self.model.borrow().nodes.first().map(|n| n.path.clone());
        s.open_file = {
            let v = self.view.borrow();
            match &v.content {
                Content::Pdf(pv) => Some(pv.path.clone()),
                Content::Text(tv) => Some(tv.path.clone()),
                _ => v
                    .loading
                    .as_ref()
                    .map(|(_, p, _)| p.clone())
                    .or_else(|| s.open_file.clone()),
            }
        };
        let mut wp = WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        if unsafe { GetWindowPlacement(self.hwnd, &mut wp) }.is_ok() {
            let r = wp.rcNormalPosition;
            s.window = Some(WindowState {
                x: r.left,
                y: r.top,
                w: r.right - r.left,
                h: r.bottom - r.top,
                maximized: wp.showCmd == SW_SHOWMAXIMIZED.0 as u32,
            });
        }
        if let Some(p) = &self.session_path
            && let Err(e) = s.save(p)
        {
            perf::record("session_save_error", e);
        }
    }

    // ───── 문서 열기 ─────

    fn open_node(&self, idx: usize) {
        let path = {
            let m = self.model.borrow();
            match m.get(idx) {
                Some(n) if !n.is_dir => n.path.clone(),
                _ => return,
            }
        };
        self.open_file(path);
    }

    fn open_file(&self, path: PathBuf) {
        self.remember_current();
        let generation = self.doc_gen.get() + 1;
        self.doc_gen.set(generation);
        let ext = path
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        self.view.borrow_mut().loading = Some((generation, path.clone(), Instant::now()));
        self.set_status(0, &format!("여는 중… {}", path.display()));

        match ext.as_str() {
            "pdf" => self.pdf.borrow_mut().open(generation, &path),
            // MD는 M3 전까지 원문을 TXT 보기로 보여 준다
            "txt" | "md" | "markdown" => {
                let waker = self.waker.clone();
                self.workers.spawn(move || {
                    let t = Instant::now();
                    let result = textload::load(&path).map(|l| {
                        // 작은 파일은 줄 색인까지 바로 (16MB 미만이라 수 ms)
                        let index = (!l.mapped).then(|| {
                            LineIndex::build(&l.doc, &AtomicBool::new(false), |_| {})
                                .unwrap_or_default()
                        });
                        LoadedText {
                            doc: l.doc,
                            size: l.size,
                            mapped: l.mapped,
                            index,
                            load_ms: t.elapsed().as_secs_f64() * 1000.0,
                        }
                    });
                    waker.send(UiMsg::TextLoaded { generation, result });
                });
            }
            _ => {
                self.set_content(Content::Message(format!(
                    "지원하지 않는 형식입니다: {}",
                    path.display()
                )));
                self.view.borrow_mut().loading = None;
            }
        }
    }

    /// 문서 내용 교체. PDF를 떠나면 엔진의 문서를 닫고 목차를 정리하며, 문서가 바뀌면 찾기 막대를 닫는다.
    fn set_content(&self, content: Content) {
        let (leaving_pdf, old) = {
            let mut v = self.view.borrow_mut();
            let was_pdf = matches!(v.content, Content::Pdf(_));
            let is_pdf = matches!(content, Content::Pdf(_));
            let old = std::mem::replace(&mut v.content, content);
            (was_pdf && !is_pdf, old)
        };
        let leaving_doc = matches!(old, Content::Pdf(_) | Content::Text(_));
        if let Content::Text(tv) = &old {
            tv.cancel_all();
        }
        // 매핑 해제 등 무거울 수 있는 정리는 빌림 밖에서
        drop(old);
        if leaving_pdf {
            self.pdf.borrow_mut().close();
            self.fill_outline(&[]);
        }
        if leaving_doc {
            self.hide_bar();
        }
        self.scroll_cache.set(None);
        self.invalidate_doc();
    }

    fn drain_messages(&self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                UiMsg::DirListed {
                    generation,
                    node,
                    result,
                } => self.on_dir_listed(generation, node, result),
                UiMsg::TextLoaded { generation, result } => self.on_text_loaded(generation, result),
                UiMsg::TextIndexProgress { generation, frac } => {
                    if let Content::Text(tv) = &mut self.view.borrow_mut().content
                        && tv.generation == generation
                    {
                        tv.set_index_progress(frac);
                    }
                    self.invalidate_doc();
                }
                UiMsg::TextIndexed {
                    generation,
                    index,
                    ms,
                } => {
                    if let Content::Text(tv) = &mut self.view.borrow_mut().content
                        && tv.generation == generation
                    {
                        perf::record(
                            "txt_index_ms",
                            format!("{ms:.1} lines={} bytes={}", index.lines, tv.size),
                        );
                        tv.set_index(index);
                    }
                    self.scroll_cache.set(None);
                    self.invalidate_doc();
                }
                UiMsg::TextHits {
                    generation,
                    id,
                    hits,
                    done_bytes,
                    finished,
                    capped,
                } => {
                    if let Content::Text(tv) = &mut self.view.borrow_mut().content
                        && tv.generation == generation
                    {
                        tv.on_search_hits(id, hits, done_bytes, finished, capped);
                    }
                    self.invalidate_doc();
                }
                UiMsg::TextCopied { generation, result } => {
                    if generation == self.doc_gen.get() {
                        match result {
                            Ok(s) => self.copy_to_clipboard(&s),
                            Err(e) => self.set_status(1, &e),
                        }
                    }
                }
                UiMsg::Pdf(ev) => self.on_pdf_event(ev),
            }
        }
    }

    fn take_loading(&self, generation: u64) -> Option<(PathBuf, Instant)> {
        let mut v = self.view.borrow_mut();
        match v.loading.take() {
            Some((g, p, t)) if g == generation => Some((p, t)),
            other => {
                v.loading = other;
                None
            }
        }
    }

    fn on_text_loaded(&self, generation: u64, result: std::result::Result<LoadedText, String>) {
        let Some((path, started)) = self.take_loading(generation) else {
            return;
        };
        let t = match result {
            Ok(t) => t,
            Err(e) => {
                self.set_content(Content::Message(format!(
                    "파일을 열 수 없습니다.\n\n{}\n\n{e}",
                    path.display()
                )));
                self.view.borrow_mut().info.clear();
                self.set_status(0, &path.display().to_string());
                return;
            }
        };
        perf::record(
            "txt_load_ms",
            format!(
                "{:.2} bytes={} mapped={} enc={}",
                t.load_ms, t.size, t.mapped, t.doc.enc.label
            ),
        );
        let info = format!(
            "{} · {} · {}",
            path.file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default(),
            t.doc.enc.label,
            human_size(t.size),
        );
        let need_index = t.index.is_none();
        let doc = t.doc.clone();
        let dwrite = self.view.borrow().renderer.dwrite.clone();
        let mut tv = TextView::new(
            generation,
            path.clone(),
            t.doc,
            t.size,
            t.index,
            dwrite,
            started,
        );
        if let Some(pos) = self.session.borrow().position_of(&path) {
            tv.restore(pos);
        }
        let cancel = tv.cancel.clone();
        self.hide_bar();
        self.set_content(Content::Text(Box::new(tv)));
        *self.perf_text.borrow_mut() = format!("로드 {:.0}ms", t.load_ms);
        self.view.borrow_mut().info = info.clone();
        self.set_status(0, &info);
        self.session.borrow_mut().open_file = Some(path);
        if need_index {
            self.spawn_text_index(generation, doc, cancel);
        }
    }

    fn on_pdf_event(&self, ev: PdfEvent) {
        match ev {
            PdfEvent::Opened {
                generation,
                page_sizes,
                outline,
                bind_ms,
                load_ms,
            } => {
                let Some((path, started)) = self.take_loading(generation) else {
                    return;
                };
                if bind_ms > 0.0 {
                    perf::record("pdfium_bind_ms", format!("{bind_ms:.2}"));
                }
                perf::record(
                    "pdf_load_ms",
                    format!("{load_ms:.2} pages={}", page_sizes.len()),
                );
                let timings = if bind_ms > 0.0 {
                    format!("DLL {bind_ms:.0} + 로드 {load_ms:.0}")
                } else {
                    format!("로드 {load_ms:.0}")
                };
                let mut pv = PdfView::new(
                    generation,
                    path.clone(),
                    page_sizes,
                    outline,
                    started,
                    timings,
                );
                {
                    let v = self.view.borrow();
                    let (vw, vh) = view_size(&v.renderer, self.doc);
                    pv.set_view(vw, vh, v.renderer.dpi());
                }
                if let Some(pos) = self.session.borrow().position_of(&path) {
                    pv.restore(pos);
                }
                let outline_items = pv.outline.clone();
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let info = format!(
                    "{} · PDF · {}쪽 · {}",
                    path.file_name()
                        .map(|n| n.to_string_lossy())
                        .unwrap_or_default(),
                    pv.page_count(),
                    human_size(size)
                );
                // 이전 문서의 찾기 막대는 닫는다
                self.hide_bar();
                self.set_content(Content::Pdf(Box::new(pv)));
                self.view.borrow_mut().info = info.clone();
                self.set_status(0, &info);
                self.fill_outline(&outline_items);
                self.session.borrow_mut().open_file = Some(path);
            }
            PdfEvent::Tile {
                generation,
                spec,
                stride,
                bgra,
                render_ms,
            } => {
                let mut v = self.view.borrow_mut();
                let dv = &mut *v;
                let Content::Pdf(pv) = &mut dv.content else {
                    return;
                };
                if pv.generation != generation {
                    return;
                }
                let Ok(t) = dv.renderer.target(self.doc) else {
                    return;
                };
                pv.check_epoch(t.epoch);
                if let Err(e) = pv.on_tile(t, spec, stride, &bgra, render_ms) {
                    drop(v);
                    self.set_status(1, &format!("비트맵 생성 실패: {e}"));
                }
                self.invalidate_doc();
            }
            PdfEvent::PageInfo {
                generation,
                page,
                text,
                links,
            } => {
                if let Content::Pdf(pv) = &mut self.view.borrow_mut().content
                    && pv.generation == generation
                {
                    pv.on_page_info(page, text, links);
                }
                self.invalidate_doc();
            }
            PdfEvent::SearchHits {
                generation,
                search_id,
                page,
                hits,
            } => {
                if let Content::Pdf(pv) = &mut self.view.borrow_mut().content
                    && pv.generation == generation
                {
                    pv.on_search_hits(search_id, page, hits);
                }
                self.invalidate_doc();
            }
            PdfEvent::SearchDone {
                generation,
                search_id,
            } => {
                if let Content::Pdf(pv) = &mut self.view.borrow_mut().content
                    && pv.generation == generation
                {
                    pv.on_search_done(search_id);
                }
                self.invalidate_doc();
            }
            PdfEvent::Copied { generation, text } => {
                if generation == self.current_pdf_gen() {
                    self.copy_to_clipboard(&text);
                }
            }
            PdfEvent::Error {
                generation,
                message,
                password: _,
            } => {
                if let Some((path, _)) = self.take_loading(generation) {
                    self.set_content(Content::Message(format!("{message}\n\n{}", path.display())));
                    self.view.borrow_mut().info.clear();
                    self.set_status(0, &path.display().to_string());
                } else if generation == self.current_pdf_gen() {
                    self.set_status(1, &message);
                }
            }
        }
    }

    // ───── 문서 영역 그리기 ─────

    fn paint_doc(&self) {
        let t0 = Instant::now();
        let outcome = {
            let mut v = self.view.borrow_mut();
            match paint(&mut v, self.doc) {
                Ok(o) => o,
                Err(e) => {
                    v.renderer.discard_target();
                    PaintOutcome {
                        status: Some(format!("그리기 오류: {e}")),
                        ..Default::default()
                    }
                }
            }
        };
        let paint_ms = t0.elapsed().as_secs_f64() * 1000.0;
        if let Some(b) = self.bench.borrow_mut().as_mut()
            && b.running
        {
            b.paints.push(paint_ms);
            if matches!(&self.view.borrow().content, Content::Pdf(pv) if !pv.last_frame_complete) {
                b.incomplete += 1;
            }
        }
        if !self.first_paint_logged.replace(true) {
            let ms = process_uptime_ms();
            perf::record("first_paint_ms", format!("{ms:.1}"));
            *self.perf_text.borrow_mut() = format!("첫 화면 {ms:.0}ms");
            self.set_status(1, &self.perf_text.borrow());
        }
        if let Some(ms) = outcome.text_first_paint_ms {
            perf::record("txt_first_paint_ms", format!("{ms:.2}"));
            *self.perf_text.borrow_mut() = format!("첫 화면 {ms:.0}ms");
        }
        self.run_cmds(outcome.generation, outcome.cmds);
        if let Some(s) = outcome.status {
            self.set_status(1, &s);
        }
        if self.bar_mode.get() == Some(BarMode::Find) {
            let info = match &self.view.borrow().content {
                Content::Pdf(pv) => pv.search_status().unwrap_or_default(),
                Content::Text(tv) => tv.search_status().unwrap_or_default(),
                _ => String::new(),
            };
            self.bar.set_info(&info);
        }
        self.update_scrollbars();
        self.maybe_start_bench();
    }

    fn scroll_metrics(&self) -> Option<ScrollMetrics> {
        match &mut self.view.borrow_mut().content {
            Content::Pdf(pv) => {
                let ((a, b, c), (d, e, f)) = pv.scroll_metrics();
                Some((
                    (a as f64, b as f64, c as f64),
                    (d as f64, e as f64, f as f64),
                ))
            }
            Content::Text(tv) => Some(tv.scroll_metrics()),
            _ => None,
        }
    }

    /// 표준 스크롤바 갱신. 막대가 생기거나 사라지면 WM_SIZE가 다시 오므로 빌림 없이 호출한다.
    fn update_scrollbars(&self) {
        let m = self.scroll_metrics();
        // 스크롤바 값은 i32: 큰 값(바이트 단위 등)은 비율을 줄여 넣는다
        let big = m.map_or(0.0, |((tw, ..), (th, ..))| tw.max(th));
        let k = if big > 1e9 { big / 1e9 } else { 1.0 };
        self.scroll_scale.set(k);
        let key = m.map(|((tw, vw, sx), (th, vh, sy))| {
            (
                (tw / k) as i32,
                (vw / k) as i32,
                (sx / k) as i32,
                (th / k) as i32,
                (vh / k) as i32,
                (sy / k) as i32,
            )
        });
        if key == self.scroll_cache.get() {
            return;
        }
        self.scroll_cache.set(key);
        let (h, v) = match key {
            Some((tw, vw, sx, th, vh, sy)) => ((tw, vw, sx), (th, vh, sy)),
            None => ((0, 0, 0), (0, 0, 0)),
        };
        unsafe {
            for (bar, (total, page, pos)) in [(SB_HORZ, h), (SB_VERT, v)] {
                let show = total > page;
                let si = SCROLLINFO {
                    cbSize: size_of::<SCROLLINFO>() as u32,
                    fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                    nMin: 0,
                    nMax: if show { total - 1 } else { 0 },
                    nPage: if show { page.max(1) as u32 } else { 0 },
                    nPos: pos,
                    nTrackPos: 0,
                };
                SetScrollInfo(self.doc, bar, &si, true);
            }
        }
    }

    fn on_scrollbar(&self, bar: SCROLLBAR_CONSTANTS, code: u32) {
        let mut si = SCROLLINFO {
            cbSize: size_of::<SCROLLINFO>() as u32,
            fMask: SIF_TRACKPOS,
            ..Default::default()
        };
        unsafe {
            let _ = GetScrollInfo(self.doc, bar, &mut si);
        }
        let track = si.nTrackPos as f64 * self.scroll_scale.get();
        let cmd = SCROLLBAR_COMMAND(code as i32);
        let is_text = matches!(self.view.borrow().content, Content::Text(_));
        if is_text {
            if let Content::Text(tv) = &mut self.view.borrow_mut().content {
                let (vw, vh) = tv.view_size();
                let vert = bar == SB_VERT;
                let step = if vert { 40.0 } else { 32.0 };
                let page = if vert { vh * 0.9 } else { vw * 0.9 };
                let d = match cmd {
                    SB_LINEUP => -step,
                    SB_LINEDOWN => step,
                    SB_PAGEUP => -page,
                    SB_PAGEDOWN => page,
                    SB_THUMBTRACK | SB_THUMBPOSITION => {
                        if vert {
                            tv.scroll_to_unit(track);
                        } else {
                            tv.set_sx(track as f32);
                        }
                        0.0
                    }
                    SB_TOP if vert => {
                        tv.home();
                        0.0
                    }
                    SB_BOTTOM if vert => {
                        tv.end();
                        0.0
                    }
                    _ => return,
                };
                if vert {
                    tv.scroll_px(d);
                } else {
                    tv.scroll_x(d);
                }
            }
            self.invalidate_doc();
            return;
        }
        let Some(((_, vw, sx), (_, vh, sy))) = self.scroll_metrics() else {
            return;
        };
        let (page, pos) = if bar == SB_VERT { (vh, sy) } else { (vw, sx) };
        let new = match cmd {
            SB_LINEUP => pos - 40.0,
            SB_LINEDOWN => pos + 40.0,
            SB_PAGEUP => pos - page * 0.9,
            SB_PAGEDOWN => pos + page * 0.9,
            SB_THUMBTRACK | SB_THUMBPOSITION => track,
            SB_TOP => 0.0,
            SB_BOTTOM => f32::MAX as f64,
            _ => return,
        } as f32;
        if let Content::Pdf(pv) = &mut self.view.borrow_mut().content {
            if bar == SB_VERT {
                pv.scroll_to(None, Some(new));
            } else {
                pv.scroll_to(Some(new), None);
            }
        }
        self.invalidate_doc();
    }

    fn to_dip(&self, lparam: LPARAM) -> (f32, f32) {
        let x = (lparam.0 & 0xFFFF) as u16 as i16 as f32;
        let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
        let k = 96.0 / self.dpi() as f32;
        (x * k, y * k)
    }

    /// `keys`: 휠 메시지의 키 상태 (MK_CONTROL 0x8, MK_SHIFT 0x4)
    fn on_doc_wheel(&self, delta: i16, keys: usize, horizontal: bool, screen_pt: LPARAM) {
        let steps = delta as f32 / 120.0;
        let (ctrl, shift) = (keys & 0x8 != 0, keys & 0x4 != 0);
        if ctrl && !horizontal {
            // 커서 위치 기준 확대/축소 (FR-V03)
            let mut pt = POINT {
                x: (screen_pt.0 & 0xFFFF) as u16 as i16 as i32,
                y: ((screen_pt.0 >> 16) & 0xFFFF) as u16 as i16 as i32,
            };
            unsafe {
                let _ = ScreenToClient(self.doc, &mut pt);
            }
            let k = 96.0 / self.dpi() as f32;
            match &mut self.view.borrow_mut().content {
                Content::Pdf(pv) => {
                    pv.zoom_by(1.1f32.powf(steps), Some((pt.x as f32 * k, pt.y as f32 * k)))
                }
                Content::Text(tv) => tv.zoom_by(1.1f32.powf(steps)),
                _ => {}
            }
            self.invalidate_doc();
            return;
        }
        // WM_MOUSEHWHEEL은 양수가 오른쪽, 세로 휠(Shift 포함)은 양수가 위/왼쪽
        let d = if horizontal {
            steps * WHEEL_STEP
        } else {
            -steps * WHEEL_STEP
        };
        let horizontal = horizontal || shift;
        {
            let mut v = self.view.borrow_mut();
            match &mut v.content {
                Content::Pdf(pv) if horizontal => pv.scroll_by(d, 0.0),
                Content::Pdf(pv) => pv.scroll_by(0.0, d),
                Content::Text(tv) if horizontal => tv.scroll_x(d),
                Content::Text(tv) => tv.scroll_px(d),
                _ => return,
            }
        }
        self.invalidate_doc();
    }

    fn on_doc_key(&self, vk: VIRTUAL_KEY) {
        let ctrl = ctrl_down();
        let mut cmds = Vec::new();
        let mut copy = None;
        {
            let mut v = self.view.borrow_mut();
            match &mut v.content {
                Content::Pdf(pv) => match vk {
                    _ if ctrl && vk.0 == b'C' as u16 => cmds.extend(pv.copy()),
                    _ if ctrl && vk.0 == b'A' as u16 => pv.select_all(),
                    VK_OEM_PLUS | VK_ADD if ctrl => pv.zoom_by(1.25, None),
                    VK_OEM_MINUS | VK_SUBTRACT if ctrl => pv.zoom_by(0.8, None),
                    _ if ctrl && (vk.0 == b'0' as u16 || vk == VK_NUMPAD0) => {
                        pv.set_zoom_mode(ZoomMode::FitWidth, None)
                    }
                    _ if ctrl && vk.0 == b'1' as u16 => {
                        pv.set_zoom_mode(ZoomMode::Custom(1.0), None)
                    }
                    _ if ctrl && vk.0 == b'2' as u16 => pv.set_zoom_mode(ZoomMode::FitPage, None),
                    _ => {
                        if !pv.on_key(vk.0, shift_down()) {
                            return;
                        }
                    }
                },
                Content::Text(tv) => match vk {
                    _ if ctrl && vk.0 == b'C' as u16 => copy = tv.copy(),
                    _ if ctrl && vk.0 == b'A' as u16 => tv.select_all(),
                    VK_OEM_PLUS | VK_ADD if ctrl => tv.zoom_by(1.1),
                    VK_OEM_MINUS | VK_SUBTRACT if ctrl => tv.zoom_by(1.0 / 1.1),
                    _ if ctrl && (vk.0 == b'0' as u16 || vk == VK_NUMPAD0) => tv.set_zoom(1.0),
                    VK_HOME if ctrl => tv.home(),
                    VK_END if ctrl => tv.end(),
                    _ => {
                        if !tv.on_key(vk.0, shift_down()) {
                            return;
                        }
                    }
                },
                _ => return,
            }
        }
        self.run_cmds(self.current_pdf_gen(), cmds);
        if let Some(job) = copy {
            self.run_copy(job);
        }
        self.invalidate_doc();
    }

    fn on_mouse(&self, msg: u32, wparam: WPARAM, lparam: LPARAM) {
        let (x, y) = self.to_dip(lparam);
        let mut cmds = Vec::new();
        {
            let mut v = self.view.borrow_mut();
            match &mut v.content {
                Content::Pdf(pv) => match msg {
                    WM_LBUTTONDOWN => pv.on_mouse_down(x, y, shift_down()),
                    WM_MOUSEMOVE => {
                        let dragging = wparam.0 & 0x0001 != 0; // MK_LBUTTON
                        let c = pv.on_mouse_move(x, y, dragging);
                        self.cursor.set(c);
                        if !dragging {
                            drop(v);
                            self.apply_cursor();
                            return;
                        }
                    }
                    WM_LBUTTONUP => cmds.extend(pv.on_mouse_up()),
                    _ => return,
                },
                Content::Text(tv) => match msg {
                    WM_LBUTTONDOWN => tv.on_mouse_down(x, y, shift_down()),
                    WM_MOUSEMOVE => {
                        let dragging = wparam.0 & 0x0001 != 0;
                        let c = tv.on_mouse_move(x, y, dragging);
                        self.cursor.set(c);
                        if !dragging {
                            drop(v);
                            self.apply_cursor();
                            return;
                        }
                    }
                    WM_LBUTTONUP => {}
                    _ => return,
                },
                _ => return,
            }
        }
        self.run_cmds(self.current_pdf_gen(), cmds);
        self.invalidate_doc();
    }

    fn apply_cursor(&self) {
        let id = match self.cursor.get() {
            CursorKind::Arrow => IDC_ARROW,
            CursorKind::IBeam => IDC_IBEAM,
            CursorKind::Hand => IDC_HAND,
        };
        unsafe {
            if let Ok(c) = LoadCursorW(None, id) {
                SetCursor(Some(c));
            }
        }
    }

    // ───── 스크롤 벤치마크 (NFR-P07, `--bench-scroll`) ─────

    fn maybe_start_bench(&self) {
        let ready = match &self.view.borrow().content {
            Content::Pdf(pv) => pv.first_page_shown(),
            Content::Text(tv) => tv.first_paint_ms.is_some(),
            _ => false,
        };
        let mut b = self.bench.borrow_mut();
        if let Some(b) = b.as_mut()
            && ready
            && !b.running
            && b.started.is_none()
        {
            b.running = true;
            b.started = Some(Instant::now());
            unsafe { SetTimer(Some(self.hwnd), TIMER_BENCH, 16, None) };
        }
    }

    fn bench_tick(&self) {
        let at_end = {
            let mut v = self.view.borrow_mut();
            match &mut v.content {
                Content::Pdf(pv) => {
                    pv.scroll_by(0.0, BENCH_STEP);
                    pv.at_end()
                }
                Content::Text(tv) => {
                    tv.scroll_px(BENCH_STEP);
                    tv.at_end()
                }
                _ => true,
            }
        };
        self.invalidate_doc();
        let done = {
            let mut b = self.bench.borrow_mut();
            let Some(b) = b.as_mut() else { return };
            b.ticks += 1;
            at_end || b.ticks >= BENCH_MAX_TICKS
        };
        if done {
            self.finish_bench();
        }
    }

    fn finish_bench(&self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_BENCH);
        }
        let (tiles, render_total, bitmap_bytes) = match &self.view.borrow().content {
            Content::Pdf(pv) => pv.tile_stats(),
            _ => (0, 0.0, 0),
        };
        let Some(mut b) = self.bench.borrow_mut().take() else {
            return;
        };
        b.paints.sort_by(f64::total_cmp);
        let n = b.paints.len().max(1);
        let pct = |p: f64| {
            b.paints
                .get(((n as f64 - 1.0) * p) as usize)
                .copied()
                .unwrap_or(0.0)
        };
        let over = b.paints.iter().filter(|&&m| m > 16.0).count();
        let secs = b.started.map(|s| s.elapsed().as_secs_f64()).unwrap_or(1.0);
        perf::record(
            "bench_scroll",
            format!(
                "frames={} incomplete={} p50={:.2} p95={:.2} max={:.2} over16={} fps={:.1} tiles={} avg_tile_ms={:.1} bitmap_mb={:.1}",
                b.paints.len(),
                b.incomplete,
                pct(0.5),
                pct(0.95),
                b.paints.last().copied().unwrap_or(0.0),
                over,
                b.paints.len() as f64 / secs,
                tiles,
                if tiles > 0 {
                    render_total / tiles as f64
                } else {
                    0.0
                },
                bitmap_bytes as f64 / 1048576.0
            ),
        );
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

/// 문서 영역 크기(DIP)
fn view_size(r: &Renderer, hwnd: HWND) -> (f32, f32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let k = 96.0 / r.dpi();
    (
        (rc.right - rc.left) as f32 * k,
        (rc.bottom - rc.top) as f32 * k,
    )
}

const WELCOME_TITLE: &str = "LeafViewer";
const WELCOME_BODY: &str = "폴더를 열려면 Ctrl+O를 누르거나 탐색기에서 폴더를 끌어다 놓으세요.\n\
트리에서 ↑↓로 이동하면 바로 미리 봅니다.\n\n\
공통: Ctrl+F 찾기 (Enter/F3 다음, Shift+Enter/Shift+F3 이전) · 끌어서 선택 후 Ctrl+C 복사 · Ctrl+A 전체 선택 · Ctrl+휠, Ctrl +/- 확대\n\
PDF: 휠·PgUp/PgDn·Space로 넘기기 · Ctrl+0 폭 맞춤 · Ctrl+1 실제 크기 · Ctrl+2 쪽 맞춤 · Ctrl+G 쪽 이동 · 사이드바 \"목차\" 탭\n\
TXT: Alt+Z 자동 줄바꿈 켜기/끄기 · Ctrl+G 줄 이동 · Ctrl+0 기본 배율 · Ctrl+Home/End 처음/끝";

fn paint(v: &mut DocView, hwnd: HWND) -> Result<PaintOutcome> {
    let mut out = PaintOutcome::default();
    let (vw, vh) = view_size(&v.renderer, hwnd);
    let dpi = v.renderer.dpi();

    let welcome = if matches!(v.content, Content::Welcome | Content::Message(_)) {
        let (title, body) = match &v.content {
            Content::Message(m) => (None, m.as_str()),
            _ => (Some(WELCOME_TITLE), WELCOME_BODY),
        };
        let w = (vw - 4.0 * MARGIN).max(50.0);
        let t = title
            .map(|t| {
                v.renderer
                    .layout(&t.encode_utf16().collect::<Vec<_>>(), &v.renderer.title, w)
            })
            .transpose()?;
        let b = v.renderer.layout(
            &body.encode_utf16().collect::<Vec<_>>(),
            &v.renderer.body,
            w,
        )?;
        Some((t, b))
    } else {
        None
    };

    let t = v.renderer.target(hwnd)?;
    unsafe {
        t.rt.BeginDraw();
        match &mut v.content {
            Content::Welcome | Content::Message(_) => {
                t.rt.Clear(Some(&rgb(render::COLOR_BG)));
                let (title, body) = welcome.unwrap();
                let mut y = 2.0 * MARGIN;
                if let Some((tl, th)) = title {
                    render::draw_layout(t, &tl, 2.0 * MARGIN, y, &t.text);
                    y += th + MARGIN;
                }
                let brush = if matches!(v.content, Content::Welcome) {
                    &t.dim
                } else {
                    &t.text
                };
                render::draw_layout(t, &body.0, 2.0 * MARGIN, y, brush);
            }
            Content::Text(tv) => {
                t.rt.Clear(Some(&rgb(render::COLOR_BG)));
                tv.set_view(vw, vh);
                let first = tv.first_paint_ms.is_none();
                let r = tv.paint(t);
                if first {
                    out.text_first_paint_ms = tv.first_paint_ms;
                }
                out.status = Some(match r {
                    Ok(()) => tv.status(),
                    Err(e) => format!("그리기 오류: {e}"),
                });
            }
            Content::Pdf(pv) => {
                pv.set_view(vw, vh, dpi);
                pv.check_epoch(t.epoch);
                out.cmds = pv.paint(t);
                out.generation = pv.generation;
                out.status = Some(format!("{} · {}", pv.status(), pv.timings));
            }
        }
    }
    v.renderer.end_draw()?;
    Ok(out)
}

// ───────────────────────── 창 프로시저 ─────────────────────────

extern "system" fn main_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app_of(hwnd) else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    unsafe {
        match msg {
            WM_SIZE => {
                app.layout();
                LRESULT(0)
            }
            WM_SETFOCUS => {
                let _ = SetFocus(Some(app.tree));
                LRESULT(0)
            }
            WM_NOTIFY => {
                let hdr = &*(lparam.0 as *const NMHDR);
                if hdr.hwndFrom == app.tree {
                    return app.on_tree_notify(lparam);
                }
                if hdr.hwndFrom == app.outline {
                    return app.on_outline_notify(lparam);
                }
                if hdr.hwndFrom == app.tabs && hdr.code == TCN_SELCHANGE {
                    let i = SendMessageW(app.tabs, TCM_GETCURSEL, None, None).0.max(0) as usize;
                    app.select_tab(i);
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_TIMER => {
                match wparam.0 {
                    TIMER_SELECT => {
                        let _ = KillTimer(Some(hwnd), TIMER_SELECT);
                        if let Some(idx) = app.pending_open.take() {
                            app.open_node(idx);
                        }
                    }
                    TIMER_FIND => app.start_search(),
                    TIMER_BENCH => app.bench_tick(),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_APP_WAKE => {
                app.drain_messages();
                LRESULT(0)
            }
            WM_APP_BAR => {
                let ev = match wparam.0 {
                    1 => BarEvent::Changed,
                    2 => BarEvent::Enter,
                    3 => BarEvent::ShiftEnter,
                    _ => BarEvent::Escape,
                };
                app.on_bar_event(ev);
                LRESULT(0)
            }
            WM_COMMAND if loword(wparam.0) as usize == ID_OPEN_FOLDER => {
                app.pick_folder();
                LRESULT(0)
            }
            WM_DROPFILES => {
                let hdrop = HDROP(wparam.0 as *mut c_void);
                let len = DragQueryFileW(hdrop, 0, None);
                let mut buf = vec![0u16; len as usize + 1];
                DragQueryFileW(hdrop, 0, Some(&mut buf));
                DragFinish(hdrop);
                buf.truncate(len as usize);
                let path = PathBuf::from(String::from_utf16_lossy(&buf));
                app.open_path(&path);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let dpi = hiword(wparam.0);
                app.apply_font(dpi);
                app.view.borrow_mut().renderer.set_dpi(dpi);
                let rc = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    rc.left,
                    rc.top,
                    rc.right - rc.left,
                    rc.bottom - rc.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                app.invalidate_doc();
                LRESULT(0)
            }
            WM_CLOSE => {
                app.save_session();
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

extern "system" fn doc_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app_of(hwnd) else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                app.paint_doc();
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_SIZE => {
                let (w, h) = (loword(lparam.0 as usize), hiword(lparam.0 as usize));
                let mut v = app.view.borrow_mut();
                if w == 0 || h == 0 {
                    // 최소화: 디바이스 리소스와 비트맵 캐시를 비운다 (SPEC §4.5)
                    v.renderer.discard_target();
                    match &mut v.content {
                        Content::Pdf(pv) => pv.release_bitmaps(),
                        Content::Text(tv) => tv.release(),
                        _ => {}
                    }
                } else {
                    v.renderer.resize(w, h);
                }
                drop(v);
                app.invalidate_doc();
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let _ = SetFocus(Some(hwnd));
                SetCapture(hwnd);
                app.on_mouse(msg, wparam, lparam);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let _ = ReleaseCapture();
                app.on_mouse(msg, wparam, lparam);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                app.on_mouse(msg, wparam, lparam);
                LRESULT(0)
            }
            WM_SETCURSOR if loword(lparam.0 as usize) == HTCLIENT => {
                app.apply_cursor();
                LRESULT(1)
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                app.on_doc_wheel(
                    hiword(wparam.0) as u16 as i16,
                    wparam.0 & 0xFFFF,
                    msg == WM_MOUSEHWHEEL,
                    lparam,
                );
                LRESULT(0)
            }
            WM_VSCROLL => {
                app.on_scrollbar(SB_VERT, loword(wparam.0));
                LRESULT(0)
            }
            WM_HSCROLL => {
                app.on_scrollbar(SB_HORZ, loword(wparam.0));
                LRESULT(0)
            }
            WM_KEYDOWN => {
                app.on_doc_key(VIRTUAL_KEY(wparam.0 as u16));
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

//! 메인 창, 폴더 트리(TreeView), 문서 영역(D2D 자식 창).
//!
//! 재진입 규칙: 창 프로시저는 컨트롤에 메시지를 보내는 동안 다시 호출될 수 있다(WM_NOTIFY 등).
//! 그래서 `App`의 상태는 필드별 `Cell`/`RefCell`로 나누고, **컨트롤에 SendMessage 하는 동안에는
//! 어떤 RefCell 빌림도 들고 있지 않는다.**

use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString, c_void};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, unbounded};
use leaf_core::fs_tree::{self, Entry, ListOptions};
use leaf_core::perf;
use leaf_core::worker::WorkerPool;
use leaf_fmt_pdf::{PdfEngine, PdfEvent};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::{D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, ID2D1Bitmap};
use windows::Win32::Graphics::DirectWrite::IDWriteTextLayout;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR, Result, w};

use crate::StartOptions;
use crate::render::{self, Renderer, rect, rgb};
use crate::tree::{LoadState, Node, TreeModel};
use crate::util::*;

const MAIN_CLASS: PCWSTR = w!("LeafViewer.Main");
const DOC_CLASS: PCWSTR = w!("LeafViewer.Doc");

const WM_APP_WAKE: u32 = WM_APP + 1;
const ID_TREE: usize = 100;
const ID_STATUS: usize = 101;
const ID_OPEN_FOLDER: usize = 200;
const TIMER_SELECT: usize = 1;

const SIDEBAR_DIP: i32 = 280;
const MARGIN: f32 = 16.0;
/// M0 텍스트 미리보기 상한. 대용량 처리는 M2(희소 인덱스 + 가상화)에서.
const TEXT_PREVIEW_LIMIT: u64 = 256 * 1024;
const LPSTR_TEXTCALLBACKW: PWSTR = PWSTR(-1isize as *mut u16);
const NO_NODE: isize = -1;

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
    Pdf(PdfEvent),
}

struct LoadedText {
    text: Vec<u16>,
    size: u64,
    encoding: &'static str,
    truncated: bool,
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
    Text(TextView),
    Pdf(PdfView),
}

struct TextView {
    text: Vec<u16>,
    mono: bool,
    /// (레이아웃 폭, 레이아웃, 높이)
    layout: Option<(f32, IDWriteTextLayout, f32)>,
    scroll: f32,
}

struct PdfView {
    generation: u64,
    page_count: u32,
    page: u32,
    page_pt: (f32, f32),
    bitmap: Option<PageBitmap>,
    /// 마지막으로 요청한 (쪽, 픽셀 폭) — 중복 요청 방지
    requested: Option<(u32, u32)>,
    scroll: f32,
    opened_at: Instant,
    timings: String,
    first_page_logged: bool,
    /// 첫 페이지 렌더 시간(ms). 실제로 화면에 그린 뒤 기록한다.
    first_page_pending: Option<f64>,
}

struct PageBitmap {
    bmp: ID2D1Bitmap,
    page: u32,
    w: u32,
    h: u32,
    epoch: u64,
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
    render: Option<(u64, u32, u32)>,
    status: Option<String>,
}

// ───────────────────────── 앱 ─────────────────────────

struct App {
    hwnd: HWND,
    tree: HWND,
    status: HWND,
    doc: HWND,
    font: Cell<HFONT>,
    model: RefCell<TreeModel>,
    view: RefCell<DocView>,
    workers: WorkerPool,
    pdf: RefCell<PdfEngine>,
    waker: Waker,
    rx: Receiver<UiMsg>,
    doc_gen: Cell<u64>,
    pending_select: RefCell<Option<OsString>>,
    pending_open: Cell<Option<usize>>,
    first_paint_logged: Cell<bool>,
    perf_text: RefCell<String>,
}

fn app_of<'a>(hwnd: HWND) -> Option<&'a App> {
    let p = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const App;
    if p.is_null() {
        None
    } else {
        Some(unsafe { &*p })
    }
}

pub fn run(opts: StartOptions) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok()?;
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_TREEVIEW_CLASSES | ICC_BAR_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);

        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        let cursor = LoadCursorW(None, IDC_ARROW)?;
        let main_cls = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(main_proc),
            hInstance: hinst,
            hCursor: cursor,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut c_void),
            lpszClassName: MAIN_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&main_cls);
        let doc_cls = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(doc_proc),
            hInstance: hinst,
            hCursor: cursor,
            lpszClassName: DOC_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&doc_cls);

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
        // 기본 크기를 DPI에 맞춰 보정
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            scale(1200, dpi),
            scale(800, dpi),
            SWP_NOMOVE | SWP_NOZORDER,
        );

        let tree = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WC_TREEVIEWW,
            None,
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WINDOW_STYLE(
                    TVS_HASBUTTONS
                        | TVS_LINESATROOT
                        | TVS_SHOWSELALWAYS
                        | TVS_FULLROWSELECT
                        | TVS_TRACKSELECT,
                ),
            0,
            0,
            0,
            0,
            Some(hwnd),
            Some(HMENU(ID_TREE as *mut c_void)),
            Some(hinst),
            None,
        )?;
        let _ = SetWindowTheme(tree, w!("Explorer"), None);
        SendMessageW(
            tree,
            TVM_SETEXTENDEDSTYLE,
            Some(WPARAM(TVS_EX_DOUBLEBUFFER as usize)),
            Some(LPARAM(TVS_EX_DOUBLEBUFFER as isize)),
        );

        let status = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            STATUSCLASSNAMEW,
            None,
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(SBARS_SIZEGRIP),
            0,
            0,
            0,
            0,
            Some(hwnd),
            Some(HMENU(ID_STATUS as *mut c_void)),
            Some(hinst),
            None,
        )?;

        let doc = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            DOC_CLASS,
            None,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            0,
            0,
            0,
            0,
            Some(hwnd),
            None,
            Some(hinst),
            None,
        )?;

        let (tx, rx) = unbounded();
        let waker = Waker {
            tx,
            hwnd: hwnd.0 as usize,
        };
        let pdf_waker = waker.clone();
        let app = Box::new(App {
            hwnd,
            tree,
            status,
            doc,
            font: Cell::new(HFONT::default()),
            model: RefCell::new(TreeModel::default()),
            view: RefCell::new(DocView {
                renderer: Renderer::new(dpi)?,
                content: Content::Welcome,
                loading: None,
                info: String::new(),
            }),
            workers: WorkerPool::new(2),
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
        });
        let app_ptr = Box::into_raw(app);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_ptr as isize);
        SetWindowLongPtrW(doc, GWLP_USERDATA, app_ptr as isize);
        let app = &*app_ptr;

        app.apply_font(dpi);
        app.layout();
        app.set_status(0, "폴더를 열려면 Ctrl+O 또는 끌어다 놓기");

        let _ = ShowWindow(hwnd, SW_SHOWDEFAULT);

        if let Some(p) = opts.path {
            app.open_path(&p);
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_KEYDOWN
                && GetKeyState(VK_CONTROL.0 as i32) < 0
                && msg.wParam.0 == 'O' as usize
            {
                let _ = PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(ID_OPEN_FOLDER), LPARAM(0));
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
                for h in [self.tree, self.status] {
                    SendMessageW(
                        h,
                        WM_SETFONT,
                        Some(WPARAM(font.0 as usize)),
                        Some(LPARAM(1)),
                    );
                }
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
            let _ = MoveWindow(self.tree, 0, 0, side, h, true);
            let _ = MoveWindow(self.doc, side, 0, (rc.right - side).max(0), h, true);
            let parts = [(rc.right - scale(420, dpi)).max(100), -1];
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

    // ───── 폴더 / 트리 ─────

    /// FR-A01: 폴더면 루트로, 파일이면 상위 폴더를 루트로 열고 그 파일을 선택.
    fn open_path(&self, p: &Path) {
        let p = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        if p.is_dir() {
            self.set_root(&p, None);
        } else if let (Some(parent), Some(name)) = (p.parent(), p.file_name()) {
            self.set_root(parent, Some(name.to_os_string()));
        } else {
            self.set_status(0, &format!("열 수 없는 경로: {}", p.display()));
        }
    }

    fn set_root(&self, root: &Path, select: Option<OsString>) {
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
        *self.pending_select.borrow_mut() = select;
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

    /// `after`: 직전에 넣은 형제 항목. TVI_LAST는 형제 목록을 끝까지 훑어 O(n²)이 되므로
    /// 대량 삽입 때는 직전 핸들을 넘긴다 (NFR-P08).
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

    fn insert_text_item(&self, parent: HTREEITEM, text: &str) -> HTREEITEM {
        let mut wtext = wide(text);
        let tvis = TVINSERTSTRUCTW {
            hParent: parent,
            hInsertAfter: TVI_LAST,
            Anonymous: TVINSERTSTRUCTW_0 {
                item: TVITEMW {
                    mask: TVIF_TEXT | TVIF_CHILDREN | TVIF_PARAM,
                    pszText: PWSTR(wtext.as_mut_ptr()),
                    cChildren: TVITEMEXW_CHILDREN(0),
                    lParam: LPARAM(NO_NODE),
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
        let ph = self.insert_text_item(item, "불러오는 중…");
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
            self.insert_text_item(parent_item, &format!("⚠ 읽을 수 없음: {e}"));
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

        // 명령줄로 받은 파일 선택
        if idx == 0 {
            let want = self.pending_select.borrow_mut().take();
            if let Some(want) = want {
                let target = {
                    let m = self.model.borrow();
                    m.nodes
                        .iter()
                        .skip(1)
                        .find(|n| n.path.file_name() == Some(want.as_os_str()))
                        .map(|n| n.item)
                };
                if let Some(item) = target {
                    unsafe {
                        SendMessageW(
                            self.tree,
                            TVM_SELECTITEM,
                            Some(WPARAM(TVGN_CARET as usize)),
                            Some(LPARAM(item)),
                        );
                    }
                }
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
            Ok(Some(p)) => self.set_root(&p, None),
            Ok(None) => {}
            Err(e) => self.set_status(0, &format!("폴더 선택 실패: {e}")),
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
        let generation = self.doc_gen.get() + 1;
        self.doc_gen.set(generation);
        let ext = path
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let now = Instant::now();
        self.view.borrow_mut().loading = Some((generation, path.clone(), now));
        self.set_status(0, &format!("여는 중… {}", path.display()));

        match ext.as_str() {
            "pdf" => self.pdf.borrow_mut().open(generation, &path),
            "txt" | "md" | "markdown" => {
                self.pdf.borrow_mut().close();
                let waker = self.waker.clone();
                self.workers.spawn(move || {
                    let result = load_text(&path);
                    waker.send(UiMsg::TextLoaded { generation, result });
                });
            }
            _ => {
                self.pdf.borrow_mut().close();
                let mut v = self.view.borrow_mut();
                v.loading = None;
                v.content =
                    Content::Message(format!("지원하지 않는 형식입니다: {}", path.display()));
                drop(v);
                self.invalidate_doc();
            }
        }
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
                UiMsg::Pdf(ev) => self.on_pdf_event(ev),
            }
        }
    }

    fn on_text_loaded(&self, generation: u64, result: std::result::Result<LoadedText, String>) {
        let mut v = self.view.borrow_mut();
        let Some((g, path, started)) = v.loading.take() else {
            return;
        };
        if g != generation {
            v.loading = Some((g, path, started));
            return;
        }
        let status = match result {
            Ok(t) => {
                let mono = !path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("md"));
                let s = format!(
                    "{} · {} · {}{}",
                    path.file_name()
                        .map(|n| n.to_string_lossy())
                        .unwrap_or_default(),
                    t.encoding,
                    human_size(t.size),
                    if t.truncated {
                        " · M0 미리보기(앞 256KB)"
                    } else {
                        ""
                    }
                );
                v.content = Content::Text(TextView {
                    text: t.text,
                    mono,
                    layout: None,
                    scroll: 0.0,
                });
                *self.perf_text.borrow_mut() =
                    format!("로드 {:.0}ms", started.elapsed().as_secs_f64() * 1000.0);
                s
            }
            Err(e) => {
                v.content = Content::Message(format!(
                    "파일을 열 수 없습니다.\n\n{}\n\n{e}",
                    path.display()
                ));
                path.display().to_string()
            }
        };
        v.info = status.clone();
        drop(v);
        self.set_status(0, &status);
        self.set_status(1, &self.perf_text.borrow());
        self.invalidate_doc();
    }

    fn on_pdf_event(&self, ev: PdfEvent) {
        let mut v = self.view.borrow_mut();
        match ev {
            PdfEvent::Opened {
                generation,
                page_count,
                page_size_pt,
                bind_ms,
                load_ms,
            } => {
                let Some((g, path, started)) = v.loading.take() else {
                    return;
                };
                if g != generation {
                    v.loading = Some((g, path, started));
                    return;
                }
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                v.info = format!(
                    "{} · PDF · {}",
                    path.file_name()
                        .map(|n| n.to_string_lossy())
                        .unwrap_or_default(),
                    human_size(size)
                );
                v.content = Content::Pdf(PdfView {
                    generation,
                    page_count,
                    page: 0,
                    page_pt: page_size_pt,
                    bitmap: None,
                    requested: None,
                    scroll: 0.0,
                    opened_at: started,
                    timings: if bind_ms > 0.0 {
                        format!("DLL {bind_ms:.0} + 로드 {load_ms:.0}")
                    } else {
                        format!("로드 {load_ms:.0}")
                    },
                    first_page_logged: false,
                    first_page_pending: None,
                });
                if bind_ms > 0.0 {
                    perf::record("pdfium_bind_ms", format!("{bind_ms:.2}"));
                }
                perf::record("pdf_load_ms", format!("{load_ms:.2}"));
            }
            PdfEvent::Rendered {
                generation,
                page,
                width,
                height,
                stride,
                bgra,
                render_ms,
            } => {
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
                match render::create_bitmap(t, width, height, stride, &bgra) {
                    Ok(bmp) => {
                        pv.bitmap = Some(PageBitmap {
                            bmp,
                            page,
                            w: width,
                            h: height,
                            epoch: t.epoch,
                        });
                        if !pv.first_page_logged {
                            pv.first_page_pending = Some(render_ms);
                        }
                    }
                    Err(e) => dv.content = Content::Message(format!("비트맵 생성 실패: {e}")),
                }
            }
            PdfEvent::Error {
                generation,
                message,
            } => {
                let current = match &v.content {
                    Content::Pdf(pv) => pv.generation == generation,
                    _ => false,
                };
                let loading = v.loading.as_ref().is_some_and(|(g, ..)| *g == generation);
                if !(current || loading) {
                    return;
                }
                v.loading = None;
                v.content = Content::Message(message);
                v.info.clear();
            }
        }
        let info = v.info.clone();
        drop(v);
        self.set_status(0, &info);
        self.invalidate_doc();
    }

    // ───── 문서 영역 그리기 ─────

    fn paint_doc(&self) {
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
        if !self.first_paint_logged.replace(true) {
            let ms = process_uptime_ms();
            perf::record("first_paint_ms", format!("{ms:.1}"));
            *self.perf_text.borrow_mut() = format!("첫 화면 {ms:.0}ms");
            self.set_status(1, &self.perf_text.borrow());
        }
        if let Some((g, page, px)) = outcome.render {
            self.pdf.borrow_mut().render(g, page, px);
        }
        if let Some(s) = outcome.status {
            *self.perf_text.borrow_mut() = s;
            self.set_status(1, &self.perf_text.borrow());
        }
    }

    fn on_doc_key(&self, vk: VIRTUAL_KEY) {
        let mut v = self.view.borrow_mut();
        let (_, view_h) = view_size(&v.renderer, self.doc);
        match &mut v.content {
            Content::Pdf(pv) => {
                let last = pv.page_count.saturating_sub(1);
                let new_page = match vk {
                    VK_NEXT | VK_RIGHT => (pv.page + 1).min(last),
                    VK_PRIOR | VK_LEFT => pv.page.saturating_sub(1),
                    VK_HOME => 0,
                    VK_END => last,
                    VK_DOWN => {
                        pv.scroll += 48.0;
                        pv.page
                    }
                    VK_UP => {
                        pv.scroll -= 48.0;
                        pv.page
                    }
                    _ => return,
                };
                if new_page != pv.page {
                    pv.page = new_page;
                    pv.scroll = 0.0;
                }
            }
            Content::Text(tv) => {
                tv.scroll += match vk {
                    VK_NEXT => view_h * 0.9,
                    VK_PRIOR => -view_h * 0.9,
                    VK_DOWN => 40.0,
                    VK_UP => -40.0,
                    VK_HOME => -f32::MAX / 4.0,
                    VK_END => f32::MAX / 4.0,
                    _ => return,
                };
            }
            _ => return,
        }
        drop(v);
        self.invalidate_doc();
    }

    fn on_doc_wheel(&self, delta: i16) {
        let step = -(delta as f32) / 120.0 * 60.0;
        let mut v = self.view.borrow_mut();
        match &mut v.content {
            Content::Pdf(pv) => pv.scroll += step,
            Content::Text(tv) => tv.scroll += step,
            _ => return,
        }
        drop(v);
        self.invalidate_doc();
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
트리에서 ↑↓로 이동하면 바로 미리 봅니다. PDF는 PgUp/PgDn으로 쪽을 넘깁니다.\n\n\
— M0 기술 검증: DirectWrite 한글 렌더링 —\n\
다람쥐 헌 쳇바퀴에 타고파. 키스의 고유조건은 입술끼리 만나야 하고 특별한 기술은 필요치 않다.\n\
The quick brown fox jumps over the lazy dog. 0123456789 (){}[] ← → ✓ 😀\n\
한자 漢字 · 일본어 ひらがな · 줄바꿈이 단어 경계에서 올바르게 일어나는지 확인하기 위해 일부러 아주 긴 문장을 적어 둡니다. 창 폭을 줄여 보세요.";

fn paint(v: &mut DocView, hwnd: HWND) -> Result<PaintOutcome> {
    let mut out = PaintOutcome::default();
    let (vw, vh) = view_size(&v.renderer, hwnd);
    let dpi = v.renderer.dpi();

    // 비트맵이 이전 타깃에서 만들어졌으면 무효 → 다시 요청
    let epoch = v.renderer.target(hwnd)?.epoch;
    if let Content::Pdf(pv) = &mut v.content
        && pv.bitmap.as_ref().is_some_and(|b| b.epoch != epoch)
    {
        pv.bitmap = None;
        pv.requested = None;
    }

    // 텍스트 레이아웃(폭이 바뀌었을 때만 재생성)
    if let Content::Text(tv) = &mut v.content {
        let w = (vw - 2.0 * MARGIN).max(50.0);
        if tv
            .layout
            .as_ref()
            .is_none_or(|(lw, ..)| (lw - w).abs() > 0.5)
        {
            let fmt = if tv.mono {
                &v.renderer.mono
            } else {
                &v.renderer.body
            };
            let (layout, h) = v.renderer.layout(&tv.text, fmt, w)?;
            tv.layout = Some((w, layout, h));
        }
    }
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
                let (_, layout, h) = tv.layout.as_ref().unwrap();
                let max_scroll = (h + 2.0 * MARGIN - vh).max(0.0);
                tv.scroll = tv.scroll.clamp(0.0, max_scroll);
                render::draw_layout(t, layout, MARGIN, MARGIN - tv.scroll, &t.text);
            }
            Content::Pdf(pv) => {
                t.rt.Clear(Some(&rgb(render::COLOR_PDF_BG)));
                // 폭 맞춤 (FR-P02 기본값)
                let page_w = (vw - 2.0 * MARGIN).max(50.0);
                let aspect = match &pv.bitmap {
                    Some(b) if b.page == pv.page => b.h as f32 / b.w as f32,
                    _ => pv.page_pt.1 / pv.page_pt.0.max(1.0),
                };
                let page_h = page_w * aspect;
                let max_scroll = (page_h + 2.0 * MARGIN - vh).max(0.0);
                pv.scroll = pv.scroll.clamp(0.0, max_scroll);
                let dest = rect(
                    MARGIN,
                    MARGIN - pv.scroll,
                    MARGIN + page_w,
                    MARGIN - pv.scroll + page_h,
                );

                let want_px = (page_w * dpi / 96.0).round() as u32;
                match &pv.bitmap {
                    // 다른 쪽이거나 폭이 달라도 우선 있는 비트맵을 늘려 그린다 (FR-P04 점진 표시)
                    Some(b) if b.page == pv.page => {
                        t.rt.DrawBitmap(
                            &b.bmp,
                            Some(&dest),
                            1.0,
                            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                            None,
                        );
                    }
                    _ => t.rt.FillRectangle(&dest, &t.placeholder),
                }
                let up_to_date = pv
                    .bitmap
                    .as_ref()
                    .is_some_and(|b| b.page == pv.page && b.w.abs_diff(want_px) <= 1);
                if !up_to_date && pv.requested != Some((pv.page, want_px)) {
                    pv.requested = Some((pv.page, want_px));
                    out.render = Some((pv.generation, pv.page, want_px));
                }
                let status = format!("{} / {}쪽 · 폭 맞춤", pv.page + 1, pv.page_count);
                if let Some(render_ms) = pv.first_page_pending.take() {
                    let total = pv.opened_at.elapsed().as_secs_f64() * 1000.0;
                    pv.first_page_logged = true;
                    perf::record(
                        "pdf_first_page_ms",
                        format!("{total:.1} render={render_ms:.1}"),
                    );
                    pv.timings = format!(
                        "첫 페이지 {total:.0}ms ({} + 렌더 {render_ms:.0})",
                        pv.timings
                    );
                }
                out.status = Some(format!("{status} · {}", pv.timings));
            }
        }
    }
    v.renderer.end_draw()?;
    Ok(out)
}

/// M0 텍스트 미리보기: BOM → UTF-8 → (실패 시) 손실 변환. 정식 인코딩 감지는 M2 (FR-T01).
fn load_text(path: &Path) -> std::result::Result<LoadedText, String> {
    use std::io::Read;
    let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut buf = Vec::new();
    f.take(TEXT_PREVIEW_LIMIT)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    let truncated = size > TEXT_PREVIEW_LIMIT;
    let (text, encoding): (Vec<u16>, &'static str) =
        if let Some(rest) = buf.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
            (
                String::from_utf8_lossy(rest).encode_utf16().collect(),
                "UTF-8 BOM",
            )
        } else if let Some(rest) = buf.strip_prefix(&[0xFF, 0xFE]) {
            (
                rest.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect(),
                "UTF-16 LE",
            )
        } else if let Some(rest) = buf.strip_prefix(&[0xFE, 0xFF]) {
            (
                rest.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_be_bytes(*c))
                    .collect(),
                "UTF-16 BE",
            )
        } else {
            match std::str::from_utf8(&buf) {
                Ok(s) => (s.encode_utf16().collect(), "UTF-8"),
                // 잘린 끝의 불완전한 UTF-8 시퀀스는 정상으로 본다
                Err(e) if truncated && e.error_len().is_none() => (
                    String::from_utf8_lossy(&buf[..e.valid_up_to()])
                        .encode_utf16()
                        .collect(),
                    "UTF-8",
                ),
                Err(_) => (
                    String::from_utf8_lossy(&buf).encode_utf16().collect(),
                    "UTF-8(손실)",
                ),
            }
        };
    Ok(LoadedText {
        text,
        size,
        encoding,
        truncated,
    })
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
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_TIMER if wparam.0 == TIMER_SELECT => {
                let _ = KillTimer(Some(hwnd), TIMER_SELECT);
                if let Some(idx) = app.pending_open.take() {
                    app.open_node(idx);
                }
                LRESULT(0)
            }
            WM_APP_WAKE => {
                app.drain_messages();
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
                    // 최소화: 디바이스 리소스(비트맵 포함)를 비운다 (SPEC §4.5)
                    v.renderer.discard_target();
                    if let Content::Pdf(pv) = &mut v.content {
                        pv.bitmap = None;
                        pv.requested = None;
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
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                app.on_doc_wheel(hiword(wparam.0) as u16 as i16);
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

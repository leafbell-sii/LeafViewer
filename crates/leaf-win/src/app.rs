//! 메인 창, 사이드바(파일 트리 / 목차 탭), 찾기 막대, 문서 영역(D2D 자식 창), 상태 표시줄.
//!
//! 재진입 규칙: 창 프로시저는 컨트롤에 메시지를 보내는 동안 다시 호출될 수 있다(WM_SIZE 등).
//! 그래서 `App`의 상태는 필드별 `Cell`/`RefCell`로 나누고, **다른 창에 SendMessage 하는 동안에는
//! 어떤 RefCell 빌림도 들고 있지 않는다.** 보기들은 요청을 `Cmd`로 돌려주고, 빌림을 놓은 뒤 처리한다.
//! 창 프로시저의 패닉은 잡아서 오류 화면으로 바꾼다 (FR-V07, §6.2).

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender, unbounded};
use leaf_core::config::{self, Config, Theme, ZoomSetting};
use leaf_core::fs_tree::{self, Entry, ListOptions};
use leaf_core::perf;
use leaf_core::session::{self, FilePos, Session, WindowState};
use leaf_core::tree::{LoadState, TreeModel, VisRow};
use leaf_core::watch::{WatchEvent, Watcher};
use leaf_core::worker::WorkerPool;
use leaf_doc::Dest;
use leaf_doc::format::ViewKind;
use leaf_doc::paged::ZoomMode;
use leaf_fmt_pdf::{PdfEngine, PdfEvent};
use leaf_fmt_text::search::Hit;
use leaf_fmt_text::{LineIndex, TextDoc};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::{
    COPYDATASTRUCT, CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::{EM_SETSEL, SetScrollInfo, WC_EDITW};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};

use crate::StartOptions;
use crate::bar::{BAR_HEIGHT_DIP, Bar, BarEvent, BarMode, WM_APP_BAR};
use crate::chrome::{self, Strip, StripKind, WM_APP_TAB};
use crate::custom::{CustomCmd, CustomView, OpenCtx};
use crate::editor::{self, Editor, SaveError};
use crate::formats;
use crate::mdload::{self, DecodedImage, LoadedMd};
use crate::mdview::{MdCmd, MdStyle, MdView};
use crate::pdfview::{self, Cmd, CursorKind, PdfView};
use crate::render::{self, Renderer, rgb};
use crate::textload;
use crate::textview::{self, CopyJob, SearchJob, TextStyle, TextView};
use crate::theme;
use crate::treelist::{self, Fonts, Icon, ListEvent, ListState, Row};
use crate::util::*;

const MAIN_CLASS: PCWSTR = w!("LeafViewer.Main");
const DOC_CLASS: PCWSTR = w!("LeafViewer.Doc");

const WM_APP_WAKE: u32 = WM_APP + 1;
/// 창 프로시저에서 잡은 패닉을 복구한다
const WM_APP_PANIC: u32 = WM_APP + 4;
/// 새 이름 입력칸이 포커스를 잃음 → 확정 (wparam = 입력칸 HWND)
const WM_APP_NAMING: u32 = WM_APP + 5;
/// 트리에 끼워 넣는 새 이름 입력 행의 key
const NAMING_KEY: isize = -2;
const ID_OPEN_FOLDER: usize = 200;
const TIMER_SELECT: usize = 1;
const TIMER_FIND: usize = 2;
const TIMER_BENCH: usize = 3;
const TIMER_WATCH: usize = 4;

/// 사이드바 기본·최소 폭(DIP), 탭 줄·상태 표시줄 높이, 끌어서 폭을 바꾸는 틈
const SIDEBAR_DIP: i32 = 280;
const SIDEBAR_MIN_DIP: i32 = 140;
const TABS_DIP: i32 = 32;
const STATUS_DIP: i32 = 24;
const SPLIT_DIP: i32 = 4;
const MARGIN: f32 = 16.0;
const WHEEL_STEP: f32 = 60.0;
/// 스크롤 벤치마크: 틱마다 내리는 양(DIP)과 최대 틱 수
const BENCH_STEP: f32 = 40.0;
const BENCH_MAX_TICKS: usize = 600;
/// 파일 감시 이벤트를 모으는 시간 (FR-F05: 200ms 디바운스)
const WATCH_DEBOUNCE_MS: u32 = 200;
/// 뒤로/앞으로 기록 최대 개수 (FR-V02)
const HISTORY_MAX: usize = 100;
/// 단일 인스턴스: 두 번째 실행이 기존 창에 경로를 넘길 때 WM_COPYDATA 식별자 (FR-A06)
const COPYDATA_OPEN: usize = 0x4C56_4F50;

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
    MdLoaded {
        generation: u64,
        result: std::result::Result<Box<LoadedMd>, String>,
        started: Instant,
    },
    MdImage {
        generation: u64,
        id: u32,
        result: std::result::Result<DecodedImage, String>,
    },
    Watch(WatchEvent),
    /// 확장자로 모르는 파일의 앞부분을 읽어 판별한 결과 (§4.3 sniff)
    Sniffed {
        generation: u64,
        path: PathBuf,
        format: Option<&'static str>,
    },
    /// 전용 보기 포맷(`CustomFormat::load`) 결과
    CustomLoaded {
        generation: u64,
        format: &'static str,
        result: std::result::Result<Box<dyn Any + Send>, String>,
    },
    /// 전용 보기가 요청한 워커 작업의 결과
    CustomResult {
        generation: u64,
        payload: Box<dyn Any + Send>,
    },
}

/// MD를 어떻게 열지 (FR-M06, FR-M09)
#[derive(Clone, Copy, PartialEq, Eq)]
enum MdMode {
    /// 렌더링 상한(기본 8MB) 이하면 렌더링, 넘으면 원문
    Auto,
    Raw,
    Render,
}

/// 문서를 여는 이유
#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenKind {
    /// 트리 선택·링크·끌어다 놓기 → 뒤로 가기 기록에 남긴다
    Navigate,
    /// 같은 문서 다시 읽기(F5·파일 감시·설정 변경·Ctrl+U)
    Reload,
    /// 뒤로/앞으로
    History,
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

/// 워커 작업의 패닉을 오류로 바꾼다 — 패닉이 나도 "여는 중…"에 멈추지 않는다 (FR-V07, §6.2)
fn guarded<T>(
    f: impl FnOnce() -> std::result::Result<T, String>,
) -> std::result::Result<T, String> {
    catch_unwind(AssertUnwindSafe(f))
        .unwrap_or_else(|p| Err(format!("내부 오류: {}", panic_text(&*p))))
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "알 수 없는 패닉".into())
}

fn log_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("LeafViewer").join("logs"))
}

/// 패닉을 `logs\crash.log`에 남긴다 (§6.2). GUI 앱이라 콘솔이 없다.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let Some(dir) = log_dir() else { return };
        let _ = std::fs::create_dir_all(&dir);
        let secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = format!(
            "unix={secs} pid={} thread={} {info}\n",
            std::process::id(),
            std::thread::current().name().unwrap_or("?")
        );
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("crash.log"))
        {
            let _ = f.write_all(line.as_bytes());
        }
    }));
}

// ───────────────────────── 문서 영역 상태 ─────────────────────────

enum Content {
    Welcome,
    /// 오류·안내 화면. 첫 줄과 본문을 빈 줄로 나누면 첫 줄을 제목으로 그린다
    Message(String),
    Text(Box<TextView>),
    Pdf(Box<PdfView>),
    Md(Box<MdView>),
    /// 레지스트리의 전용 보기 포맷 (이미지 등, §4.3 `OpenedDoc::Custom`)
    Custom(Box<dyn CustomView>),
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
    md_cmds: Vec<MdCmd>,
    md_first_paint_ms: Option<f64>,
    custom_cmds: Vec<CustomCmd>,
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

/// 뒤로/앞으로 (FR-V02): 문서와 위치를 함께 기억한다
#[derive(Default)]
struct History {
    back: Vec<FilePos>,
    fwd: Vec<FilePos>,
}

impl History {
    fn push(&mut self, pos: FilePos) {
        self.back.push(pos);
        if self.back.len() > HISTORY_MAX {
            self.back.remove(0);
        }
        self.fwd.clear();
    }
}

/// 파일 감시로 모은 변경 (디바운스 동안)
#[derive(Default)]
struct WatchDirty {
    paths: Vec<PathBuf>,
    rescan: bool,
    timer: bool,
}

/// 사이드바의 가상 트리 목록 하나
struct List {
    hwnd: HWND,
    st: RefCell<ListState>,
}

/// 트리에서 새 파일·폴더 이름을 입력하는 중 (입력칸은 파일 트리 목록의 자식 EDIT)
struct Naming {
    /// 만들 위치(폴더)의 모델 번호
    parent: usize,
    dir: bool,
    edit: HWND,
}

/// 목차 (FR-P07, FR-M07): 평탄화한 항목과 펼침 상태
#[derive(Default)]
struct Outline {
    items: Vec<(usize, String, Option<Dest>)>,
    expanded: Vec<bool>,
    current: Option<usize>,
}

// ───────────────────────── 앱 ─────────────────────────

struct App {
    hwnd: HWND,
    tabs: Strip,
    status: Strip,
    files: List,
    outline: List,
    doc: HWND,
    bar: Bar,
    bar_mode: Cell<Option<BarMode>>,
    fonts: RefCell<Fonts>,
    active_tab: Cell<usize>,
    model: RefCell<TreeModel>,
    outline_data: RefCell<Outline>,
    /// 링크로 다른 MD를 열 때 열린 뒤 이동할 앵커 (경로, 앵커)
    pending_anchor: RefCell<Option<(PathBuf, String)>>,
    /// 뒤로/앞으로로 열 때 복원할 위치
    pending_pos: RefCell<Option<FilePos>>,
    /// 트리에서 찾아가 선택할 파일 (폴더를 펼치며 찾아간다)
    pending_reveal: RefCell<Option<PathBuf>>,
    /// 세션에서 복원할 펼친 폴더
    pending_expand: RefCell<Vec<PathBuf>>,
    /// MD를 원문/렌더링 중 어느 쪽으로 열지 (Ctrl+U, 큰 MD)
    md_mode: Cell<MdMode>,
    view: RefCell<DocView>,
    workers: WorkerPool,
    pdf: RefCell<PdfEngine>,
    waker: Waker,
    rx: Receiver<UiMsg>,
    doc_gen: Cell<u64>,
    /// 지금 문서 파일의 (크기, 수정 시각) — 감시 이벤트가 실제 변경인지 확인
    doc_stamp: Cell<Option<(u64, SystemTime)>>,
    pending_open: Cell<Option<usize>>,
    first_paint_logged: Cell<bool>,
    perf_text: RefCell<String>,
    cursor: Cell<CursorKind>,
    scroll_cache: Cell<Option<ScrollKey>>,
    /// 스크롤바 값 축소 비율 (i32 범위를 넘는 바이트 단위용)
    scroll_scale: Cell<f64>,
    session: RefCell<Session>,
    session_path: Option<PathBuf>,
    config: RefCell<Config>,
    config_path: Option<PathBuf>,
    /// 트리에 지원하지 않는 파일도 보일지 (설정값으로 시작, 우클릭 메뉴로 전환)
    show_all: Cell<bool>,
    watcher: RefCell<Watcher>,
    watch_dirty: RefCell<WatchDirty>,
    history: RefCell<History>,
    sidebar_w: Cell<i32>,
    sidebar_hidden: Cell<bool>,
    /// 사이드바 폭을 끄는 중: 누른 지점과 사이드바 오른쪽 끝의 차이(px)
    split_drag: Cell<Option<i32>>,
    bench: RefCell<Option<Bench>>,
    /// TXT·MD 편집 중이면 EDIT 컨트롤 (문서 영역을 덮는다)
    editor: RefCell<Option<Editor>>,
    naming: RefCell<Option<Naming>>,
    /// 저장 확인 등 메시지 상자를 띄운 중 (그 사이 다시 묻지 않는다)
    modal: Cell<bool>,
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
            None,
            Some(hinst),
            None,
        )
    }
}

/// 단일 인스턴스 (FR-A06): 이미 떠 있는 창이 있으면 경로를 넘기고 true
fn hand_off_to_running(path: Option<&Path>) -> bool {
    unsafe {
        let _mutex = CreateMutexW(None, false, w!("Local\\LeafViewer.SingleInstance"));
        if GetLastError() != ERROR_ALREADY_EXISTS {
            // 핸들은 프로세스가 끝날 때까지 들고 있는다 (닫으면 다음 실행이 못 알아본다)
            std::mem::forget(_mutex);
            return false;
        }
        let Ok(hwnd) = FindWindowW(MAIN_CLASS, None) else {
            return false;
        };
        let text: Vec<u16> = path
            .map(|p| {
                std::path::absolute(p)
                    .unwrap_or_else(|_| p.to_path_buf())
                    .as_os_str()
                    .to_string_lossy()
                    .encode_utf16()
                    .collect()
            })
            .unwrap_or_default();
        let cds = COPYDATASTRUCT {
            dwData: COPYDATA_OPEN,
            cbData: (text.len() * 2) as u32,
            lpData: text.as_ptr() as *mut c_void,
        };
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
        SendMessageW(
            hwnd,
            WM_COPYDATA,
            None,
            Some(LPARAM(&cds as *const _ as isize)),
        );
        true
    }
}

pub fn run(opts: StartOptions) -> Result<()> {
    install_panic_hook();
    formats::install();
    let config_path = config::default_path();
    let (cfg, cfg_err) = config_path.as_deref().map(Config::load).unwrap_or_default();
    if cfg.general.single_instance
        && !opts.new_instance
        && !opts.bench_scroll
        && hand_off_to_running(opts.path.as_deref())
    {
        return Ok(());
    }
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok()?;

        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        let cursor = LoadCursorW(None, IDC_ARROW)?;
        // 앱 아이콘: leaf-app이 리소스 ID 1로 넣는다. 없으면(테스트 빌드 등) 기본 아이콘
        let app_icon = |cx, cy| {
            LoadImageW(
                Some(hinst),
                PCWSTR(1 as _),
                IMAGE_ICON,
                cx,
                cy,
                LR_DEFAULTCOLOR,
            )
            .map(|h| HICON(h.0))
            .unwrap_or_default()
        };
        RegisterClassExW(&WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(main_proc),
            hInstance: hinst,
            hIcon: app_icon(GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON)),
            hIconSm: app_icon(GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)),
            hCursor: cursor,
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
        RegisterClassExW(&WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(list_proc),
            hInstance: hinst,
            hCursor: cursor,
            lpszClassName: treelist::LIST_CLASS,
            ..Default::default()
        });
        chrome::register(hinst)?;

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

        let tabs = Strip::create(hwnd, hinst, StripKind::Tabs, &["파일", "목차"])?;
        let files = create_child(
            treelist::LIST_CLASS,
            WS_VISIBLE | WS_VSCROLL | WS_TABSTOP | WS_CLIPCHILDREN,
            hwnd,
            hinst,
        )?;
        let outline = create_child(treelist::LIST_CLASS, WS_VSCROLL | WS_TABSTOP, hwnd, hinst)?;
        let status = Strip::create(hwnd, hinst, StripKind::Status, &["", ""])?;
        let doc = create_child(DOC_CLASS, WS_VISIBLE | WS_TABSTOP, hwnd, hinst)?;
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
        let watch_waker = waker.clone();
        let pdfium_path = if cfg.pdf.pdfium_path.trim().is_empty() {
            opts.pdfium_path
        } else {
            PathBuf::from(cfg.pdf.pdfium_path.trim())
        };
        let app = Box::new(App {
            hwnd,
            tabs,
            status,
            files: List {
                hwnd: files,
                st: RefCell::new(ListState::new(files)),
            },
            outline: List {
                hwnd: outline,
                st: RefCell::new(ListState::new(outline)),
            },
            doc,
            bar,
            bar_mode: Cell::new(None),
            fonts: RefCell::new(Fonts {
                text: HFONT::default(),
                icon: HFONT::default(),
            }),
            active_tab: Cell::new(0),
            model: RefCell::new(TreeModel::default()),
            outline_data: RefCell::new(Outline::default()),
            pending_anchor: RefCell::new(None),
            pending_pos: RefCell::new(None),
            pending_reveal: RefCell::new(None),
            pending_expand: RefCell::new(session.expanded.clone()),
            md_mode: Cell::new(MdMode::Auto),
            view: RefCell::new(DocView {
                renderer: Renderer::new(dpi, cfg.general.gpu)?,
                content: Content::Welcome,
                loading: None,
                info: String::new(),
            }),
            // 폴더 읽기 · 줄 색인 · 찾기가 서로 막지 않도록 3개 (SPEC §4.2: 2–4)
            workers: WorkerPool::new(3),
            pdf: RefCell::new(PdfEngine::new(pdfium_path, move |ev| {
                pdf_waker.send(UiMsg::Pdf(ev))
            })),
            waker,
            rx,
            doc_gen: Cell::new(0),
            doc_stamp: Cell::new(None),
            pending_open: Cell::new(None),
            first_paint_logged: Cell::new(false),
            perf_text: RefCell::new(String::new()),
            cursor: Cell::new(CursorKind::Arrow),
            scroll_cache: Cell::new(None),
            scroll_scale: Cell::new(1.0),
            sidebar_w: Cell::new(
                session
                    .sidebar_w
                    .unwrap_or(SIDEBAR_DIP)
                    .max(SIDEBAR_MIN_DIP),
            ),
            sidebar_hidden: Cell::new(session.sidebar_hidden),
            session: RefCell::new(session),
            session_path,
            show_all: Cell::new(cfg.general.show_all_files),
            config: RefCell::new(cfg),
            config_path,
            watcher: RefCell::new(Watcher::new(move |ev| watch_waker.send(UiMsg::Watch(ev)))),
            watch_dirty: RefCell::new(WatchDirty::default()),
            history: RefCell::new(History::default()),
            split_drag: Cell::new(None),
            bench: RefCell::new(opts.bench_scroll.then(Bench::default)),
            editor: RefCell::new(None),
            naming: RefCell::new(None),
            modal: Cell::new(false),
        });
        let app_ptr = Box::into_raw(app);
        for h in [hwnd, doc, files, outline] {
            SetWindowLongPtrW(h, GWLP_USERDATA, app_ptr as isize);
        }
        let app = &*app_ptr;

        app.apply_theme();
        app.apply_font(dpi);
        app.fill_outline(&[], false);
        app.set_status(0, "폴더를 열려면 Ctrl+O 또는 끌어다 놓기");
        if let Some(e) = cfg_err {
            app.set_status(1, &format!("설정 파일 오류 — 기본값 사용: {e}"));
        }

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
                app.set_root(&root);
                if let Some(f) = file.filter(|f| f.is_file()) {
                    app.reveal_in_tree(&f);
                    app.open_file(f, OpenKind::Reload);
                }
            }
        }
        app.update_watch();

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if (msg.message == WM_KEYDOWN || msg.message == WM_SYSKEYDOWN)
                && guard_bool(app.hwnd, || {
                    app.accelerator(msg.wParam.0 as u16, msg.message == WM_SYSKEYDOWN)
                })
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

/// 가속기 처리 중 패닉도 잡는다
fn guard_bool(hwnd: HWND, f: impl FnOnce() -> bool) -> bool {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_APP_PANIC, WPARAM(0), LPARAM(0));
        }
        true
    })
}

fn is_md_path(p: &Path) -> bool {
    leaf_core::format::registry()
        .by_path(p)
        .is_some_and(|f| f.id == leaf_fmt_markdown::FORMAT.id)
}

/// 파일 앞부분으로 포맷 판별 (워커에서 호출, 확장자를 모를 때)
fn sniff_file(path: &Path) -> Option<&'static str> {
    use std::io::Read;
    let mut head = Vec::with_capacity(leaf_core::format::SNIFF_LEN);
    std::fs::File::open(path)
        .ok()?
        .take(leaf_core::format::SNIFF_LEN as u64)
        .read_to_end(&mut head)
        .ok()?;
    leaf_core::format::registry().sniff(&head).map(|f| f.id)
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

/// Windows 경로는 대소문자를 구분하지 않는다.
fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
}

/// (크기, 수정 시각). 파일 감시 이벤트가 실제 변경인지 가린다.
fn file_stamp(p: &Path) -> Option<(u64, SystemTime)> {
    let m = std::fs::metadata(p).ok()?;
    Some((m.len(), m.modified().ok()?))
}

/// 이 앱이 여는 확장자: "열기" 동사가 다시 이 앱을 띄울 수 있어 편집기 후보에서 뺀다
fn is_viewer_ext(p: &Path) -> bool {
    p.extension()
        .and_then(OsStr::to_str)
        .is_some_and(leaf_core::is_supported_ext)
}

/// `"C:\…\code.exe" --flag 파일` → (실행 파일, 나머지 인자)
/// 새 파일·폴더 이름 검사 (Windows 규칙)
fn validate_name(s: &str) -> std::result::Result<(), &'static str> {
    if s == "." || s == ".." {
        return Err("쓸 수 없는 이름입니다");
    }
    if s.chars().any(|c| c < ' ' || "<>:\"/\\|?*".contains(c)) {
        return Err("이름에 \\ / : * ? \" < > | 를 쓸 수 없습니다");
    }
    if s.ends_with('.') {
        return Err("이름은 마침표로 끝날 수 없습니다");
    }
    let stem = s
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0');
    if reserved {
        return Err("Windows 예약 이름이라 쓸 수 없습니다");
    }
    Ok(())
}

fn split_command(line: &str) -> (String, String) {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix('"')
        && let Some(end) = rest.find('"')
    {
        return (rest[..end].to_owned(), rest[end + 1..].trim().to_owned());
    }
    match line.split_once(' ') {
        Some((a, b)) => (a.to_owned(), b.trim().to_owned()),
        None => (line.to_owned(), String::new()),
    }
}

/// 실행하지 않고 탐색기에서 선택만 한다 (§6.3, FR-F10)
fn reveal_in_explorer(hwnd: HWND, p: &Path) {
    let args = wide(format!("/select,\"{}\"", p.display()));
    unsafe {
        ShellExecuteW(
            Some(hwnd),
            w!("open"),
            w!("explorer.exe"),
            PCWSTR(args.as_ptr()),
            None,
            SW_SHOWNORMAL,
        );
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
            .is_err()
            {
                return;
            }
            let font = CreateFontIndirectW(&ncm.lfMessageFont);
            // 트리 펼침 표시·아이콘: Windows 11 기본 아이콘 글꼴
            let mut lf = LOGFONTW {
                lfHeight: -scale(12, dpi),
                lfCharSet: DEFAULT_CHARSET,
                lfQuality: CLEARTYPE_QUALITY,
                ..Default::default()
            };
            for (d, s) in lf
                .lfFaceName
                .iter_mut()
                .zip("Segoe Fluent Icons".encode_utf16())
            {
                *d = s;
            }
            let icon = CreateFontIndirectW(&lf);
            self.tabs.set_font(font, dpi);
            self.status.set_font(font, dpi);
            self.bar.set_font(font);
            let old = std::mem::replace(&mut *self.fonts.borrow_mut(), Fonts { text: font, icon });
            for l in [&self.files, &self.outline] {
                l.st.borrow_mut().set_dpi(dpi);
                l.st.borrow().invalidate();
            }
            for h in [old.text, old.icon] {
                if !h.is_invalid() {
                    let _ = DeleteObject(h.into());
                }
            }
        }
    }

    // ───── 테마 (FR-A03) ─────

    fn want_dark(&self) -> bool {
        match self.config.borrow().general.theme {
            Theme::System => theme::system_dark(),
            Theme::Light => false,
            Theme::Dark => true,
        }
    }

    /// 테마 적용: 색표 교체, 제목 표시줄·스크롤바·메뉴, D2D 타깃 다시 만들기
    fn apply_theme(&self) {
        let dark = self.want_dark();
        theme::set(if dark { theme::DARK } else { theme::LIGHT });
        theme::apply_title_bar(self.hwnd, dark);
        theme::apply_menu_theme(dark);
        for h in [self.doc, self.files.hwnd, self.outline.hwnd] {
            theme::apply_scrollbar_theme(h, dark);
        }
        self.bar.apply_theme(dark);
        if let Some(e) = self.editor.borrow().as_ref() {
            e.apply_theme(dark);
        }
        if let Some(n) = self.naming.borrow().as_ref() {
            theme::apply_edit_theme(n.edit, dark);
        }
        self.view.borrow_mut().renderer.discard_target();
        self.scroll_cache.set(None);
        unsafe {
            let _ = RedrawWindow(
                Some(self.hwnd),
                None,
                None,
                RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_FRAME,
            );
        }
    }

    // ───── 설정 (§8, FR-A07) ─────

    fn reload_config(&self) {
        let Some(path) = &self.config_path else {
            return;
        };
        let (new, err) = Config::load(path);
        let old = self.config.replace(new.clone());
        match err {
            Some(e) => self.set_status(1, &format!("설정 파일 오류 — 기본값 사용: {e}")),
            None => self.set_status(1, "설정을 다시 읽었습니다"),
        }
        if old.general.theme != new.general.theme {
            self.apply_theme();
        }
        if old.general.gpu != new.general.gpu {
            self.view.borrow_mut().renderer.set_gpu(new.general.gpu);
            self.invalidate_doc();
        }
        if old.general.show_all_files != new.general.show_all_files
            || old.general.show_hidden != new.general.show_hidden
        {
            self.show_all.set(new.general.show_all_files);
            self.relist_all();
        }
        if old.text != new.text
            && let Some(e) = self.editor.borrow().as_ref()
        {
            e.set_font(&new.text, self.dpi());
        }
        if old.text != new.text
            || old.markdown != new.markdown
            || old.pdf != new.pdf
            || old.memory != new.memory
        {
            self.reload_doc();
        }
    }

    /// Ctrl+, : 설정 파일 열기. 없으면 기본값과 설명을 담아 만든다 (FR-A07)
    fn open_config(&self) {
        let Some(path) = self.config_path.clone() else {
            return;
        };
        if !path.exists() {
            let r = path
                .parent()
                .map(std::fs::create_dir_all)
                .unwrap_or(Ok(()))
                .and_then(|_| std::fs::write(&path, config::TEMPLATE));
            if let Err(e) = r {
                self.set_status(1, &format!("설정 파일을 만들 수 없습니다: {e}"));
                return;
            }
        }
        self.update_watch();
        self.edit_externally(&path);
    }

    /// 외부 편집기로 열기 (FR-V10). 설정 `[editor] command`, 비우면 연결 프로그램(편집 → 열기 → 메모장)
    fn edit_externally(&self, path: &Path) {
        let cmd = self.config.borrow().editor.command.trim().to_owned();
        let shell = |verb: PCWSTR, file: &OsStr, args: Option<&str>| -> bool {
            let f = wide(file);
            let a = args.map(wide);
            let r = unsafe {
                ShellExecuteW(
                    Some(self.hwnd),
                    verb,
                    PCWSTR(f.as_ptr()),
                    a.as_ref().map_or(PCWSTR::null(), |a| PCWSTR(a.as_ptr())),
                    None,
                    SW_SHOWNORMAL,
                )
            };
            r.0 as isize > 32
        };
        let ok = if cmd.is_empty() {
            shell(w!("edit"), path.as_os_str(), None)
                || (!is_viewer_ext(path) && shell(w!("open"), path.as_os_str(), None))
                || shell(
                    w!("open"),
                    OsStr::new("notepad.exe"),
                    Some(&format!("\"{}\"", path.display())),
                )
        } else {
            let line = if cmd.contains("{path}") {
                cmd.replace("{path}", &path.display().to_string())
            } else {
                format!("{cmd} \"{}\"", path.display())
            };
            let (exe, args) = split_command(&line);
            shell(w!("open"), OsStr::new(&exe), Some(&args))
        };
        if !ok {
            self.set_status(1, &format!("편집기를 열 수 없습니다: {}", path.display()));
        }
    }

    // ───── 배치 ─────

    fn side_px(&self, client_w: i32) -> i32 {
        if self.sidebar_hidden.get() {
            return 0;
        }
        let dpi = self.dpi();
        scale(self.sidebar_w.get(), dpi)
            .min(client_w - scale(200, dpi))
            .max(scale(SIDEBAR_MIN_DIP, dpi).min(client_w / 2))
    }

    fn layout(&self) {
        unsafe {
            let dpi = self.dpi();
            let mut rc = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rc);
            let status_h = scale(STATUS_DIP, dpi);
            let h = (rc.bottom - status_h).max(0);
            let _ = MoveWindow(self.status.hwnd, 0, h, rc.right, status_h, true);
            let side = self.side_px(rc.right);
            let hidden = self.sidebar_hidden.get();
            let split = if hidden { 0 } else { scale(SPLIT_DIP, dpi) };
            let tabs_h = scale(TABS_DIP, dpi);
            let tab = self.active_tab.get();
            let _ = ShowWindow(self.tabs.hwnd, if hidden { SW_HIDE } else { SW_SHOW });
            for (i, l) in [&self.files, &self.outline].iter().enumerate() {
                let _ = ShowWindow(
                    l.hwnd,
                    if !hidden && i == tab {
                        SW_SHOW
                    } else {
                        SW_HIDE
                    },
                );
                let _ = MoveWindow(l.hwnd, 0, tabs_h, side, (h - tabs_h).max(0), true);
            }
            let _ = MoveWindow(self.tabs.hwnd, 0, 0, side, tabs_h, true);
            let x = side + split;
            let doc_w = (rc.right - x).max(0);
            let bar_h = if self.bar_mode.get().is_some() {
                scale(BAR_HEIGHT_DIP, dpi)
            } else {
                0
            };
            let _ = MoveWindow(self.bar.hwnd, x, 0, doc_w, bar_h, true);
            self.bar.layout(doc_w, dpi);
            let _ = MoveWindow(self.doc, x, bar_h, doc_w, (h - bar_h).max(0), true);
            // 편집 중이면 EDIT 컨트롤이 문서 영역을 대신한다
            let ed = self.editor.borrow().as_ref().map(|e| e.hwnd);
            match ed {
                Some(e) => {
                    let _ = MoveWindow(e, x, bar_h, doc_w, (h - bar_h).max(0), true);
                    let _ = ShowWindow(e, SW_SHOW);
                    let _ = ShowWindow(self.doc, SW_HIDE);
                }
                None => {
                    let _ = ShowWindow(self.doc, SW_SHOW);
                }
            }
            let _ = InvalidateRect(Some(self.hwnd), None, true);
        }
    }

    /// 사이드바와 문서 사이 틈(폭 조절 손잡이) 안인지
    fn in_splitter(&self, x: i32) -> bool {
        if self.sidebar_hidden.get() {
            return false;
        }
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        let side = self.side_px(rc.right);
        x >= side - 2 && x < side + scale(SPLIT_DIP, self.dpi()) + 2
    }

    /// Ctrl+B: 사이드바 접기/펴기 (FR-A05)
    fn toggle_sidebar(&self) {
        let hidden = !self.sidebar_hidden.get();
        self.sidebar_hidden.set(hidden);
        self.layout();
        unsafe {
            let _ = SetFocus(Some(if hidden {
                self.doc_focus()
            } else {
                self.current_list().hwnd
            }));
        }
    }

    /// 문서 영역에서 포커스를 받을 창 (편집 중이면 EDIT)
    fn doc_focus(&self) -> HWND {
        self.editor.borrow().as_ref().map_or(self.doc, |e| e.hwnd)
    }

    fn current_list(&self) -> &List {
        if self.active_tab.get() == 1 {
            &self.outline
        } else {
            &self.files
        }
    }

    fn set_status(&self, part: usize, text: &str) {
        self.status.set_text(part, text);
    }

    fn invalidate_doc(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.doc), None, false);
        }
    }

    fn select_tab(&self, i: usize) {
        self.active_tab.set(i.min(1));
        self.tabs.set_active(i.min(1));
        self.layout();
    }

    // ───── 키보드 가속기 ─────

    /// 포커스 위치와 상관없이 동작하는 단축키. 처리했으면 true. `alt`: WM_SYSKEYDOWN
    fn accelerator(&self, vk: u16, alt: bool) -> bool {
        let ctrl = ctrl_down();
        let vk = VIRTUAL_KEY(vk);
        let focus = unsafe { GetFocus() };
        // 트리의 새 이름 입력칸: Enter 확정, Esc 취소. 나머지 키는 입력칸이 받는다
        let naming = self.naming.borrow().as_ref().map(|n| n.edit);
        if naming == Some(focus) {
            match vk {
                VK_RETURN if !alt => self.finish_naming(true, true),
                VK_ESCAPE if !alt => self.finish_naming(false, false),
                _ => return false,
            }
            return true;
        }
        // 편집 중: Ctrl+S 저장 · Ctrl+E/Esc 끝내기 · Ctrl+A 전체 선택. 그 밖의 키는 EDIT가 받는다
        let ed = self.editor.borrow().as_ref().map(|e| e.hwnd);
        if let Some(ed) = ed {
            let in_editor = focus == ed;
            if !alt && ctrl && vk.0 == b'S' as u16 {
                self.save_edit();
                return true;
            }
            if !alt && ctrl && !shift_down() && vk.0 == b'E' as u16 {
                self.end_edit();
                return true;
            }
            if in_editor {
                match vk {
                    _ if alt => return false,
                    _ if ctrl && vk.0 == b'A' as u16 => {
                        if let Some(e) = self.editor.borrow().as_ref() {
                            e.select_all();
                        }
                    }
                    _ if ctrl && vk.0 == b'B' as u16 => self.toggle_sidebar(),
                    VK_ESCAPE => {
                        self.end_edit();
                    }
                    _ => return false,
                }
                return true;
            }
        }
        if alt {
            match vk {
                // Alt+←/→: 뒤로/앞으로 (FR-V02)
                VK_LEFT => self.go_history(true),
                VK_RIGHT => self.go_history(false),
                // Alt+Z: 자동 줄바꿈 토글 (FR-T03)
                _ if vk.0 == b'Z' as u16 => {
                    if let Content::Text(tv) = &mut self.view.borrow_mut().content {
                        tv.toggle_wrap();
                    } else {
                        return false;
                    }
                    self.scroll_cache.set(None);
                    self.invalidate_doc();
                }
                _ => return false,
            }
            return true;
        }
        // PDF를 보는 중이면 화살표는 어디에 포커스가 있든 쪽 넘기기
        if !ctrl
            && matches!(vk, VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN)
            && matches!(self.view.borrow().content, Content::Pdf(_))
        {
            self.on_doc_key(vk);
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
            _ if ctrl && vk.0 == b'U' as u16 => return self.toggle_md_source(),
            _ if ctrl && vk.0 == b'G' as u16 => {
                let line = matches!(self.view.borrow().content, Content::Text(_));
                self.show_bar(if line {
                    BarMode::GotoLine
                } else {
                    BarMode::Goto
                });
            }
            _ if ctrl && vk.0 == b'B' as u16 => self.toggle_sidebar(),
            _ if ctrl && shift_down() && vk.0 == b'E' as u16 => match self.current_path() {
                Some(p) => self.edit_externally(&p),
                None => return false,
            },
            // Ctrl+E: 지금 문서(TXT·MD) 편집
            _ if ctrl && vk.0 == b'E' as u16 => match self.current_path() {
                Some(p) if editor::is_editable(&p) => self.start_edit(&p),
                _ => return false,
            },
            VK_OEM_COMMA if ctrl => self.open_config(),
            VK_F5 => {
                self.relist_all();
                self.reload_doc();
            }
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
        if self.editor.borrow().is_some() {
            return;
        }
        let ok = match self.view.borrow().content {
            Content::Pdf(_) | Content::Text(_) => true,
            // MD에는 쪽·줄 번호가 없다
            Content::Md(_) => mode == BarMode::Find,
            _ => false,
        };
        if !ok {
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
            Content::Md(mv) => mv.search_query().map(str::to_owned),
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
                    Content::Md(mv) => {
                        mv.clear_search();
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
            (Some(BarMode::Goto | BarMode::GotoLine), BarEvent::Enter) => {
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
            Content::Md(mv) => {
                // 렌더링하는 MD는 8MB 이하라 UI 스레드에서 바로 찾는다
                let t = Instant::now();
                mv.start_search(&q);
                perf::record(
                    "md_search_ms",
                    format!("{:.1}", t.elapsed().as_secs_f64() * 1000.0),
                );
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
            Content::Md(mv) => mv.find_step(forward),
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
                Cmd::OpenUri(u) => self.open_uri(&u),
                Cmd::Jumped(pos) => self.push_history(pos),
            }
        }
    }

    /// SPEC §6.3: http/https/mailto만, 사용자가 클릭했을 때만 기본 브라우저로
    fn open_uri(&self, u: &str) {
        if leaf_doc::is_allowed_uri(u) {
            let wu = wide(u);
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

    /// MD 보기가 돌려준 명령 처리 (빌림 없이 호출할 것).
    fn run_md_cmds(&self, generation: u64, cmds: Vec<MdCmd>) {
        for cmd in cmds {
            match cmd {
                MdCmd::LoadImage { id, path, max_w } => {
                    let waker = self.waker.clone();
                    self.workers.spawn(move || {
                        let result = mdload::decode(&path, max_w);
                        waker.send(UiMsg::MdImage {
                            generation,
                            id,
                            result,
                        });
                    });
                }
                MdCmd::OpenUri(u) => self.open_uri(&u),
                MdCmd::OpenDoc { path, anchor } => self.navigate(path, anchor),
                MdCmd::Reveal(p) => {
                    // 지원하지 않는 파일은 실행하지 않고 탐색기에서 선택만 한다 (§6.3)
                    reveal_in_explorer(self.hwnd, &p);
                    self.set_status(1, &format!("탐색기에서 보기: {}", p.display()));
                }
                MdCmd::Status(s) => self.set_status(1, &s),
                MdCmd::Jumped(pos) => self.push_history(pos),
            }
        }
    }

    /// 링크로 다른 문서 열기 (FR-M04): 트리 루트 아래면 트리 선택도 맞춘다.
    fn navigate(&self, path: PathBuf, anchor: Option<String>) {
        *self.pending_anchor.borrow_mut() = anchor.map(|a| (path.clone(), a));
        self.reveal_in_tree(&path);
        self.open_file(path, OpenKind::Navigate);
    }

    /// Ctrl+U: MD 원문 보기 ↔ 렌더링 (FR-M09). 큰 MD(FR-M06)를 렌더링할 때도 쓴다.
    fn toggle_md_source(&self) -> bool {
        let target = match &self.view.borrow().content {
            Content::Md(mv) => Some((mv.path.clone(), MdMode::Raw)),
            Content::Text(tv) if is_md_path(&tv.path) => Some((tv.path.clone(), MdMode::Render)),
            _ => None,
        };
        let Some((path, mode)) = target else {
            return false;
        };
        self.md_mode.set(mode);
        self.open_file(path, OpenKind::Reload);
        true
    }

    fn copy_to_clipboard(&self, s: &str) {
        match set_clipboard(self.hwnd, s) {
            Ok(()) => self.set_status(1, &format!("{}자 복사함", s.chars().count())),
            Err(e) => self.set_status(1, &format!("복사 실패: {e}")),
        }
    }

    // ───── 목차 (FR-P07, FR-M07) ─────

    /// `expand_all`: MD는 제목 계층을 모두 펼쳐 둔다(문서 목차처럼 한눈에), PDF는 맨 위 단계만
    fn fill_outline(&self, items: &[leaf_doc::OutlineItem], expand_all: bool) {
        {
            let mut o = self.outline_data.borrow_mut();
            o.items = pdfview::flatten_outline(items);
            o.expanded = vec![expand_all; o.items.len()];
            o.current = None;
        }
        self.rebuild_outline_rows();
        let mut st = self.outline.st.borrow_mut();
        st.select_key(None, false);
        st.scroll_to_top();
    }

    fn rebuild_outline_rows(&self) {
        let rows = {
            let o = self.outline_data.borrow();
            let mut rows = Vec::new();
            let mut hide_below: Option<usize> = None;
            for (i, (depth, title, _)) in o.items.iter().enumerate() {
                if let Some(d) = hide_below {
                    if *depth > d {
                        continue;
                    }
                    hide_below = None;
                }
                let has_children = o.items.get(i + 1).is_some_and(|n| n.0 > *depth);
                if has_children && !o.expanded[i] {
                    hide_below = Some(*depth);
                }
                let title = if title.is_empty() {
                    "(제목 없음)"
                } else {
                    title.as_str()
                };
                rows.push(Row {
                    key: i as isize,
                    depth: *depth as u16,
                    text: title.encode_utf16().collect(),
                    icon: Icon::None,
                    expandable: has_children,
                    expanded: o.expanded[i],
                    dim: false,
                });
            }
            if rows.is_empty() {
                rows.push(Row {
                    key: -1,
                    depth: 0,
                    text: "(목차 없음)".encode_utf16().collect(),
                    icon: Icon::None,
                    expandable: false,
                    expanded: false,
                    dim: true,
                });
            }
            rows
        };
        self.outline.st.borrow_mut().set_rows(rows);
    }

    fn on_outline_event(&self, ev: ListEvent) {
        match ev {
            ListEvent::Select(i) | ListEvent::Activate(i) if i >= 0 => {
                let dest = self
                    .outline_data
                    .borrow()
                    .items
                    .get(i as usize)
                    .and_then(|x| x.2);
                if let Some(d) = dest {
                    match &mut self.view.borrow_mut().content {
                        Content::Pdf(pv) => pv.go_to(d),
                        // MD: Dest.page = 레이아웃 단위
                        Content::Md(mv) => mv.go_unit(d.page as usize),
                        _ => {}
                    }
                    self.outline_data.borrow_mut().current = Some(i as usize);
                }
                self.invalidate_doc();
            }
            ListEvent::Toggle(i, e) if i >= 0 => {
                if let Some(x) = self.outline_data.borrow_mut().expanded.get_mut(i as usize) {
                    *x = e;
                }
                self.rebuild_outline_rows();
            }
            _ => {}
        }
    }

    // ───── 폴더 / 트리 (FR-F) ─────

    /// FR-A01: 폴더면 루트로, 파일이면 상위 폴더를 루트로 열고 그 파일을 연다.
    fn open_path(&self, p: &Path) {
        let p = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        if p.is_dir() {
            self.set_root(&p);
        } else if let Some(parent) = p.parent().filter(|_| p.is_file()) {
            let same_root = self
                .model
                .borrow()
                .root()
                .is_some_and(|r| same_path(&r.path, parent));
            if !same_root {
                self.set_root(parent);
            }
            self.reveal_in_tree(&p);
            self.open_file(p, OpenKind::Navigate);
        } else {
            self.set_status(0, &format!("열 수 없는 경로: {}", p.display()));
        }
        self.update_watch();
    }

    fn set_root(&self, root: &Path) {
        self.finish_naming(false, false);
        self.model.borrow_mut().set_root(root);
        *self.pending_reveal.borrow_mut() = None;
        self.files.st.borrow_mut().select_key(None, false);
        self.toggle_node(0, true);
        self.files.st.borrow_mut().select_key(Some(0), true);
        unsafe {
            let title = wide(format!("{} — LeafViewer", root.display()));
            let _ = SetWindowTextW(self.hwnd, PCWSTR(title.as_ptr()));
        }
        self.set_status(0, &root.display().to_string());
        self.update_watch();
    }

    fn list_options(&self) -> ListOptions {
        ListOptions {
            show_all_files: self.show_all.get(),
            show_hidden: self.config.borrow().general.show_hidden,
        }
    }

    /// 펼치기/접기. 처음 펼치는 폴더는 워커에서 읽는다 (FR-F02, FR-F08).
    fn toggle_node(&self, idx: usize, expand: bool) {
        let need = self.model.borrow_mut().set_expanded(idx, expand);
        if need {
            self.list_dir(idx);
        }
        self.rebuild_file_rows();
    }

    fn list_dir(&self, idx: usize) {
        let (path, generation) = {
            let m = self.model.borrow();
            let Some(n) = m.get(idx) else { return };
            (n.path.clone(), m.generation)
        };
        let opts = self.list_options();
        let waker = self.waker.clone();
        let started = Instant::now();
        self.workers.spawn(move || {
            let result = guarded(|| fs_tree::list_dir(&path, opts).map_err(|e| e.to_string()));
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

    /// 읽어 둔 폴더를 모두 다시 읽는다 (F5, 감시 이벤트 유실, 필터 변경)
    fn relist_all(&self) {
        let dirs = self.model.borrow().listed_dirs();
        for d in dirs {
            self.list_dir(d);
        }
    }

    fn on_dir_listed(
        &self,
        generation: u64,
        idx: usize,
        result: std::result::Result<Vec<Entry>, String>,
    ) {
        let t = Instant::now();
        let (added, count) = {
            let mut m = self.model.borrow_mut();
            if m.generation != generation || m.get(idx).is_none() {
                return;
            }
            let added = m.apply_listing(idx, result);
            (added, m.get(idx).map_or(0, |n| n.children.len()))
        };
        // 세션에서 펼쳐 두었던 하위 폴더를 이어서 펼친다 (FR-A02)
        let reopen: Vec<usize> = {
            let mut pe = self.pending_expand.borrow_mut();
            if pe.is_empty() {
                Vec::new()
            } else {
                let m = self.model.borrow();
                added
                    .iter()
                    .copied()
                    .filter(|&c| {
                        let n = &m.nodes[c];
                        n.is_dir && {
                            let hit = pe.iter().position(|p| same_path(p, &n.path));
                            if let Some(k) = hit {
                                pe.swap_remove(k);
                            }
                            hit.is_some()
                        }
                    })
                    .collect()
            }
        };
        for c in reopen {
            if self.model.borrow_mut().set_expanded(c, true) {
                self.list_dir(c);
            }
        }
        self.rebuild_file_rows();
        perf::record(
            "tree_insert_ms",
            format!("{:.2} entries={count}", t.elapsed().as_secs_f64() * 1000.0),
        );
        self.advance_reveal();
    }

    fn rebuild_file_rows(&self) {
        let rows: Vec<Row> = {
            let m = self.model.borrow();
            m.visible()
                .into_iter()
                .map(|r| match r {
                    VisRow::Node { idx, depth } => {
                        let n = &m.nodes[idx];
                        Row {
                            key: idx as isize,
                            depth,
                            text: n.name.encode_utf16().collect(),
                            icon: if n.is_dir { Icon::Folder } else { Icon::File },
                            expandable: m.expandable(idx),
                            expanded: n.expanded,
                            dim: false,
                        }
                    }
                    VisRow::Status { parent, depth } => {
                        let (text, icon) = match &m.nodes[parent].state {
                            LoadState::Failed(e) => (format!("읽을 수 없음: {e}"), Icon::Warning),
                            _ => ("불러오는 중…".to_owned(), Icon::None),
                        };
                        Row {
                            key: -1,
                            depth,
                            text: text.encode_utf16().collect(),
                            icon,
                            expandable: false,
                            expanded: false,
                            dim: true,
                        }
                    }
                })
                .collect::<Vec<Row>>()
        };
        let mut rows = rows;
        // 새 이름 입력 행: 만들 폴더 바로 아래 첫 줄
        let naming = self.naming.borrow().as_ref().map(|n| (n.parent, n.dir));
        if let Some((parent, dir)) = naming
            && let Some(p) = rows.iter().position(|r| r.key == parent as isize)
        {
            let depth = rows[p].depth + 1;
            rows.insert(
                p + 1,
                Row {
                    key: NAMING_KEY,
                    depth,
                    text: Vec::new(),
                    icon: if dir { Icon::Folder } else { Icon::File },
                    expandable: false,
                    expanded: false,
                    dim: false,
                },
            );
        }
        self.files.st.borrow_mut().set_rows(rows);
        self.place_naming_edit();
    }

    /// 트리에서 그 파일을 찾아 선택한다(접힌 폴더는 펼치며 찾아간다). 문서는 열지 않는다.
    /// 루트 밖 파일이면 아무것도 하지 않는다.
    fn reveal_in_tree(&self, path: &Path) {
        let under = self
            .model
            .borrow()
            .root()
            .is_some_and(|r| path.starts_with(&r.path) && path != r.path);
        if under {
            *self.pending_reveal.borrow_mut() = Some(path.to_path_buf());
            self.advance_reveal();
        }
    }

    fn advance_reveal(&self) {
        let Some(target) = self.pending_reveal.borrow().clone() else {
            return;
        };
        let comps: Vec<OsString> = {
            let m = self.model.borrow();
            let Some(root) = m.root() else { return };
            match target.strip_prefix(&root.path) {
                Ok(rel) => rel.iter().map(|c| c.to_os_string()).collect(),
                Err(_) => return,
            }
        };
        let mut cur = 0usize;
        let mut changed = false;
        for (i, comp) in comps.iter().enumerate() {
            let state = self.model.borrow().get(cur).map(|n| n.state.clone());
            match state {
                Some(LoadState::Loaded) => {
                    if !self.model.borrow().nodes[cur].expanded {
                        self.model.borrow_mut().set_expanded(cur, true);
                        changed = true;
                    }
                }
                Some(LoadState::NotLoaded) => {
                    // 아직 읽지 않은 폴더: 펼치고 읽기가 끝나면 이어서 찾는다
                    self.toggle_node(cur, true);
                    return;
                }
                Some(LoadState::Loading) => return,
                _ => {
                    *self.pending_reveal.borrow_mut() = None;
                    break;
                }
            }
            let child = self.model.borrow().find_child(cur, comp);
            let Some(c) = child else {
                // 숨김·필터 등으로 트리에 없는 파일
                *self.pending_reveal.borrow_mut() = None;
                break;
            };
            if i + 1 == comps.len() {
                *self.pending_reveal.borrow_mut() = None;
                if changed {
                    self.rebuild_file_rows();
                    changed = false;
                }
                self.files
                    .st
                    .borrow_mut()
                    .select_key(Some(c as isize), true);
                break;
            }
            cur = c;
        }
        if changed {
            self.rebuild_file_rows();
        }
    }

    fn on_files_event(&self, ev: ListEvent) {
        match ev {
            ListEvent::Select(k) if k >= 0 => {
                let is_file = self
                    .model
                    .borrow()
                    .get(k as usize)
                    .is_some_and(|n| !n.is_dir);
                if is_file {
                    // FR-F06: 100ms 디바운스 — 빠르게 넘기면 중간 파일은 열지 않는다
                    self.pending_open.set(Some(k as usize));
                    unsafe { SetTimer(Some(self.hwnd), TIMER_SELECT, 100, None) };
                }
            }
            ListEvent::Activate(k) if k >= 0 => {
                let node = self
                    .model
                    .borrow()
                    .get(k as usize)
                    .map(|n| (n.is_dir, n.expanded));
                match node {
                    Some((true, expanded)) => self.toggle_node(k as usize, !expanded),
                    Some((false, _)) => {
                        unsafe {
                            let _ = KillTimer(Some(self.hwnd), TIMER_SELECT);
                        }
                        self.pending_open.set(None);
                        self.open_node(k as usize);
                        unsafe {
                            let _ = SetFocus(Some(self.doc));
                        }
                    }
                    None => {}
                }
            }
            ListEvent::Toggle(k, e) if k >= 0 => self.toggle_node(k as usize, e),
            ListEvent::Menu(k, pt) if k >= 0 => self.file_menu(k as usize, pt),
            // 빈 곳: 루트 폴더 메뉴
            ListEvent::Menu(_, pt) if self.model.borrow().root().is_some() => self.file_menu(0, pt),
            _ => {}
        }
    }

    /// 파일 트리 우클릭 메뉴 (FR-F10, FR-F03)
    fn file_menu(&self, idx: usize, pt: POINT) {
        const OPEN: usize = 1;
        const REVEAL: usize = 2;
        const COPY: usize = 3;
        const SHELL: usize = 4;
        const EDIT: usize = 5;
        const SHOW_ALL: usize = 6;
        const REFRESH: usize = 7;
        const NEW_FILE: usize = 8;
        const NEW_DIR: usize = 9;
        const EDIT_HERE: usize = 10;
        let Some((path, is_dir)) = self
            .model
            .borrow()
            .get(idx)
            .map(|n| (n.path.clone(), n.is_dir))
        else {
            return;
        };
        let cmd = unsafe {
            let Ok(m) = CreatePopupMenu() else { return };
            let add = |id: usize, text: &str, flags: MENU_ITEM_FLAGS| {
                let t = wide(text);
                let _ = AppendMenuW(m, MF_STRING | flags, id, PCWSTR(t.as_ptr()));
            };
            if !is_dir {
                add(OPEN, "열기\tEnter", MF_ENABLED);
                if editor::is_editable(&path) {
                    add(EDIT_HERE, "편집\tCtrl+E", MF_ENABLED);
                }
            }
            add(NEW_FILE, "새 파일", MF_ENABLED);
            add(NEW_DIR, "새 폴더", MF_ENABLED);
            let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
            add(REVEAL, "탐색기에서 보기", MF_ENABLED);
            add(COPY, "경로 복사", MF_ENABLED);
            if !is_dir {
                add(SHELL, "기본 앱으로 열기", MF_ENABLED);
                add(EDIT, "외부 편집기로 열기", MF_ENABLED);
            }
            let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
            add(
                SHOW_ALL,
                "모든 파일 표시",
                if self.show_all.get() {
                    MF_CHECKED
                } else {
                    MF_UNCHECKED
                },
            );
            add(REFRESH, "새로 고침\tF5", MF_ENABLED);
            let r = TrackPopupMenu(
                m,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                pt.x,
                pt.y,
                None,
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(m);
            r.0 as usize
        };
        match cmd {
            OPEN => self.open_node(idx),
            EDIT_HERE => {
                let other = self
                    .editor
                    .borrow()
                    .as_ref()
                    .is_some_and(|e| !same_path(&e.path, &path));
                if other && !self.end_edit() {
                    return;
                }
                if !self.current_path().is_some_and(|p| same_path(&p, &path)) {
                    self.open_node(idx);
                }
                self.start_edit(&path);
            }
            NEW_FILE => self.begin_naming(idx, false, ""),
            NEW_DIR => self.begin_naming(idx, true, ""),
            REVEAL => reveal_in_explorer(self.hwnd, &path),
            COPY => {
                let s = path.display().to_string();
                if set_clipboard(self.hwnd, &s).is_ok() {
                    self.set_status(1, &format!("경로 복사함: {s}"));
                }
            }
            SHELL => {
                let p = wide(&path);
                let r = unsafe {
                    ShellExecuteW(
                        Some(self.hwnd),
                        w!("open"),
                        PCWSTR(p.as_ptr()),
                        None,
                        None,
                        SW_SHOWNORMAL,
                    )
                };
                if (r.0 as isize) <= 32 {
                    self.set_status(1, &format!("연결된 앱이 없습니다: {}", path.display()));
                }
            }
            EDIT => self.edit_externally(&path),
            SHOW_ALL => {
                self.show_all.set(!self.show_all.get());
                self.relist_all();
            }
            REFRESH => {
                self.relist_all();
                self.reload_doc();
            }
            _ => {}
        }
    }

    // ───── 새 파일 / 새 폴더 (트리 행에서 이름 입력) ─────

    /// `idx`가 폴더면 그 안에, 파일이면 같은 폴더에 만든다. `initial`: 입력칸에 미리 넣을 이름
    fn begin_naming(&self, idx: usize, dir: bool, initial: &str) {
        self.finish_naming(false, false);
        let parent = {
            let m = self.model.borrow();
            match m.get(idx) {
                Some(n) if n.is_dir => idx,
                Some(n) => match n.parent {
                    Some(p) => p,
                    None => return,
                },
                None => return,
            }
        };
        if !self.model.borrow().nodes[parent].expanded {
            self.toggle_node(parent, true);
        }
        let edit = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                WC_EDITW,
                None,
                WS_CHILD | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                0,
                0,
                0,
                0,
                Some(self.files.hwnd),
                None,
                None,
                None,
            )
        };
        let Ok(edit) = edit else { return };
        unsafe {
            let font = self.fonts.borrow().text;
            SendMessageW(
                edit,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(0)),
            );
            theme::apply_edit_theme(edit, theme::current().dark);
            let t = wide(initial);
            let _ = SetWindowTextW(edit, PCWSTR(t.as_ptr()));
            SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        }
        *self.naming.borrow_mut() = Some(Naming { parent, dir, edit });
        self.rebuild_file_rows();
        self.files.st.borrow_mut().ensure_key_visible(NAMING_KEY);
        self.place_naming_edit();
        unsafe {
            let _ = SetFocus(Some(edit));
        }
    }

    /// 입력칸을 입력 행 위치에 맞춘다 (행 목록이 바뀌거나 스크롤할 때마다)
    fn place_naming_edit(&self) {
        let Some(edit) = self.naming.borrow().as_ref().map(|n| n.edit) else {
            return;
        };
        let rc = self
            .files
            .st
            .try_borrow()
            .ok()
            .and_then(|st| st.text_rect(NAMING_KEY));
        unsafe {
            match rc {
                Some(rc) => {
                    let _ = MoveWindow(
                        edit,
                        rc.left,
                        rc.top,
                        (rc.right - rc.left).max(20),
                        rc.bottom - rc.top,
                        true,
                    );
                    let _ = ShowWindow(edit, SW_SHOW);
                }
                None => {
                    let _ = ShowWindow(edit, SW_HIDE);
                }
            }
        }
    }

    /// 이름 입력 끝. `commit`이면 만든다. `retry`(Enter로 확정)면 잘못된 이름일 때 다시 입력받는다
    fn finish_naming(&self, commit: bool, retry: bool) {
        let Some(n) = self.naming.borrow_mut().take() else {
            return;
        };
        let text = if commit {
            unsafe {
                let len = GetWindowTextLengthW(n.edit).max(0) as usize;
                let mut buf = vec![0u16; len + 1];
                let got = GetWindowTextW(n.edit, &mut buf).max(0) as usize;
                String::from_utf16_lossy(&buf[..got.min(len)])
            }
        } else {
            String::new()
        };
        let had_focus = unsafe { GetFocus() } == n.edit;
        unsafe {
            // 파괴하며 오는 EN_KILLFOCUS는 naming이 이미 비어 있어 무시된다
            let _ = DestroyWindow(n.edit);
        }
        self.rebuild_file_rows();
        if had_focus {
            unsafe {
                let _ = SetFocus(Some(self.files.hwnd));
            }
        }
        let name = text.trim();
        if name.is_empty() {
            return;
        }
        let fail = |why: &str| {
            self.set_status(1, why);
            if retry {
                self.begin_naming(n.parent, n.dir, name);
            }
        };
        if let Err(why) = validate_name(name) {
            fail(why);
            return;
        }
        let Some(dir_path) = self.model.borrow().get(n.parent).map(|p| p.path.clone()) else {
            return;
        };
        let mut file_name = name.to_owned();
        // 확장자를 안 쓰면 텍스트 파일
        if !n.dir && Path::new(name).extension().is_none() {
            file_name.push_str(".txt");
        }
        let path = dir_path.join(&file_name);
        let r = if n.dir {
            std::fs::create_dir(&path)
        } else {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map(drop)
        };
        if let Err(e) = r {
            fail(&if e.kind() == std::io::ErrorKind::AlreadyExists {
                format!("같은 이름이 이미 있습니다: {file_name}")
            } else {
                format!("만들 수 없습니다: {e}")
            });
            return;
        }
        let hidden = !n.dir
            && !self.show_all.get()
            && leaf_core::format::registry().by_path(&path).is_none();
        self.set_status(
            1,
            &if hidden {
                format!(
                    "만듦: {file_name} (지원 형식이 아니라 트리에 보이지 않음 — '모든 파일 표시')"
                )
            } else {
                format!("만듦: {file_name}")
            },
        );
        // 폴더를 다시 읽은 뒤 새 항목을 선택한다
        *self.pending_reveal.borrow_mut() = Some(path.clone());
        self.list_dir(n.parent);
        if !n.dir && editor::is_editable(&path) {
            if !self.end_edit() {
                return;
            }
            self.open_file(path.clone(), OpenKind::Navigate);
            self.start_edit(&path);
        }
    }

    // ───── TXT·MD 편집 ─────

    /// 메시지 상자. 띄운 동안 다른 확인 요청은 취소로 본다
    fn ask(&self, text: &str, flags: MESSAGEBOX_STYLE) -> Option<MESSAGEBOX_RESULT> {
        if self.modal.replace(true) {
            return None;
        }
        let t = wide(text);
        let r =
            unsafe { MessageBoxW(Some(self.hwnd), PCWSTR(t.as_ptr()), w!("LeafViewer"), flags) };
        self.modal.set(false);
        Some(r)
    }

    fn start_edit(&self, path: &Path) {
        let cur = self.editor.borrow().as_ref().map(|e| e.path.clone());
        if let Some(cur) = cur {
            if same_path(&cur, path) {
                unsafe {
                    let _ = SetFocus(Some(self.doc_focus()));
                }
                return;
            }
            if !self.end_edit() {
                return;
            }
        }
        if !editor::is_editable(path) {
            self.set_status(1, "TXT·MD 파일만 편집할 수 있습니다");
            return;
        }
        let cfg = self.config.borrow().text.clone();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let r = Editor::open(self.hwnd, path, &cfg, self.dpi(), || {
            self.ask(
                &format!(
                    "'{name}'에 인코딩을 해석할 수 없는 바이트가 있습니다.\n\
                     편집해 저장하면 그 부분이 대체 문자(�)로 바뀝니다. 계속할까요?"
                ),
                MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
            ) == Some(IDYES)
        });
        let ed = match r {
            Ok(ed) => ed,
            Err(e) => {
                self.set_status(1, &format!("편집할 수 없습니다: {e}"));
                return;
            }
        };
        ed.apply_theme(theme::current().dark);
        let hwnd = ed.hwnd;
        *self.editor.borrow_mut() = Some(ed);
        self.hide_bar();
        self.layout();
        unsafe {
            let _ = SetFocus(Some(hwnd));
        }
        self.update_edit_status();
    }

    fn update_edit_status(&self) {
        let s = self.editor.borrow().as_ref().map(|e| {
            format!(
                "편집 중{} — {} · {} · Ctrl+S 저장 · Esc 끝내기",
                if e.shown_dirty.get() { " ●" } else { "" },
                e.file_name(),
                e.encoding_label()
            )
        });
        if let Some(s) = s {
            self.set_status(0, &s);
        }
    }

    /// EN_CHANGE: 수정 표시(●)가 바뀔 때만 상태 표시줄을 고친다
    fn on_editor_change(&self) {
        let changed = match self.editor.try_borrow() {
            Ok(e) => e.as_ref().is_some_and(|e| {
                let d = e.is_modified();
                e.shown_dirty.replace(d) != d
            }),
            Err(_) => false,
        };
        if changed {
            self.update_edit_status();
        }
    }

    /// Ctrl+S. 실패하거나 취소하면 false
    fn save_edit(&self) -> bool {
        let r = match self.editor.borrow().as_ref() {
            Some(e) => e.save(false),
            None => return true,
        };
        let r = match r {
            Err(SaveError::Unmappable) => {
                let (name, label) = match self.editor.borrow().as_ref() {
                    Some(e) => (e.file_name(), e.encoding_label()),
                    None => return false,
                };
                let ok = self.ask(
                    &format!(
                        "'{name}'에 원래 인코딩({label})으로 저장할 수 없는 글자가 있습니다.\n\
                         UTF-8로 바꿔 저장할까요?"
                    ),
                    MB_YESNO | MB_ICONWARNING,
                ) == Some(IDYES);
                if !ok {
                    return false;
                }
                match self.editor.borrow().as_ref() {
                    Some(e) => e.save(true),
                    None => return false,
                }
            }
            r => r,
        };
        match r {
            Ok(()) => {
                if let Some(e) = self.editor.borrow().as_ref() {
                    e.shown_dirty.set(false);
                }
                self.update_edit_status();
                self.set_status(1, "저장했습니다");
                true
            }
            Err(SaveError::Io(e)) => {
                self.ask(
                    &format!("저장하지 못했습니다.\n\n{e}"),
                    MB_OK | MB_ICONERROR,
                );
                false
            }
            Err(SaveError::Unmappable) => false,
        }
    }

    /// 편집 끝내기. 수정했으면 저장할지 묻는다. 취소하면 false (편집을 이어 간다)
    fn end_edit(&self) -> bool {
        let info = self
            .editor
            .borrow()
            .as_ref()
            .map(|e| (e.is_modified(), e.file_name()));
        let Some((dirty, name)) = info else {
            return true;
        };
        if dirty {
            match self.ask(
                &format!("'{name}'의 변경 내용을 저장할까요?"),
                MB_YESNOCANCEL | MB_ICONQUESTION,
            ) {
                Some(IDYES) => {
                    if !self.save_edit() {
                        return false;
                    }
                }
                Some(IDNO) => {}
                _ => return false,
            }
        }
        let Some(ed) = self.editor.borrow_mut().take() else {
            return true;
        };
        let had_focus = unsafe { GetFocus() } == ed.hwnd;
        let (path, saved) = (ed.path.clone(), ed.saved.get());
        drop(ed);
        self.layout();
        if had_focus {
            unsafe {
                let _ = SetFocus(Some(self.doc));
            }
        }
        // 저장했으면 감시 이벤트를 기다리지 않고 보기를 새로 읽는다
        let showing = self.current_path().is_some_and(|p| same_path(&p, &path));
        if showing && saved && self.doc_changed_on_disk() {
            self.reload_doc();
        } else {
            let info = self.view.borrow().info.clone();
            self.set_status(0, &info);
        }
        true
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
            Ok(Some(p)) => self.set_root(&p),
            Ok(None) => {}
            Err(e) => self.set_status(0, &format!("폴더 선택 실패: {e}")),
        }
    }

    // ───── 파일 감시 (FR-F05, FR-V06, FR-A07) ─────

    /// 감시 대상: 루트(하위 포함), 루트 밖 문서의 폴더, 설정 파일 폴더
    fn update_watch(&self) {
        let mut targets = Vec::new();
        let root = self.model.borrow().root().map(|r| r.path.clone());
        if let Some(r) = &root {
            targets.push((r.clone(), true));
        }
        if let Some(p) = self.current_path()
            && let Some(dir) = p.parent()
            && !root.as_ref().is_some_and(|r| dir.starts_with(r))
        {
            targets.push((dir.to_path_buf(), false));
        }
        if let Some(dir) = self.config_path.as_ref().and_then(|p| p.parent())
            && dir.is_dir()
            && !targets
                .iter()
                .any(|(t, rec)| dir == t || (*rec && dir.starts_with(t)))
        {
            targets.push((dir.to_path_buf(), false));
        }
        let errors = self.watcher.borrow_mut().set(&targets);
        if let Some(e) = errors.first() {
            perf::record("watch_error", e);
        }
    }

    fn on_watch(&self, ev: WatchEvent) {
        let mut d = self.watch_dirty.borrow_mut();
        match ev {
            WatchEvent::Changed(ps) => {
                if d.paths.len() < 10_000 {
                    d.paths.extend(ps);
                } else {
                    d.rescan = true;
                }
            }
            WatchEvent::Rescan => d.rescan = true,
        }
        // 고정 창(트로틀): 계속 쓰이는 로그 파일도 200ms마다는 반영된다
        if !d.timer {
            d.timer = true;
            unsafe { SetTimer(Some(self.hwnd), TIMER_WATCH, WATCH_DEBOUNCE_MS, None) };
        }
    }

    fn flush_watch(&self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_WATCH);
        }
        let WatchDirty { paths, rescan, .. } = std::mem::take(&mut *self.watch_dirty.borrow_mut());
        let doc = self.current_path();
        let mut dirs: Vec<usize> = Vec::new();
        let mut doc_hit = rescan;
        let mut config_hit = false;
        if rescan {
            dirs = self.model.borrow().listed_dirs();
        }
        {
            let m = self.model.borrow();
            for p in &paths {
                if let Some(parent) = p.parent()
                    && let Some(i) = m.find_path(parent)
                    && m.nodes[i].is_dir
                    && matches!(m.nodes[i].state, LoadState::Loaded | LoadState::Failed(_))
                    && !dirs.contains(&i)
                {
                    dirs.push(i);
                }
                if doc.as_ref().is_some_and(|d| same_path(d, p)) {
                    doc_hit = true;
                }
                if self.config_path.as_ref().is_some_and(|c| same_path(c, p)) {
                    config_hit = true;
                }
            }
        }
        for d in dirs {
            self.list_dir(d);
        }
        if config_hit {
            self.reload_config();
        }
        if doc_hit && self.doc_changed_on_disk() {
            self.reload_doc();
        }
    }

    /// 감시 이벤트가 실제 내용 변경인지: 크기·수정 시각을 비교한다 (없어졌으면 true)
    fn doc_changed_on_disk(&self) -> bool {
        let Some(p) = self.current_path() else {
            return false;
        };
        let now = file_stamp(&p);
        now.is_none() || now != self.doc_stamp.get()
    }

    /// 지금 문서를 다시 읽는다. 위치는 세션 기록으로 복원된다 (FR-V06)
    fn reload_doc(&self) {
        let (path, mode, mapped) = match &self.view.borrow().content {
            Content::Md(mv) => (mv.path.clone(), MdMode::Render, false),
            Content::Text(tv) => (
                tv.path.clone(),
                if is_md_path(&tv.path) {
                    MdMode::Raw
                } else {
                    MdMode::Auto
                },
                tv.is_mapped(),
            ),
            Content::Pdf(pv) => (pv.path.clone(), MdMode::Auto, false),
            Content::Custom(cv) => (cv.path().to_path_buf(), MdMode::Auto, false),
            _ => return,
        };
        if mapped {
            // 매핑을 바로 놓아 다른 프로그램의 저장을 막지 않는다 (§4.5 mmap 주의, U-04)
            self.remember_current();
            self.set_content(Content::Message("파일이 바뀌어 다시 읽는 중…".into()));
        }
        self.md_mode.set(mode);
        self.open_file(path, OpenKind::Reload);
    }

    // ───── 뒤로 / 앞으로 (FR-V02) ─────

    fn current_pos(&self) -> Option<FilePos> {
        match &self.view.borrow().content {
            Content::Pdf(pv) => Some(pv.position()),
            Content::Text(tv) => Some(tv.position()),
            Content::Md(mv) => Some(mv.position()),
            Content::Custom(cv) => Some(cv.position()),
            _ => None,
        }
    }

    fn current_path(&self) -> Option<PathBuf> {
        match &self.view.borrow().content {
            Content::Pdf(pv) => Some(pv.path.clone()),
            Content::Text(tv) => Some(tv.path.clone()),
            Content::Md(mv) => Some(mv.path.clone()),
            Content::Custom(cv) => Some(cv.path().to_path_buf()),
            _ => None,
        }
    }

    fn push_history(&self, pos: FilePos) {
        self.history.borrow_mut().push(pos);
    }

    fn go_history(&self, back: bool) {
        if self.editor.borrow().is_some() && !self.end_edit() {
            return;
        }
        let target = {
            let mut h = self.history.borrow_mut();
            if back { h.back.pop() } else { h.fwd.pop() }
        };
        let Some(pos) = target else {
            self.set_status(
                1,
                if back {
                    "뒤로 갈 곳이 없습니다"
                } else {
                    "앞으로 갈 곳이 없습니다"
                },
            );
            return;
        };
        if let Some(cur) = self.current_pos() {
            let mut h = self.history.borrow_mut();
            if back {
                h.fwd.push(cur)
            } else {
                h.back.push(cur)
            }
        }
        // 같은 문서·같은 보기면 위치만 옮긴다
        let same = {
            let mut v = self.view.borrow_mut();
            match &mut v.content {
                Content::Pdf(pv) if same_path(&pv.path, &pos.path) && pos.view == "pdf" => {
                    pv.restore(&pos);
                    true
                }
                Content::Text(tv) if same_path(&tv.path, &pos.path) && pos.view == "txt" => {
                    tv.restore(&pos);
                    true
                }
                Content::Md(mv) if same_path(&mv.path, &pos.path) && pos.view == "md" => {
                    mv.restore(&pos);
                    true
                }
                Content::Custom(cv)
                    if same_path(cv.path(), &pos.path) && pos.view == cv.view_key() =>
                {
                    cv.restore(&pos);
                    true
                }
                _ => false,
            }
        };
        if same {
            self.scroll_cache.set(None);
            self.invalidate_doc();
            return;
        }
        if is_md_path(&pos.path) {
            self.md_mode.set(if pos.view == "txt" {
                MdMode::Raw
            } else {
                MdMode::Render
            });
        }
        let path = pos.path.clone();
        *self.pending_pos.borrow_mut() = Some(pos);
        self.reveal_in_tree(&path);
        self.open_file(path, OpenKind::History);
    }

    // ───── 세션 (FR-A02, FR-V09) ─────

    fn remember_current(&self) {
        let pos = match &self.view.borrow().content {
            Content::Pdf(pv) => Some(pv.position()),
            Content::Text(tv) => Some(tv.position()),
            Content::Md(mv) => Some(mv.position()),
            Content::Custom(cv) => Some(cv.position()),
            _ => None,
        };
        if let Some(pos) = pos {
            self.session.borrow_mut().remember(pos);
        }
    }

    /// 파일별 마지막 위치 (FR-V09). 스크롤 벤치마크는 늘 문서 처음부터 잰다.
    fn saved_pos(&self, path: &Path) -> Option<FilePos> {
        // 뒤로/앞으로로 연 문서는 그 기록 위치로
        {
            let mut pp = self.pending_pos.borrow_mut();
            if pp.as_ref().is_some_and(|p| same_path(&p.path, path)) {
                return pp.take();
            }
        }
        if self.bench.borrow().is_some() {
            return None;
        }
        self.session.borrow().position_of(path).cloned()
    }

    fn save_session(&self) {
        self.remember_current();
        let mut s = self.session.borrow_mut();
        s.root = self.model.borrow().root().map(|n| n.path.clone());
        s.expanded = self.model.borrow().expanded_paths();
        s.sidebar_w = Some(self.sidebar_w.get());
        s.sidebar_hidden = self.sidebar_hidden.get();
        s.open_file = {
            let v = self.view.borrow();
            match &v.content {
                Content::Pdf(pv) => Some(pv.path.clone()),
                Content::Text(tv) => Some(tv.path.clone()),
                Content::Md(mv) => Some(mv.path.clone()),
                Content::Custom(cv) => Some(cv.path().to_path_buf()),
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
        self.open_file(path, OpenKind::Navigate);
    }

    fn open_file(&self, path: PathBuf, kind: OpenKind) {
        // 다른 파일을 편집 중이면 먼저 끝낸다(저장 확인). 취소하면 열지 않는다
        if kind != OpenKind::Reload {
            let editing = self.editor.borrow().as_ref().map(|e| e.path.clone());
            if let Some(ep) = editing
                && !same_path(&ep, &path)
                && !self.end_edit()
            {
                self.reveal_in_tree(&ep);
                return;
            }
        }
        if kind == OpenKind::Navigate
            && let Some(cur) = self.current_pos()
            && !same_path(&cur.path, &path)
        {
            self.push_history(cur);
        }
        self.remember_current();
        let generation = self.doc_gen.get() + 1;
        self.doc_gen.set(generation);
        self.view.borrow_mut().loading = Some((generation, path.clone(), Instant::now()));
        self.set_status(0, &format!("여는 중… {}", path.display()));

        // Ctrl+U로 고른 방식은 이번 열기에만 쓴다
        let md_mode = self.md_mode.replace(MdMode::Auto);
        let md_raw = match md_mode {
            MdMode::Raw => true,
            MdMode::Render => false,
            MdMode::Auto => {
                let limit = self.config.borrow().md_render_limit();
                std::fs::metadata(&path).is_ok_and(|m| m.len() > limit)
            }
        };
        let Some(format) = leaf_core::format::registry().by_path(&path) else {
            // 확장자로 모르면 앞부분을 읽어 판별한다 (워커, §4.3 sniff)
            let waker = self.waker.clone();
            self.workers.spawn(move || {
                let format = sniff_file(&path);
                waker.send(UiMsg::Sniffed {
                    generation,
                    path,
                    format,
                });
            });
            return;
        };
        self.start_open(generation, path, format, md_raw);
    }

    /// 판별한 포맷의 보기로 연다
    fn start_open(
        &self,
        generation: u64,
        path: PathBuf,
        format: &'static leaf_doc::format::FormatInfo,
        md_raw: bool,
    ) {
        match format.view {
            ViewKind::Paged => self.pdf.borrow_mut().open(generation, &path),
            ViewKind::Flow if !md_raw => {
                let waker = self.waker.clone();
                let root = self.model.borrow().nodes.first().map(|n| n.path.clone());
                let started = Instant::now();
                self.workers.spawn(move || {
                    let result = guarded(|| mdload::load(&path, root.as_deref())).map(Box::new);
                    waker.send(UiMsg::MdLoaded {
                        generation,
                        result,
                        started,
                    });
                });
            }
            ViewKind::Flow | ViewKind::Text => {
                let waker = self.waker.clone();
                let threshold = self.config.borrow().mmap_threshold();
                self.workers.spawn(move || {
                    let t = Instant::now();
                    let result = guarded(|| textload::load(&path, threshold)).map(|l| {
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
            ViewKind::Custom => {
                let Some(cf) = formats::custom(format.id) else {
                    self.open_failed(&path, "이 형식의 보기가 빌드에 포함되지 않았습니다");
                    return;
                };
                let ctx = self.open_ctx();
                let waker = self.waker.clone();
                self.workers.spawn(move || {
                    let result = guarded(|| cf.load(&path, &ctx));
                    waker.send(UiMsg::CustomLoaded {
                        generation,
                        format: cf.info().id,
                        result,
                    });
                });
            }
        }
    }

    fn open_ctx(&self) -> OpenCtx {
        let v = self.view.borrow();
        OpenCtx::from_config(
            &self.config.borrow(),
            view_size(&v.renderer, self.doc),
            v.renderer.dpi(),
        )
    }

    /// 열기 실패: 오류 화면 (FR-V07)
    fn open_failed(&self, path: &Path, why: &str) {
        self.view.borrow_mut().loading = None;
        self.set_content(Content::Message(format!(
            "파일을 열 수 없습니다.\n\n{}\n\n{why}",
            path.display()
        )));
        self.view.borrow_mut().info.clear();
        self.set_status(0, &path.display().to_string());
    }

    fn on_sniffed(&self, generation: u64, path: PathBuf, format: Option<&'static str>) {
        if self.view.borrow().loading.as_ref().map(|l| l.0) != Some(generation) {
            return;
        }
        match format.and_then(|id| leaf_core::format::registry().by_id(id)) {
            Some(f) => self.start_open(generation, path, f, false),
            None => self.open_failed(&path, "지원하지 않는 형식입니다"),
        }
    }

    fn on_custom_loaded(
        &self,
        generation: u64,
        format: &'static str,
        result: std::result::Result<Box<dyn Any + Send>, String>,
    ) {
        let Some((path, _)) = self.take_loading(generation) else {
            return;
        };
        let loaded = match result {
            Ok(l) => l,
            Err(e) => {
                self.open_failed(&path, &e);
                return;
            }
        };
        let Some(cf) = formats::custom(format) else {
            return;
        };
        let ctx = self.open_ctx();
        let mut cv = cf.create(generation, path.clone(), loaded, &ctx);
        if let Some(pos) = self.saved_pos(&path) {
            cv.restore(&pos);
        }
        let info = cv.info();
        self.hide_bar();
        self.set_content(Content::Custom(cv));
        perf::record("custom_open", format!("format={format} {info}"));
        self.view.borrow_mut().info = info.clone();
        self.set_status(0, &info);
        self.doc_opened(path);
    }

    /// 전용 보기의 워커 작업을 돌린다. 결과는 같은 세대의 보기에만 전달된다
    fn run_custom_cmds(&self, generation: u64, cmds: Vec<CustomCmd>) {
        for c in cmds {
            match c {
                CustomCmd::Spawn(job) => {
                    let waker = self.waker.clone();
                    self.workers.spawn(move || {
                        let payload = catch_unwind(AssertUnwindSafe(job)).unwrap_or_else(|p| {
                            Box::new(format!("내부 오류: {}", panic_text(&*p)))
                        });
                        waker.send(UiMsg::CustomResult {
                            generation,
                            payload,
                        });
                    });
                }
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
        let leaving_md = matches!(old, Content::Md(_));
        let leaving_doc = matches!(
            old,
            Content::Pdf(_) | Content::Text(_) | Content::Md(_) | Content::Custom(_)
        );
        if let Content::Text(tv) = &old {
            tv.cancel_all();
        }
        // 매핑 해제 등 무거울 수 있는 정리는 빌림 밖에서
        drop(old);
        if leaving_pdf {
            self.pdf.borrow_mut().close();
        }
        if leaving_pdf || leaving_md {
            self.fill_outline(&[], false);
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
                UiMsg::Watch(ev) => self.on_watch(ev),
                UiMsg::Sniffed {
                    generation,
                    path,
                    format,
                } => self.on_sniffed(generation, path, format),
                UiMsg::CustomLoaded {
                    generation,
                    format,
                    result,
                } => self.on_custom_loaded(generation, format, result),
                UiMsg::CustomResult {
                    generation,
                    payload,
                } => {
                    let mut v = self.view.borrow_mut();
                    let dv = &mut *v;
                    if let Content::Custom(cv) = &mut dv.content
                        && cv.generation() == generation
                        && let Ok(t) = dv.renderer.target(self.doc)
                    {
                        cv.on_result(t, payload);
                    }
                    drop(v);
                    self.invalidate_doc();
                }
                UiMsg::MdLoaded {
                    generation,
                    result,
                    started,
                } => self.on_md_loaded(generation, result, started),
                UiMsg::MdImage {
                    generation,
                    id,
                    result,
                } => {
                    let mut v = self.view.borrow_mut();
                    let dv = &mut *v;
                    if let Content::Md(mv) = &mut dv.content
                        && mv.generation == generation
                        && let Ok(t) = dv.renderer.target(self.doc)
                    {
                        mv.on_image(t, id, result);
                    }
                    drop(v);
                    self.invalidate_doc();
                }
            }
        }
    }

    fn on_md_loaded(
        &self,
        generation: u64,
        result: std::result::Result<Box<LoadedMd>, String>,
        started: Instant,
    ) {
        let Some((path, _)) = self.take_loading(generation) else {
            return;
        };
        let md = match result {
            Ok(m) => *m,
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
            "md_load_ms",
            format!(
                "read={:.2} parse={:.2} probe={:.2} bytes={} units={} paras={} images={}",
                md.read_ms,
                md.parse_ms,
                md.probe_ms,
                md.size,
                md.content.units.len(),
                md.content.paras.len(),
                md.content.images.len()
            ),
        );
        let info = format!(
            "{} · MD · {} · {}",
            path.file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default(),
            md.encoding,
            human_size(md.size),
        );
        let dwrite = self.view.borrow().renderer.dwrite.clone();
        let root = self.model.borrow().root().map(|n| n.path.clone());
        let style = {
            let c = self.config.borrow();
            MdStyle {
                font: wide(&c.markdown.font),
                size_pt: c.markdown.font_size,
                max_width_em: c.markdown.max_width_em,
                image_budget: (c.memory.image_cache_mb as usize) << 20,
            }
        };
        let mut mv = MdView::new(
            generation,
            path.clone(),
            root,
            md.content,
            md.sizes,
            md.srcs,
            dwrite,
            style,
            started,
        );
        {
            let v = self.view.borrow();
            let (vw, vh) = view_size(&v.renderer, self.doc);
            mv.set_view(vw, vh, v.renderer.dpi());
        }
        let anchor = {
            let mut pa = self.pending_anchor.borrow_mut();
            match pa.take() {
                Some((p, a)) if p == path => Some(a),
                other => {
                    *pa = other;
                    None
                }
            }
        };
        match &anchor {
            Some(a) => {
                if !mv.go_anchor(a) {
                    self.set_status(1, &format!("문서에 없는 앵커: #{a}"));
                }
            }
            None => {
                if let Some(pos) = self.saved_pos(&path) {
                    mv.restore(&pos);
                }
            }
        }
        let outline = mv.outline();
        self.hide_bar();
        self.set_content(Content::Md(Box::new(mv)));
        *self.perf_text.borrow_mut() =
            format!("읽기 {:.0} + 파싱 {:.0}ms", md.read_ms, md.parse_ms);
        self.view.borrow_mut().info = info.clone();
        self.set_status(0, &info);
        self.fill_outline(&outline, true);
        self.doc_opened(path);
    }

    /// 문서가 화면에 올라온 뒤: 세션·감시 대상·변경 확인용 표식
    fn doc_opened(&self, path: PathBuf) {
        self.doc_stamp.set(file_stamp(&path));
        self.session.borrow_mut().open_file = Some(path);
        self.update_watch();
        // 편집 중엔 뒤에서 다시 읽은 보기 대신 편집 상태를 보인다
        self.update_edit_status();
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
        let mut info = format!(
            "{} · {} · {}",
            path.file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default(),
            t.doc.enc.label,
            human_size(t.size),
        );
        if is_md_path(&path) {
            // FR-M06: 큰 MD는 원문으로 연다. Ctrl+U로 렌더링
            info.push_str(if t.size > self.config.borrow().md_render_limit() {
                " · 큰 MD라 원문 표시 — Ctrl+U: 렌더링"
            } else {
                " · 원문 — Ctrl+U: 렌더링"
            });
        }
        let need_index = t.index.is_none();
        let doc = t.doc.clone();
        let dwrite = self.view.borrow().renderer.dwrite.clone();
        let style = {
            let c = self.config.borrow();
            TextStyle {
                font: wide(&c.text.font),
                size_pt: c.text.font_size,
                tab_width: c.text.tab_width as f32,
                wrap: c.text.word_wrap,
            }
        };
        let mut tv = TextView::new(
            generation,
            path.clone(),
            t.doc,
            t.size,
            t.index,
            dwrite,
            style,
            started,
        );
        tv.mapped = t.mapped;
        if let Some(pos) = self.saved_pos(&path) {
            tv.restore(&pos);
        }
        let cancel = tv.cancel.clone();
        self.hide_bar();
        self.set_content(Content::Text(Box::new(tv)));
        *self.perf_text.borrow_mut() = format!("로드 {:.0}ms", t.load_ms);
        self.view.borrow_mut().info = info.clone();
        self.set_status(0, &info);
        self.doc_opened(path);
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
                let (budget, zoom) = {
                    let c = self.config.borrow();
                    let zoom = match c.pdf.default_zoom {
                        ZoomSetting::FitWidth => ZoomMode::FitWidth,
                        ZoomSetting::FitPage => ZoomMode::FitPage,
                        ZoomSetting::Percent(z) => ZoomMode::Custom(z),
                    };
                    ((c.pdf.bitmap_cache_mb as usize) << 20, zoom)
                };
                let mut pv = PdfView::new(
                    generation,
                    path.clone(),
                    page_sizes,
                    outline,
                    started,
                    timings,
                    budget,
                    zoom,
                );
                {
                    let v = self.view.borrow();
                    let (vw, vh) = view_size(&v.renderer, self.doc);
                    pv.set_view(vw, vh, v.renderer.dpi());
                }
                if let Some(pos) = self.saved_pos(&path) {
                    pv.restore(&pos);
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
                self.fill_outline(&outline_items, false);
                self.doc_opened(path);
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
        if let Some(ms) = outcome.md_first_paint_ms {
            perf::record("md_first_paint_ms", format!("{ms:.2}"));
            *self.perf_text.borrow_mut() = format!("첫 화면 {ms:.0}ms");
        }
        self.run_cmds(outcome.generation, outcome.cmds);
        self.run_md_cmds(outcome.generation, outcome.md_cmds);
        self.run_custom_cmds(outcome.generation, outcome.custom_cmds);
        self.sync_outline_current();
        if let Some(s) = outcome.status {
            self.set_status(1, &s);
        }
        if self.bar_mode.get() == Some(BarMode::Find) {
            let info = match &self.view.borrow().content {
                Content::Pdf(pv) => pv.search_status().unwrap_or_default(),
                Content::Text(tv) => tv.search_status().unwrap_or_default(),
                Content::Md(mv) => mv.search_status().unwrap_or_default(),
                _ => String::new(),
            };
            self.bar.set_info(&info);
        }
        self.update_scrollbars();
        self.maybe_start_bench();
    }

    /// MD 목차에서 지금 읽는 제목을 선택해 둔다 (FR-M07 현재 위치 강조)
    fn sync_outline_current(&self) {
        let cur = match &mut self.view.borrow_mut().content {
            Content::Md(mv) => mv.current_heading(),
            _ => return,
        };
        {
            let mut o = self.outline_data.borrow_mut();
            if cur == o.current {
                return;
            }
            o.current = cur;
        }
        // 프로그램이 바꾼 선택은 이벤트를 내지 않는다 (이동 명령으로 처리되지 않음)
        self.outline
            .st
            .borrow_mut()
            .select_key(cur.map(|i| i as isize), true);
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
            Content::Md(mv) => Some(mv.scroll_metrics()),
            Content::Custom(cv) => {
                let ((a, b, c), (d, e, f)) = cv.scroll_metrics();
                Some((
                    (a as f64, b as f64, c as f64),
                    (d as f64, e as f64, f as f64),
                ))
            }
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
        let is_md = matches!(self.view.borrow().content, Content::Md(_));
        if is_md {
            if bar != SB_VERT {
                return;
            }
            if let Content::Md(mv) = &mut self.view.borrow_mut().content {
                let (_, vh) = mv.view_size();
                match cmd {
                    SB_LINEUP => mv.scroll_by(-40.0),
                    SB_LINEDOWN => mv.scroll_by(40.0),
                    SB_PAGEUP => mv.scroll_by(-vh * 0.9),
                    SB_PAGEDOWN => mv.scroll_by(vh * 0.9),
                    SB_THUMBTRACK | SB_THUMBPOSITION => mv.scroll_to(track as f32),
                    SB_TOP => mv.home(),
                    SB_BOTTOM => mv.end(),
                    _ => return,
                }
            }
            self.invalidate_doc();
            return;
        }
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
        let (x, y) = if bar == SB_VERT {
            (None, Some(new))
        } else {
            (Some(new), None)
        };
        match &mut self.view.borrow_mut().content {
            Content::Pdf(pv) => pv.scroll_to(x, y),
            Content::Custom(cv) => cv.scroll_to(x, y),
            _ => {}
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
                Content::Md(mv) => mv.zoom_by(1.1f32.powf(steps)),
                Content::Custom(cv) => {
                    cv.zoom_by(1.1f32.powf(steps), Some((pt.x as f32 * k, pt.y as f32 * k)))
                }
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
                Content::Md(_) if horizontal => return,
                Content::Md(mv) => mv.scroll_by(d),
                Content::Custom(cv) if horizontal => cv.scroll_by(d, 0.0),
                Content::Custom(cv) => cv.scroll_by(0.0, d),
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
                Content::Md(mv) => match vk {
                    _ if ctrl && vk.0 == b'C' as u16 => {
                        if let Some(s) = mv.copy() {
                            copy = Some(CopyJob::Ready(s));
                        }
                    }
                    _ if ctrl && vk.0 == b'A' as u16 => mv.select_all(),
                    VK_OEM_PLUS | VK_ADD if ctrl => mv.zoom_by(1.1),
                    VK_OEM_MINUS | VK_SUBTRACT if ctrl => mv.zoom_by(1.0 / 1.1),
                    _ if ctrl && (vk.0 == b'0' as u16 || vk == VK_NUMPAD0) => mv.set_zoom(1.0),
                    VK_HOME if ctrl => mv.home(),
                    VK_END if ctrl => mv.end(),
                    _ => {
                        if !mv.on_key(vk.0, shift_down()) {
                            return;
                        }
                    }
                },
                Content::Custom(cv) => {
                    if !cv.on_key(vk.0, ctrl, shift_down()) {
                        return;
                    }
                }
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
        let mut md = None;
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
                Content::Md(mv) => match msg {
                    WM_LBUTTONDOWN => mv.on_mouse_down(x, y, shift_down()),
                    WM_MOUSEMOVE => {
                        let dragging = wparam.0 & 0x0001 != 0;
                        let c = mv.on_mouse_move(x, y, dragging);
                        self.cursor.set(c);
                        if !dragging {
                            drop(v);
                            self.apply_cursor();
                            return;
                        }
                    }
                    WM_LBUTTONUP => {
                        let generation = mv.generation;
                        md = Some((generation, mv.on_mouse_up(x, y)));
                    }
                    _ => return,
                },
                Content::Custom(cv) => match msg {
                    WM_LBUTTONDOWN => cv.on_mouse_down(x, y),
                    WM_MOUSEMOVE => {
                        let dragging = wparam.0 & 0x0001 != 0;
                        let c = cv.on_mouse_move(x, y, dragging);
                        self.cursor.set(c);
                        if !dragging {
                            drop(v);
                            self.apply_cursor();
                            return;
                        }
                    }
                    WM_LBUTTONUP => cv.on_mouse_up(),
                    _ => return,
                },
                _ => return,
            }
        }
        self.run_cmds(self.current_pdf_gen(), cmds);
        if let Some((generation, c)) = md {
            self.run_md_cmds(generation, c);
        }
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
            Content::Md(mv) => mv.first_paint_ms.is_some(),
            Content::Custom(cv) => cv.first_paint_done(),
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
                Content::Md(mv) => {
                    mv.scroll_by(BENCH_STEP);
                    mv.at_end()
                }
                Content::Custom(cv) => {
                    cv.scroll_by(0.0, BENCH_STEP);
                    cv.at_end()
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
            Content::Md(mv) => (mv.image_count() as u32, 0.0, mv.bitmap_bytes()),
            Content::Custom(cv) => {
                let (n, bytes) = cv.bitmap_stats();
                (n, 0.0, bytes)
            }
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
TXT: Alt+Z 자동 줄바꿈 켜기/끄기 · Ctrl+G 줄 이동 · Ctrl+0 기본 배율 · Ctrl+Home/End 처음/끝\n\
MD: 링크 클릭으로 이동 · Ctrl+U 원문 보기 · 사이드바 \"목차\" 탭\n\
이미지: Ctrl+0 창에 맞춤 · Ctrl+1 실제 크기 · 끌어서 이동";

fn paint(v: &mut DocView, hwnd: HWND) -> Result<PaintOutcome> {
    let mut out = PaintOutcome::default();
    let (vw, vh) = view_size(&v.renderer, hwnd);
    let dpi = v.renderer.dpi();

    let welcome = if matches!(v.content, Content::Welcome | Content::Message(_)) {
        let (title, body) = match &v.content {
            // 오류 화면 (FR-V07): 첫 단락을 제목으로
            Content::Message(m) => match m.split_once("\n\n") {
                Some((t, b)) => (Some(t), b),
                None => (None, m.as_str()),
            },
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
                t.rt.Clear(Some(&rgb(t.pal.bg)));
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
                t.rt.Clear(Some(&rgb(t.pal.bg)));
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
            Content::Md(mv) => {
                t.rt.Clear(Some(&rgb(t.pal.bg)));
                mv.set_view(vw, vh, dpi);
                let first = mv.first_paint_ms.is_none();
                out.md_cmds = mv.paint(t);
                if first {
                    out.md_first_paint_ms = mv.first_paint_ms;
                }
                out.generation = mv.generation;
                out.status = Some(mv.status());
            }
            Content::Custom(cv) => {
                t.rt.Clear(Some(&rgb(t.pal.bg)));
                cv.set_view(vw, vh, dpi);
                out.custom_cmds = cv.paint(t);
                out.generation = cv.generation();
                out.status = Some(cv.status());
            }
        }
    }
    v.renderer.end_draw()?;
    Ok(out)
}

// ───────────────────────── 창 프로시저 ─────────────────────────

/// 창 프로시저 본문의 패닉을 잡는다: 기록은 패닉 훅이 하고, 복구(오류 화면)는 메시지로 미룬다.
fn guarded_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    f: impl FnOnce(&App) -> LRESULT,
) -> LRESULT {
    let Some(app) = app_of(hwnd) else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    match catch_unwind(AssertUnwindSafe(|| f(app))) {
        Ok(r) => r,
        Err(_) => {
            unsafe {
                let _ = PostMessageW(Some(app.hwnd), WM_APP_PANIC, WPARAM(0), LPARAM(0));
                if msg == WM_PAINT {
                    // 그리다 멈췄으면 무효 영역을 비워 같은 패닉이 반복되지 않게 한다
                    let _ = ValidateRect(Some(hwnd), None);
                }
            }
            LRESULT(0)
        }
    }
}

impl App {
    /// 패닉 뒤 복구: 문서를 내려놓고 오류 화면을 띄운다 (FR-V07)
    fn recover_from_panic(&self) {
        if let Ok(mut v) = self.view.try_borrow_mut() {
            v.renderer.discard_target();
            v.loading = None;
        }
        let dir = log_dir()
            .map(|d| d.join("crash.log").display().to_string())
            .unwrap_or_default();
        self.set_content(Content::Message(format!(
            "내부 오류가 발생했습니다.\n\n문서를 닫았습니다. 다른 파일을 열어 계속할 수 있습니다.\n기록: {dir}"
        )));
        self.set_status(1, "내부 오류 — 문서를 닫았습니다");
    }

    fn on_copydata(&self, lparam: LPARAM) -> LRESULT {
        let cds = unsafe { &*(lparam.0 as *const COPYDATASTRUCT) };
        if cds.dwData != COPYDATA_OPEN {
            return LRESULT(0);
        }
        let n = cds.cbData as usize / 2;
        let text = if n == 0 || cds.lpData.is_null() {
            String::new()
        } else {
            String::from_utf16_lossy(unsafe {
                std::slice::from_raw_parts(cds.lpData as *const u16, n)
            })
        };
        if !text.is_empty() {
            *self.pending_reveal.borrow_mut() = None;
            let p = PathBuf::from(text);
            self.open_path(&p);
        }
        LRESULT(1)
    }
}

extern "system" fn main_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    guarded_proc(hwnd, msg, wparam, lparam, |app| unsafe {
        match msg {
            WM_SIZE => {
                app.layout();
                LRESULT(0)
            }
            WM_ERASEBKGND => {
                // 사이드바와 문서 사이 틈 (폭 조절 손잡이)
                let hdc = HDC(wparam.0 as *mut c_void);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let p = theme::current();
                FillRect(hdc, &rc, theme::brush(p.side_bg));
                if !app.sidebar_hidden.get() {
                    let x = app.side_px(rc.right) + scale(SPLIT_DIP, app.dpi()) - 1;
                    let line = RECT {
                        left: x,
                        top: 0,
                        right: x + 1,
                        bottom: rc.bottom,
                    };
                    FillRect(hdc, &line, theme::brush(p.splitter));
                }
                LRESULT(1)
            }
            WM_SETFOCUS => {
                let _ = SetFocus(Some(if app.sidebar_hidden.get() {
                    app.doc_focus()
                } else {
                    app.current_list().hwnd
                }));
                LRESULT(0)
            }
            WM_SETCURSOR
                if loword(lparam.0 as usize) == HTCLIENT && wparam.0 == hwnd.0 as usize =>
            {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(hwnd, &mut pt);
                if app.in_splitter(pt.x) || app.split_drag.get().is_some() {
                    if let Ok(c) = LoadCursorW(None, IDC_SIZEWE) {
                        SetCursor(Some(c));
                    }
                    return LRESULT(1);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_LBUTTONDOWN => {
                let x = loword(lparam.0 as usize) as u16 as i16 as i32;
                if app.in_splitter(x) {
                    let mut rc = RECT::default();
                    let _ = GetClientRect(hwnd, &mut rc);
                    app.split_drag.set(Some(x - app.side_px(rc.right)));
                    SetCapture(hwnd);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                if let Some(off) = app.split_drag.get() {
                    let x = loword(lparam.0 as usize) as u16 as i16 as i32;
                    let dpi = app.dpi();
                    let mut rc = RECT::default();
                    let _ = GetClientRect(hwnd, &mut rc);
                    let px = (x - off).clamp(
                        scale(SIDEBAR_MIN_DIP, dpi),
                        (rc.right - scale(200, dpi)).max(0),
                    );
                    app.sidebar_w.set((px as i64 * 96 / dpi as i64) as i32);
                    app.layout();
                }
                LRESULT(0)
            }
            WM_LBUTTONUP | WM_CAPTURECHANGED => {
                if app.split_drag.take().is_some() && msg == WM_LBUTTONUP {
                    let _ = ReleaseCapture();
                }
                LRESULT(0)
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
                    TIMER_WATCH => app.flush_watch(),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_APP_WAKE => {
                app.drain_messages();
                LRESULT(0)
            }
            WM_APP_PANIC => {
                app.recover_from_panic();
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
            WM_APP_TAB => {
                app.select_tab(wparam.0);
                let _ = SetFocus(Some(app.current_list().hwnd));
                LRESULT(0)
            }
            // 마우스 X1/X2 버튼·키보드 브라우저 키 → 뒤로/앞으로 (FR-V02)
            WM_APPCOMMAND => {
                let cmd = hiword(lparam.0 as usize) & !0xF000;
                match cmd {
                    // APPCOMMAND_BROWSER_BACKWARD / FORWARD / REFRESH
                    1 => app.go_history(true),
                    2 => app.go_history(false),
                    3 => {
                        app.relist_all();
                        app.reload_doc();
                    }
                    _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
                }
                LRESULT(1)
            }
            WM_CTLCOLOREDIT => {
                // 편집기: 문서 배경·글자색
                let hdc = HDC(wparam.0 as *mut c_void);
                let p = theme::current();
                SetTextColor(hdc, theme::colorref(p.text));
                SetBkColor(hdc, theme::colorref(p.bg));
                LRESULT(theme::brush(p.bg).0 as isize)
            }
            WM_COMMAND if hiword(wparam.0) == EN_CHANGE && lparam.0 != 0 => {
                app.on_editor_change();
                LRESULT(0)
            }
            WM_APP_NAMING => {
                let still = app
                    .naming
                    .borrow()
                    .as_ref()
                    .is_some_and(|n| n.edit.0 as usize == wparam.0);
                if still {
                    app.finish_naming(true, false);
                }
                LRESULT(0)
            }
            WM_COMMAND if loword(wparam.0) as usize == ID_OPEN_FOLDER => {
                app.pick_folder();
                LRESULT(0)
            }
            WM_COPYDATA => app.on_copydata(lparam),
            WM_SETTINGCHANGE => {
                // Windows 앱 모드(라이트/다크)가 바뀜
                if lparam.0 != 0 {
                    let s = PCWSTR(lparam.0 as *const u16)
                        .to_string()
                        .unwrap_or_default();
                    if s == "ImmersiveColorSet"
                        && app.config.borrow().general.theme == Theme::System
                    {
                        app.apply_theme();
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
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
                if let Some(e) = app.editor.borrow().as_ref() {
                    e.set_font(&app.config.borrow().text, dpi);
                }
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
                // 편집 중이면 저장 확인. 취소하면 닫지 않는다
                if !app.end_edit() {
                    return LRESULT(0);
                }
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
    })
}

/// 사이드바 목록(파일 트리·목차) 창
extern "system" fn list_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    guarded_proc(hwnd, msg, wparam, lparam, |app| {
        let is_files = hwnd == app.files.hwnd;
        let list = if is_files { &app.files } else { &app.outline };
        // 새 이름 입력칸 (파일 트리의 자식 EDIT)
        if is_files {
            match msg {
                WM_CTLCOLOREDIT => unsafe {
                    let hdc = HDC(wparam.0 as *mut c_void);
                    let p = theme::current();
                    SetTextColor(hdc, theme::colorref(p.text));
                    SetBkColor(hdc, theme::colorref(p.edit_bg));
                    return LRESULT(theme::brush(p.edit_bg).0 as isize);
                },
                WM_COMMAND if hiword(wparam.0) == EN_KILLFOCUS => unsafe {
                    // 포커스가 옮겨 가는 도중이라 파괴는 나중에
                    let _ = PostMessageW(
                        Some(app.hwnd),
                        WM_APP_NAMING,
                        WPARAM(lparam.0 as usize),
                        LPARAM(0),
                    );
                    return LRESULT(0);
                },
                _ => {}
            }
        }
        let mut events = Vec::new();
        // SetScrollInfo 등으로 재진입하면(이미 빌림) 기본 처리에 맡긴다
        let handled = match (list.st.try_borrow_mut(), app.fonts.try_borrow()) {
            (Ok(mut st), Ok(fonts)) => st.on_msg(msg, wparam, lparam, &fonts, &mut events),
            _ => None,
        };
        for ev in events {
            if is_files {
                app.on_files_event(ev);
            } else {
                app.on_outline_event(ev);
            }
        }
        if is_files && matches!(msg, WM_VSCROLL | WM_MOUSEWHEEL | WM_SIZE | WM_KEYDOWN) {
            app.place_naming_edit();
        }
        handled.unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) })
    })
}

extern "system" fn doc_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    guarded_proc(hwnd, msg, wparam, lparam, |app| unsafe {
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
                        Content::Md(mv) => mv.release(),
                        Content::Custom(cv) => cv.release(),
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
    })
}

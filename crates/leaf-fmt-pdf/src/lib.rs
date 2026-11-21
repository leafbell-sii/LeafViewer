//! PDF 엔진 (SPEC §3.5).
//!
//! - `pdfium.dll`은 **첫 PDF를 열 때** 로드한다. PDF를 보지 않는 세션의 비용은 0.
//! - PDFium 호출은 **전용 스레드 하나**에서만 한다 — 이 스레드도 첫 요청 때 만든다.
//! - 결과는 `notify` 콜백으로 넘긴다. UI는 콜백 안에서 채널 전송 + `PostMessageW`만 한다.
//! - 작업 우선순위: 렌더 타일 → 쪽 텍스트/링크 → 복사 → 검색(쪽 하나씩). 새 요청이 오면
//!   진행 중인 목록을 최신 것으로 바꾼다 — 스크롤 중 화면 밖으로 나간 쪽은 그리지 않는다.
#![forbid(unsafe_code)]

mod engine;

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::thread;

use crossbeam_channel::{Sender, unbounded};
pub use leaf_doc::paged::{TileKey, TileSpec};
use leaf_doc::text::PageText;
use leaf_doc::{Link, OutlineItem, RectF};

/// 요청 세대 번호 (SPEC §4.2). 문서를 바꿀 때마다 증가시키고, 다른 세대의 결과는 버린다.
pub type Generation = u64;

/// 텍스트 위치: (쪽, 글자 인덱스). 인덱스 `usize::MAX`는 "쪽 끝".
pub type TextPos = (u32, usize);

#[derive(Debug)]
pub enum PdfEvent {
    Opened {
        generation: Generation,
        /// 쪽 크기(pt)
        page_sizes: Vec<(f32, f32)>,
        outline: Vec<OutlineItem>,
        /// 이 요청에서 DLL 바인딩에 걸린 시간 (이미 로드돼 있으면 0)
        bind_ms: f64,
        load_ms: f64,
    },
    Tile {
        generation: Generation,
        spec: TileSpec,
        stride: u32,
        /// BGRA, 불투명(흰 배경)
        bgra: Vec<u8>,
        render_ms: f64,
    },
    PageInfo {
        generation: Generation,
        page: u32,
        text: PageText,
        links: Vec<Link>,
    },
    SearchHits {
        generation: Generation,
        search_id: u64,
        page: u32,
        /// 일치 항목별 (글자 범위, 줄 단위 사각형 pt)
        hits: Vec<(Range<usize>, Vec<RectF>)>,
    },
    SearchDone {
        generation: Generation,
        search_id: u64,
    },
    Copied {
        generation: Generation,
        text: String,
    },
    Error {
        generation: Generation,
        message: String,
        /// 암호가 필요한 문서 (FR-P08, P1 — 아직 입력 대화상자는 없다)
        password: bool,
    },
}

pub(crate) enum Req {
    Open {
        generation: Generation,
        path: PathBuf,
    },
    Render {
        generation: Generation,
        tiles: Vec<TileSpec>,
    },
    PageInfo {
        generation: Generation,
        pages: Vec<u32>,
    },
    Search {
        generation: Generation,
        search_id: u64,
        query: String,
        start_page: u32,
    },
    CancelSearch,
    Copy {
        generation: Generation,
        from: TextPos,
        to: TextPos,
    },
    Close,
}

type Notify = Box<dyn Fn(PdfEvent) + Send + 'static>;

pub struct PdfEngine {
    library_path: PathBuf,
    notify: Option<Notify>,
    tx: Option<Sender<Req>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl PdfEngine {
    /// 스레드도 DLL도 아직 만들지 않는다.
    pub fn new(library_path: PathBuf, notify: impl Fn(PdfEvent) + Send + 'static) -> Self {
        Self {
            library_path,
            notify: Some(Box::new(notify)),
            tx: None,
            handle: None,
        }
    }

    pub fn open(&mut self, generation: Generation, path: &Path) {
        self.send(Req::Open {
            generation,
            path: path.to_path_buf(),
        });
    }

    /// 지금 필요한 타일 목록 전체. 이전에 보낸 목록 중 아직 안 그린 것은 버려진다.
    pub fn render(&mut self, generation: Generation, tiles: Vec<TileSpec>) {
        self.send(Req::Render { generation, tiles });
    }

    /// 쪽 텍스트·링크 요청 (지금 필요한 쪽 전체, 이전 목록을 대체).
    pub fn page_info(&mut self, generation: Generation, pages: Vec<u32>) {
        self.send(Req::PageInfo { generation, pages });
    }

    /// 문서 전체 찾기. `start_page`부터 끝까지, 이어서 처음부터 돌아온다. 결과는 쪽마다 스트리밍.
    pub fn search(&mut self, generation: Generation, search_id: u64, query: &str, start_page: u32) {
        self.send(Req::Search {
            generation,
            search_id,
            query: query.to_owned(),
            start_page,
        });
    }

    pub fn cancel_search(&mut self) {
        if self.tx.is_some() {
            self.send(Req::CancelSearch);
        }
    }

    pub fn copy(&mut self, generation: Generation, from: TextPos, to: TextPos) {
        self.send(Req::Copy {
            generation,
            from,
            to,
        });
    }

    /// 열린 문서를 닫는다 (DLL은 유지).
    pub fn close(&mut self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Req::Close);
        }
    }

    fn send(&mut self, req: Req) {
        if self.tx.is_none() {
            let (tx, rx) = unbounded();
            let notify = self.notify.take().expect("notify");
            let lib = self.library_path.clone();
            self.handle = Some(
                thread::Builder::new()
                    .name("leaf-pdfium".into())
                    .spawn(move || engine::run(lib, rx, notify))
                    .expect("spawn pdf thread"),
            );
            self.tx = Some(tx);
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(req);
        }
    }
}

impl Drop for PdfEngine {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

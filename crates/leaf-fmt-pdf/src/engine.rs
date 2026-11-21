//! PDFium 전용 스레드 본체.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

use crossbeam_channel::{Receiver, TryRecvError};
use leaf_doc::paged::TileSpec;
use leaf_doc::text::{PageText, contains_normalized};
use leaf_doc::{Dest, Link, LinkTarget, OutlineItem, RectF};
use pdfium_render::prelude::*;

use crate::{Generation, Notify, PdfEvent, Req, TextPos};

/// 열린 쪽 객체 캐시 크기 기본값. FPDF_LoadPage 반복을 줄이지만, 열린 쪽은 디코딩된 이미지를
/// 들고 있어 메모리를 쓴다 (G1). 측정 비교용으로 LEAF_PDF_PAGE_CACHE 환경 변수로 바꿀 수 있다.
const PAGE_CACHE: usize = 2;

fn page_cache_size() -> usize {
    std::env::var("LEAF_PDF_PAGE_CACHE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(PAGE_CACHE)
        .max(1)
}
const OUTLINE_MAX_ITEMS: usize = 20_000;
const OUTLINE_MAX_DEPTH: usize = 32;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn generation_of(r: &Req) -> Option<Generation> {
    match r {
        Req::Open { generation, .. }
        | Req::Render { generation, .. }
        | Req::PageInfo { generation, .. }
        | Req::Search { generation, .. }
        | Req::Copy { generation, .. } => Some(*generation),
        Req::CancelSearch | Req::Close => None,
    }
}

enum Next {
    Open(Req),
    Idle,
    Quit,
}

pub(crate) fn run(lib: PathBuf, rx: Receiver<Req>, notify: Notify) {
    // 바인딩 성공 전까지는 요청마다 다시 시도한다 (DLL을 나중에 넣어도 재시작 불필요).
    let (pdfium, mut bind_ms, mut next) = loop {
        let Ok(req) = rx.recv() else { return };
        let t = Instant::now();
        match Pdfium::bind_to_library(&lib) {
            Ok(b) => break (Pdfium::new(b), ms(t), Some(req)),
            Err(e) => {
                if let Some(generation) = generation_of(&req) {
                    notify(PdfEvent::Error {
                        generation,
                        message: format!("pdfium.dll 로드 실패 ({}): {e}", lib.display()),
                        password: false,
                    });
                }
            }
        }
    };

    loop {
        let req = match next.take() {
            Some(r) => r,
            None => match rx.recv() {
                Ok(r) => r,
                Err(_) => return,
            },
        };
        let Req::Open { generation, path } = req else {
            continue;
        };
        // 더 새 Open이 이미 와 있으면 이 문서는 건너뛴다 (빠르게 문서를 넘길 때)
        if let Some(newer) = latest_open(&rx) {
            next = Some(newer);
            continue;
        }
        let t = Instant::now();
        match pdfium.load_pdf_from_file(&path, None) {
            Ok(doc) => {
                let page_sizes: Vec<(f32, f32)> = doc
                    .pages()
                    .page_sizes()
                    .unwrap_or_default()
                    .iter()
                    .map(|r| (r.width().value, r.height().value))
                    .collect();
                let load_ms = ms(t);
                let outline = read_outline(&doc, &page_sizes);
                notify(PdfEvent::Opened {
                    generation,
                    page_sizes: page_sizes.clone(),
                    outline,
                    bind_ms,
                    load_ms,
                });
                bind_ms = 0.0;
                match serve(&doc, generation, &page_sizes, &rx, &notify) {
                    Next::Open(r) => next = Some(r),
                    Next::Idle => {}
                    Next::Quit => return,
                }
            }
            Err(e) => {
                let password = matches!(
                    e,
                    PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError)
                );
                notify(PdfEvent::Error {
                    generation,
                    message: if password {
                        "암호로 보호된 PDF입니다. (암호 입력은 아직 지원하지 않습니다)".into()
                    } else {
                        format!("PDF를 열 수 없습니다: {e}")
                    },
                    password,
                });
            }
        }
    }
}

/// 채널에 쌓인 요청 중 마지막 Open (나머지는 이전 문서 것이므로 버린다).
fn latest_open(rx: &Receiver<Req>) -> Option<Req> {
    let mut found = None;
    while let Ok(r) = rx.try_recv() {
        if matches!(r, Req::Open { .. }) {
            found = Some(r);
        }
    }
    found
}

struct SearchState {
    id: u64,
    query: String,
    order: VecDeque<u32>,
}

struct State<'a> {
    generation: Generation,
    tiles: VecDeque<TileSpec>,
    info: VecDeque<u32>,
    copies: VecDeque<(TextPos, TextPos)>,
    search: Option<SearchState>,
    pages: Vec<(u32, PdfPage<'a>)>,
    page_cache: usize,
}

impl<'a> State<'a> {
    fn idle(&self) -> bool {
        self.tiles.is_empty()
            && self.info.is_empty()
            && self.copies.is_empty()
            && self.search.is_none()
    }

    fn page(&mut self, doc: &PdfDocument<'a>, index: u32) -> Option<&PdfPage<'a>> {
        if let Some(pos) = self.pages.iter().position(|(i, _)| *i == index) {
            let p = self.pages.remove(pos);
            self.pages.insert(0, p);
        } else {
            let p = doc.pages().get(index as PdfPageIndex).ok()?;
            self.pages.insert(0, (index, p));
            self.pages.truncate(self.page_cache);
        }
        self.pages.first().map(|(_, p)| p)
    }
}

/// 문서 하나를 연 동안의 요청 처리. 새 Open/Close가 오면 돌아간다.
fn serve<'a>(
    doc: &PdfDocument<'a>,
    generation: Generation,
    sizes: &[(f32, f32)],
    rx: &Receiver<Req>,
    notify: &Notify,
) -> Next {
    let mut st = State {
        generation,
        tiles: VecDeque::new(),
        info: VecDeque::new(),
        copies: VecDeque::new(),
        search: None,
        pages: Vec::new(),
        page_cache: page_cache_size(),
    };
    loop {
        // 1) 들어온 요청 반영 (할 일이 없으면 기다린다)
        let mut incoming = Vec::new();
        if st.idle() {
            match rx.recv() {
                Ok(r) => incoming.push(r),
                Err(_) => return Next::Quit,
            }
        }
        loop {
            match rx.try_recv() {
                Ok(r) => incoming.push(r),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Next::Quit,
            }
        }
        for req in incoming {
            if generation_of(&req).is_some_and(|g| g != st.generation)
                && !matches!(req, Req::Open { .. })
            {
                continue;
            }
            match req {
                Req::Open { .. } => return Next::Open(req),
                Req::Close => return Next::Idle,
                Req::Render { tiles, .. } => st.tiles = tiles.into(),
                Req::PageInfo { pages, .. } => st.info = pages.into(),
                Req::Copy { from, to, .. } => st.copies.push_back((from, to)),
                Req::CancelSearch => st.search = None,
                Req::Search {
                    search_id,
                    query,
                    start_page,
                    ..
                } => {
                    let n = sizes.len() as u32;
                    let start = start_page.min(n.saturating_sub(1));
                    st.search = Some(SearchState {
                        id: search_id,
                        query,
                        order: (start..n).chain(0..start).collect(),
                    });
                }
            }
        }

        // 2) 한 단위만 처리하고 다시 요청을 확인한다
        if let Some(spec) = st.tiles.pop_front() {
            let t = Instant::now();
            let Some(page) = st.page(doc, spec.key.page) else {
                continue;
            };
            match render_tile(page, &spec) {
                Ok((stride, bgra)) => notify(PdfEvent::Tile {
                    generation,
                    spec,
                    stride,
                    bgra,
                    render_ms: ms(t),
                }),
                Err(e) => notify(PdfEvent::Error {
                    generation,
                    message: format!("{}쪽 렌더 실패: {e}", spec.key.page + 1),
                    password: false,
                }),
            }
        } else if let Some(index) = st.info.pop_front() {
            let Some(page) = st.page(doc, index) else {
                continue;
            };
            let text = build_text(page);
            let links = read_links(page, sizes);
            notify(PdfEvent::PageInfo {
                generation,
                page: index,
                text,
                links,
            });
        } else if let Some((from, to)) = st.copies.pop_front() {
            let text = copy_text(&mut st, doc, from, to);
            notify(PdfEvent::Copied { generation, text });
        } else if let Some(search) = st.search.as_mut() {
            let Some(index) = search.order.pop_front() else {
                let id = search.id;
                st.search = None;
                notify(PdfEvent::SearchDone {
                    generation,
                    search_id: id,
                });
                continue;
            };
            let (id, query) = (search.id, search.query.clone());
            let Some(page) = st.page(doc, index) else {
                continue;
            };
            let all = page.text().map(|t| t.all()).unwrap_or_default();
            if !contains_normalized(&all, &query) {
                continue;
            }
            let pt = build_text(page);
            let hits: Vec<_> = pt
                .find_all(&query)
                .into_iter()
                .map(|r| {
                    let rects = pt.range_rects(r.clone());
                    (r, rects)
                })
                .collect();
            if !hits.is_empty() {
                notify(PdfEvent::SearchHits {
                    generation,
                    search_id: id,
                    page: index,
                    hits,
                });
            }
        }
    }
}

fn render_tile(page: &PdfPage<'_>, spec: &TileSpec) -> Result<(u32, Vec<u8>), PdfiumError> {
    let mut bmp = PdfBitmap::empty(spec.w as Pixels, spec.h as Pixels, PdfBitmapFormat::BGRA)?;
    let full_res = spec.key.page_w_px > leaf_doc::paged::LOW_RES_W;
    let cfg = PdfRenderConfig::new()
        .set_fixed_size(spec.key.page_w_px as Pixels, spec.page_h_px as Pixels)
        .set_origin(-(spec.key.x as Pixels), -(spec.key.y as Pixels))
        .set_clear_color(PdfColor::WHITE)
        // pdfium-render 기본값은 RGBA로 뒤집는다. D2D는 BGRA를 받으므로 끈다.
        .set_reverse_byte_order(false)
        .use_lcd_text_rendering(full_res)
        // 이미지 디코딩 캐시 상한 (SPEC G1)
        .limit_render_image_cache_size(true)
        .render_form_data(true);
    page.render_into_bitmap_with_config(&mut bmp, &cfg)?;
    let bgra = bmp.as_raw_bytes();
    let stride = if spec.h > 0 {
        (bgra.len() / spec.h as usize) as u32
    } else {
        spec.w * 4
    };
    Ok((stride, bgra))
}

/// PDF 좌표(왼쪽 아래 원점, y 위로) → 쪽 왼쪽 위 원점 pt.
fn to_top_down(r: PdfRect, page_h: f32) -> RectF {
    RectF::new(
        r.left().value,
        page_h - r.top().value,
        r.right().value,
        page_h - r.bottom().value,
    )
}

fn build_text(page: &PdfPage<'_>) -> PageText {
    let h = page.height().value;
    let mut pt = PageText::default();
    let Ok(text) = page.text() else { return pt };
    for ch in text.chars().iter() {
        let c = ch.unicode_char().unwrap_or('\u{FFFD}');
        let generated = ch.is_generated().unwrap_or(false);
        let b = if generated {
            RectF::default()
        } else {
            ch.loose_bounds()
                .map(|r| to_top_down(r, h))
                .unwrap_or_default()
        };
        pt.chars.push(c);
        pt.boxes.push(b);
    }
    pt
}

fn dest_of(d: &PdfDestination<'_>, sizes: &[(f32, f32)]) -> Option<Dest> {
    let page = d.page_index().ok()?;
    if page < 0 || page as usize >= sizes.len() {
        return None;
    }
    let page_h = sizes[page as usize].1;
    let y = match d.view_settings() {
        Ok(PdfDestinationViewSettings::SpecificCoordinatesAndZoom(_, Some(y), _))
        | Ok(PdfDestinationViewSettings::FitPageHorizontallyToWindow(Some(y))) => {
            Some((page_h - y.value).clamp(0.0, page_h))
        }
        _ => None,
    };
    Some(Dest {
        page: page as u32,
        y_pt: y,
    })
}

fn action_target(a: &PdfAction<'_>, sizes: &[(f32, f32)]) -> Option<LinkTarget> {
    if let Some(u) = a.as_uri_action() {
        return u.uri().ok().map(LinkTarget::Uri);
    }
    if let Some(l) = a.as_local_destination_action() {
        return l
            .destination()
            .ok()
            .and_then(|d| dest_of(&d, sizes))
            .map(LinkTarget::Internal);
    }
    None
}

fn read_links(page: &PdfPage<'_>, sizes: &[(f32, f32)]) -> Vec<Link> {
    let h = page.height().value;
    let mut out = Vec::new();
    for link in page.links().iter() {
        let Ok(rect) = link.rect() else { continue };
        let target = match link.action() {
            Some(a) => action_target(&a, sizes),
            None => link
                .destination()
                .and_then(|d| dest_of(&d, sizes))
                .map(LinkTarget::Internal),
        };
        if let Some(target) = target {
            out.push(Link {
                rect: to_top_down(rect, h),
                target,
            });
        }
    }
    out
}

fn read_outline(doc: &PdfDocument<'_>, sizes: &[(f32, f32)]) -> Vec<OutlineItem> {
    let mut budget = OUTLINE_MAX_ITEMS;
    let mut out = Vec::new();
    let mut cur = doc.bookmarks().root();
    while let Some(b) = cur {
        if budget == 0 {
            break;
        }
        out.push(outline_item(&b, sizes, 0, &mut budget));
        cur = b.next_sibling();
    }
    out
}

fn outline_item(
    b: &PdfBookmark<'_>,
    sizes: &[(f32, f32)],
    depth: usize,
    budget: &mut usize,
) -> OutlineItem {
    *budget = budget.saturating_sub(1);
    let dest = b
        .destination()
        .and_then(|d| dest_of(&d, sizes))
        .or_else(|| match b.action().and_then(|a| action_target(&a, sizes)) {
            Some(LinkTarget::Internal(d)) => Some(d),
            _ => None,
        });
    let mut children = Vec::new();
    if depth < OUTLINE_MAX_DEPTH {
        for c in b.iter_direct_children() {
            if *budget == 0 {
                break;
            }
            children.push(outline_item(&c, sizes, depth + 1, budget));
        }
    }
    OutlineItem {
        title: b.title().unwrap_or_default().trim().to_string(),
        dest,
        children,
    }
}

fn copy_text<'a>(st: &mut State<'a>, doc: &PdfDocument<'a>, from: TextPos, to: TextPos) -> String {
    let (from, to) = if from <= to { (from, to) } else { (to, from) };
    let mut out = String::new();
    for index in from.0..=to.0 {
        let start = if index == from.0 { from.1 } else { 0 };
        let end = if index == to.0 { to.1 } else { usize::MAX };
        let Some(page) = st.page(doc, index) else {
            continue;
        };
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str("\r\n");
        }
        if start == 0 && end == usize::MAX {
            // 쪽 전체는 글자 상자 없이 빠르게
            out.push_str(&page.text().map(|t| t.all()).unwrap_or_default());
        } else {
            out.push_str(&build_text(page).text(start..end));
        }
    }
    out
}

//! PDF 엔진 (SPEC §3.5).
//!
//! - `pdfium.dll`은 **첫 PDF를 열 때** 로드한다. PDF를 보지 않는 세션의 비용은 0.
//! - PDFium 호출은 **전용 스레드 하나**에서만 한다 — 이 스레드도 첫 요청 때 만든다.
//! - 결과는 `notify` 콜백으로 넘긴다. UI는 콜백 안에서 채널 전송 + `PostMessageW`만 한다.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, unbounded};
use pdfium_render::prelude::*;

/// 요청 세대 번호 (SPEC §4.2). 문서를 바꿀 때마다 증가시키고, 다른 세대의 결과는 버린다.
pub type Generation = u64;

#[derive(Debug)]
pub enum PdfEvent {
    Opened {
        generation: Generation,
        page_count: u32,
        /// 첫 페이지 크기(pt). 레이아웃 추정용.
        page_size_pt: (f32, f32),
        /// 이 요청에서 DLL 바인딩에 걸린 시간 (이미 로드돼 있으면 0)
        bind_ms: f64,
        load_ms: f64,
    },
    Rendered {
        generation: Generation,
        page: u32,
        width: u32,
        height: u32,
        stride: u32,
        /// BGRA, 불투명(흰 배경)
        bgra: Vec<u8>,
        render_ms: f64,
    },
    Error {
        generation: Generation,
        message: String,
    },
}

enum Req {
    Open {
        generation: Generation,
        path: PathBuf,
    },
    Render {
        generation: Generation,
        page: u32,
        target_width_px: u32,
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

    pub fn is_started(&self) -> bool {
        self.tx.is_some()
    }

    pub fn open(&mut self, generation: Generation, path: &Path) {
        self.send(Req::Open {
            generation,
            path: path.to_path_buf(),
        });
    }

    pub fn render(&mut self, generation: Generation, page: u32, target_width_px: u32) {
        self.send(Req::Render {
            generation,
            page,
            target_width_px: target_width_px.max(1),
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
                    .spawn(move || pdf_thread(lib, rx, notify))
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

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// 대기 중인 요청을 모아 최신 것만 남긴다: 빠르게 문서를 넘길 때 중간 문서는 열지 않는다.
fn coalesce(first: Req, rx: &Receiver<Req>) -> Vec<Req> {
    let mut open: Option<Req> = None;
    let mut render: Option<Req> = None;
    let mut close = false;
    let mut push = |r: Req| match r {
        Req::Open { .. } => {
            open = Some(r);
            render = None;
            close = false;
        }
        Req::Render { .. } => render = Some(r),
        Req::Close => {
            open = None;
            render = None;
            close = true;
        }
    };
    push(first);
    while let Ok(r) = rx.try_recv() {
        push(r);
    }
    let mut out = Vec::new();
    if close {
        out.push(Req::Close);
    }
    out.extend(open);
    out.extend(render);
    out
}

fn pdf_thread(lib: PathBuf, rx: Receiver<Req>, notify: Notify) {
    // 바인딩 성공 전까지는 요청마다 다시 시도한다 (DLL을 나중에 넣어도 재시작 불필요).
    while let Ok(first) = rx.recv() {
        let t = Instant::now();
        match Pdfium::bind_to_library(&lib) {
            Ok(bindings) => {
                let bind_ms = ms(t);
                let pdfium = Pdfium::new(bindings);
                serve(&pdfium, first, &rx, &notify, bind_ms);
                return;
            }
            Err(e) => {
                if let Req::Open { generation, .. } | Req::Render { generation, .. } = first {
                    notify(PdfEvent::Error {
                        generation,
                        message: format!("pdfium.dll 로드 실패 ({}): {e}", lib.display()),
                    });
                }
            }
        }
    }
}

fn serve(pdfium: &Pdfium, first: Req, rx: &Receiver<Req>, notify: &Notify, mut bind_ms: f64) {
    let mut doc: Option<(Generation, PdfDocument<'_>)> = None;
    let mut next = Some(first);
    loop {
        let req = match next.take() {
            Some(r) => r,
            None => match rx.recv() {
                Ok(r) => r,
                Err(_) => return,
            },
        };
        for req in coalesce(req, rx) {
            match req {
                Req::Close => doc = None,
                Req::Open { generation, path } => {
                    doc = None;
                    let t = Instant::now();
                    match pdfium.load_pdf_from_file(&path, None) {
                        Ok(d) => {
                            let page_count = d.pages().len().max(0) as u32;
                            let page_size_pt = d
                                .pages()
                                .page_size(0)
                                .map(|r| (r.width().value, r.height().value))
                                .unwrap_or((612.0, 792.0));
                            notify(PdfEvent::Opened {
                                generation,
                                page_count,
                                page_size_pt,
                                bind_ms,
                                load_ms: ms(t),
                            });
                            bind_ms = 0.0;
                            doc = Some((generation, d));
                        }
                        Err(e) => notify(PdfEvent::Error {
                            generation,
                            message: format!("PDF를 열 수 없습니다: {e}"),
                        }),
                    }
                }
                Req::Render {
                    generation,
                    page,
                    target_width_px,
                } => {
                    let Some((g, d)) = &doc else { continue };
                    if *g != generation {
                        continue;
                    }
                    let t = Instant::now();
                    match render_page(d, page, target_width_px) {
                        Ok((width, height, stride, bgra)) => notify(PdfEvent::Rendered {
                            generation,
                            page,
                            width,
                            height,
                            stride,
                            bgra,
                            render_ms: ms(t),
                        }),
                        Err(e) => notify(PdfEvent::Error {
                            generation,
                            message: format!("{}쪽 렌더 실패: {e}", page + 1),
                        }),
                    }
                }
            }
        }
    }
}

fn render_page(
    doc: &PdfDocument<'_>,
    page: u32,
    target_width_px: u32,
) -> Result<(u32, u32, u32, Vec<u8>), PdfiumError> {
    let p = doc.pages().get(page as PdfPageIndex)?;
    let cfg = PdfRenderConfig::new()
        .set_target_width(target_width_px as Pixels)
        .set_clear_color(PdfColor::WHITE)
        .render_form_data(true);
    let bmp = p.render_with_config(&cfg)?;
    let (w, h) = (bmp.width() as u32, bmp.height() as u32);
    let bgra = bmp.as_raw_bytes();
    let stride = if h > 0 {
        (bgra.len() / h as usize) as u32
    } else {
        w * 4
    };
    Ok((w, h, stride, bgra))
}

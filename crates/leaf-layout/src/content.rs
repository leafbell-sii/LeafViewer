//! Flow IR → 레이아웃 단위(unit)와 문단(Para) 목록. 폭과 무관한 단계라 문서를 열 때 한 번만 한다.
//!
//! - **문단(Para)** = 텍스트 레이아웃 1개(SPEC §4.4 "문단 1개 = IDWriteTextLayout 1개"). 문서 순서대로
//!   번호가 매겨지므로 선택 위치 `(문단, UTF-16 인덱스)`의 대소 비교가 곧 문서 순서다.
//! - **단위(Unit)** = 가상화의 단위. 최상위 블록 하나지만, 큰 목록은 항목별로, 긴 코드 블록·표는
//!   일정 줄·행 단위로 쪼갠다 — 화면 밖의 거대한 블록 하나 때문에 레이아웃이 멈추지 않도록.

use std::collections::HashMap;

use leaf_doc::flow::{Align, Block, FlowDoc, Inline, ListItem};

pub type ParaId = u32;
pub type ImageId = u32;

/// 코드 블록을 이 줄 수마다 단위로 나눈다
pub const CODE_CHUNK_LINES: usize = 80;
/// 표를 이 행 수마다 단위로 나눈다
pub const TABLE_CHUNK_ROWS: usize = 40;
/// 표 열 폭 계산에 쓰는 행 수 (머리글 + 앞쪽 행)
pub const TABLE_SAMPLE_ROWS: usize = 100;

/// 줄 나눔 (HardBreak). DirectWrite가 문단 안 줄바꿈으로 처리한다.
pub const LINE_SEP: u16 = 0x2028;
/// 인라인 이미지 자리 (DirectWrite 인라인 객체가 차지한다)
pub const OBJ_CHAR: u16 = 0xFFFC;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParaKind {
    Body,
    Heading(u8),
    Code,
    /// 목록 글머리표·번호, 각주 번호
    Marker,
    TableHead,
    TableCell,
}

/// 인라인 서식 비트
pub mod style {
    pub const BOLD: u8 = 1;
    pub const ITALIC: u8 = 2;
    pub const STRIKE: u8 = 4;
    pub const CODE: u8 = 8;
    pub const LINK: u8 = 16;
    /// 각주 참조 (작게)
    pub const SMALL: u8 = 32;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub style: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpan {
    pub start: u32,
    pub end: u32,
    pub href: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlineObj {
    pub pos: u32,
    pub image: ImageId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Leading,
    Center,
    Trailing,
}

/// 복사할 때 이 문단 뒤에 붙일 구분자
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Join {
    Space,
    Tab,
    Line,
}

#[derive(Debug, Clone)]
pub struct Para {
    pub text: Vec<u16>,
    pub kind: ParaKind,
    pub align: TextAlign,
    /// 서식이 있는 범위만 (서식 없는 글은 기본)
    pub spans: Vec<Span>,
    pub links: Vec<LinkSpan>,
    pub objects: Vec<InlineObj>,
    /// 흐린 색 (인용, 원시 HTML, 6단계 제목)
    pub dim: bool,
    pub join: Join,
    /// 높이 추정용 폭(em): 라틴 0.55, 한글·한자 1.0
    pub em: f32,
}

impl Para {
    fn new(kind: ParaKind) -> Self {
        Self {
            text: Vec::new(),
            kind,
            align: TextAlign::Leading,
            spans: Vec::new(),
            links: Vec::new(),
            objects: Vec::new(),
            dim: false,
            join: Join::Line,
            em: 0.0,
        }
    }

    /// `idx` 위치의 링크
    pub fn link_at(&self, idx: u32) -> Option<&str> {
        self.links
            .iter()
            .find(|l| l.start <= idx && idx < l.end)
            .map(|l| l.href.as_str())
    }
}

#[derive(Debug, Clone)]
pub enum Node {
    Para(ParaId),
    Heading {
        para: ParaId,
        level: u8,
    },
    /// 세로로 쌓기. 각 자식 앞 간격(em, 첫 자식은 0)
    Stack(Vec<(f32, Node)>),
    /// 목록 항목: 왼쪽 여백에 글머리표, 오른쪽에 내용
    Item {
        marker: ParaId,
        child: Box<Node>,
    },
    Quote(Box<Node>),
    /// 코드 블록 조각. 첫·끝 조각만 위·아래 안쪽 여백을 가진다.
    Code {
        para: ParaId,
        first: bool,
        last: bool,
    },
    /// 표 조각 (`row0`: 이 조각 첫 행의 표 안 번호, 줄무늬용)
    Table {
        table: u32,
        head: Option<Vec<ParaId>>,
        rows: Vec<Vec<ParaId>>,
        row0: usize,
        first: bool,
        last: bool,
    },
    Rule,
    /// 문단에 이미지 하나만 있으면 블록 이미지로 (링크로 감싸였으면 클릭 가능)
    Image {
        image: ImageId,
        link: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Unit {
    /// 앞 단위와의 간격(em). 첫 단위는 문서 위 여백.
    pub gap: f32,
    pub node: Node,
}

#[derive(Debug, Clone)]
pub struct ImageRef {
    pub src: String,
    pub alt: String,
}

#[derive(Debug, Clone)]
pub struct HeadingRef {
    pub level: u8,
    pub text: String,
    pub anchor: String,
    pub unit: usize,
}

#[derive(Debug, Clone)]
pub struct TableInfo {
    pub cols: usize,
    pub align: Vec<Align>,
    /// 열 폭 계산 표본: 머리글 + 앞쪽 행 (행 우선)
    pub sample: Vec<Vec<ParaId>>,
}

#[derive(Debug, Clone, Default)]
pub struct Content {
    pub paras: Vec<Para>,
    pub units: Vec<Unit>,
    pub images: Vec<ImageRef>,
    /// 이미지 → 그 이미지가 든 단위들 (크기를 알게 되면 다시 레이아웃)
    pub image_units: Vec<Vec<usize>>,
    pub headings: Vec<HeadingRef>,
    /// 앵커(`#…`) → 단위. 제목과 각주(`fn-라벨`).
    pub anchors: HashMap<String, usize>,
    pub tables: Vec<TableInfo>,
    /// 문단 → 단위
    pub para_unit: Vec<u32>,
}

// ───── 간격 규칙 (em). 앞 블록의 after와 이 블록의 before 중 큰 값 ─────

const TOP_PAD: f32 = 1.0;
const AFTER: f32 = 0.85;
const TIGHT: f32 = 0.25;

fn before_of(b: &Block) -> f32 {
    match b {
        Block::Heading { level, .. } if *level <= 2 => 1.5,
        Block::Heading { .. } => 1.25,
        _ => 0.0,
    }
}

fn after_of(b: &Block) -> f32 {
    match b {
        Block::Heading { .. } => 0.6,
        _ => AFTER,
    }
}

impl Content {
    pub fn build(doc: &FlowDoc) -> Content {
        let mut b = Builder::default();
        let mut prev_after: Option<f32> = None;
        for blk in &doc.blocks {
            b.top_block(blk, &mut prev_after);
        }
        if !doc.footnotes.is_empty() {
            let gap = prev_after.map_or(TOP_PAD, |a| a.max(1.0));
            b.push_unit(gap, Node::Rule);
            for (i, f) in doc.footnotes.iter().enumerate() {
                let marker = b.marker_para(&format!("{}.", f.number), false);
                let unit = b.c.units.len();
                b.c.anchors.insert(format!("fn-{}", f.label), unit);
                let child = b.stack(&f.blocks, true, false);
                let gap = if i == 0 { 0.5 } else { TIGHT };
                b.push_unit(
                    gap,
                    Node::Item {
                        marker,
                        child: Box::new(child),
                    },
                );
            }
        }
        let mut c = b.c;
        c.image_units = vec![Vec::new(); c.images.len()];
        c.para_unit = vec![0; c.paras.len()];
        for (ui, u) in c.units.iter().enumerate() {
            let mut paras = Vec::new();
            let mut imgs = Vec::new();
            collect(&u.node, &mut paras, &mut imgs);
            for p in paras {
                c.para_unit[p as usize] = ui as u32;
                for o in &c.paras[p as usize].objects {
                    imgs.push(o.image);
                }
            }
            for im in imgs {
                let v = &mut c.image_units[im as usize];
                if v.last() != Some(&ui) {
                    v.push(ui);
                }
            }
        }
        c
    }

    /// 단위의 첫 문단 (단위 안에 문단이 없으면 None)
    pub fn first_para_of(&self, unit: usize) -> Option<ParaId> {
        let mut paras = Vec::new();
        collect(&self.units.get(unit)?.node, &mut paras, &mut Vec::new());
        paras.first().copied()
    }

    /// 두 위치 사이의 글자. 문단 사이에는 `join` 구분자, 줄 나눔은 `\n`, 인라인 이미지는 뺀다.
    pub fn text_between(&self, a: (ParaId, u32), b: (ParaId, u32)) -> String {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let mut s = String::new();
        for p in a.0..=b.0 {
            let Some(para) = self.paras.get(p as usize) else {
                break;
            };
            let len = para.text.len() as u32;
            let st = if p == a.0 { a.1.min(len) } else { 0 };
            let en = if p == b.0 { b.1.min(len) } else { len };
            let part: Vec<u16> = para.text[st as usize..en.max(st) as usize]
                .iter()
                .filter(|&&u| u != OBJ_CHAR)
                .map(|&u| if u == LINE_SEP { b'\n' as u16 } else { u })
                .collect();
            s.push_str(&String::from_utf16_lossy(&part));
            if p < b.0 {
                s.push(match para.join {
                    Join::Space => ' ',
                    Join::Tab => '\t',
                    Join::Line => '\n',
                });
            }
        }
        s
    }

    /// 마지막 위치 (전체 선택용)
    pub fn end_pos(&self) -> (ParaId, u32) {
        let n = self.paras.len().saturating_sub(1);
        (
            n as ParaId,
            self.paras.get(n).map_or(0, |p| p.text.len() as u32),
        )
    }

    /// 대소문자 무시 찾기. (문단, UTF-16 범위) 목록, 문서 순서.
    pub fn find(&self, query: &str, max: usize) -> Vec<(ParaId, u32, u32)> {
        let needle: Vec<char> = query.trim().chars().map(fold).collect();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut hay: Vec<(char, u32)> = Vec::new();
        for (pi, p) in self.paras.iter().enumerate() {
            if p.kind == ParaKind::Marker {
                continue;
            }
            hay.clear();
            let mut idx = 0u32;
            for c in char::decode_utf16(p.text.iter().copied()) {
                let c = c.unwrap_or('\u{FFFD}');
                let c2 = if c == '\u{2028}' || c == '\n' {
                    ' '
                } else {
                    fold(c)
                };
                hay.push((c2, idx));
                idx += c.len_utf16() as u32;
            }
            hay.push(('\0', idx));
            let n = needle.len();
            let mut i = 0;
            while i + n < hay.len() {
                if hay[i..i + n].iter().zip(&needle).all(|((h, _), q)| h == q) {
                    out.push((pi as ParaId, hay[i].1, hay[i + n].1));
                    if out.len() >= max {
                        return out;
                    }
                    i += n;
                } else {
                    i += 1;
                }
            }
        }
        out
    }
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn collect(n: &Node, paras: &mut Vec<ParaId>, imgs: &mut Vec<ImageId>) {
    match n {
        Node::Para(p) | Node::Heading { para: p, .. } | Node::Code { para: p, .. } => {
            paras.push(*p)
        }
        Node::Stack(v) => v.iter().for_each(|(_, c)| collect(c, paras, imgs)),
        Node::Item { marker, child } => {
            paras.push(*marker);
            collect(child, paras, imgs);
        }
        Node::Quote(c) => collect(c, paras, imgs),
        Node::Table { head, rows, .. } => {
            for r in head.iter().chain(rows.iter()) {
                paras.extend(r);
            }
        }
        Node::Rule => {}
        Node::Image { image, .. } => imgs.push(*image),
    }
}

#[derive(Default)]
struct Builder {
    c: Content,
    /// 원본 이미지 경로 → 이미지 번호 (같은 그림을 여러 번 써도 한 번만 디코딩)
    image_ids: HashMap<String, ImageId>,
}

/// 인라인 → (UTF-16 글, 서식 범위, 링크, 인라인 객체)
struct InlineText<'a> {
    b: &'a mut Builder,
    p: Para,
    style: u8,
    link: Option<String>,
}

impl InlineText<'_> {
    fn push_str(&mut self, s: &str) {
        let start = self.p.text.len() as u32;
        for c in s.chars() {
            let mut buf = [0u16; 2];
            self.p.text.extend_from_slice(c.encode_utf16(&mut buf));
            self.p.em += char_em(c);
        }
        self.mark(start);
    }

    fn push_unit(&mut self, u: u16, em: f32) {
        let start = self.p.text.len() as u32;
        self.p.text.push(u);
        self.p.em += em;
        self.mark(start);
    }

    /// 방금 넣은 [start, 끝) 범위에 현재 서식·링크를 기록
    fn mark(&mut self, start: u32) {
        let end = self.p.text.len() as u32;
        if end == start {
            return;
        }
        if self.style != 0 {
            match self.p.spans.last_mut() {
                Some(s) if s.end == start && s.style == self.style => s.end = end,
                _ => self.p.spans.push(Span {
                    start,
                    end,
                    style: self.style,
                }),
            }
        }
        if let Some(h) = &self.link {
            match self.p.links.last_mut() {
                Some(l) if l.end == start && &l.href == h => l.end = end,
                _ => self.p.links.push(LinkSpan {
                    start,
                    end,
                    href: h.clone(),
                }),
            }
        }
    }

    fn with_style(&mut self, bit: u8, v: &[Inline]) {
        let old = self.style;
        self.style |= bit;
        self.walk(v);
        self.style = old;
    }

    fn walk(&mut self, v: &[Inline]) {
        for i in v {
            match i {
                Inline::Text(t) => self.push_str(t),
                Inline::Strong(c) => self.with_style(style::BOLD, c),
                Inline::Emph(c) => self.with_style(style::ITALIC, c),
                Inline::Strike(c) => self.with_style(style::STRIKE, c),
                Inline::Code(t) | Inline::Html(t) => {
                    let old = self.style;
                    self.style |= style::CODE;
                    self.push_str(t);
                    self.style = old;
                }
                Inline::Link { href, children } => {
                    let old = (self.style, self.link.take());
                    self.style |= style::LINK;
                    self.link = Some(href.clone());
                    self.walk(children);
                    self.style = old.0;
                    self.link = old.1;
                }
                Inline::Image { src, alt } => {
                    let image = self.b.image(src, alt);
                    let pos = self.p.text.len() as u32;
                    self.p.objects.push(InlineObj { pos, image });
                    self.push_unit(OBJ_CHAR, 1.0);
                }
                Inline::SoftBreak => self.push_str(" "),
                Inline::HardBreak => self.push_unit(LINE_SEP, 0.0),
                Inline::FootnoteRef { label, number } => {
                    let old = (self.style, self.link.take());
                    self.style |= style::LINK | style::SMALL;
                    self.link = Some(format!("#fn-{label}"));
                    self.push_str(&format!("[{number}]"));
                    self.style = old.0;
                    self.link = old.1;
                }
            }
        }
    }
}

fn char_em(c: char) -> f32 {
    let u = c as u32;
    if u < 0x80 {
        0.55
    } else if (0x1100..=0x11FF).contains(&u)
        || (0x2E80..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || u >= 0x1F300
    {
        1.0
    } else {
        0.6
    }
}

/// 이미지 하나(링크로 감싼 것 포함)만 있는 문단이면 (이미지, 링크)
fn sole_image(v: &[Inline]) -> Option<(&str, &str, Option<&str>)> {
    let meaningful: Vec<&Inline> = v
        .iter()
        .filter(|i| match i {
            Inline::Text(t) => !t.trim().is_empty(),
            Inline::SoftBreak | Inline::HardBreak => false,
            _ => true,
        })
        .collect();
    match meaningful.as_slice() {
        [Inline::Image { src, alt }] => Some((src, alt, None)),
        [Inline::Link { href, children }] => match sole_image(children) {
            Some((s, a, None)) => Some((s, a, Some(href.as_str()))),
            _ => None,
        },
        _ => None,
    }
}

impl Builder {
    fn image(&mut self, src: &str, alt: &str) -> ImageId {
        if let Some(&id) = self.image_ids.get(src) {
            return id;
        }
        let id = self.c.images.len() as ImageId;
        self.c.images.push(ImageRef {
            src: src.to_string(),
            alt: alt.to_string(),
        });
        self.image_ids.insert(src.to_string(), id);
        id
    }

    fn add_para(&mut self, p: Para) -> ParaId {
        self.c.paras.push(p);
        (self.c.paras.len() - 1) as ParaId
    }

    fn inline_para(&mut self, kind: ParaKind, v: &[Inline], dim: bool) -> ParaId {
        let mut it = InlineText {
            b: self,
            p: Para::new(kind),
            style: 0,
            link: None,
        };
        it.walk(v);
        let mut p = it.p;
        p.dim = dim;
        self.add_para(p)
    }

    fn text_para(&mut self, kind: ParaKind, s: &str, dim: bool) -> ParaId {
        let mut p = Para::new(kind);
        p.text = s.encode_utf16().collect();
        p.em = s.chars().map(char_em).sum();
        p.dim = dim;
        self.add_para(p)
    }

    fn marker_para(&mut self, s: &str, dim: bool) -> ParaId {
        let mut p = Para::new(ParaKind::Marker);
        p.text = s.encode_utf16().collect();
        p.align = TextAlign::Trailing;
        p.join = Join::Space;
        p.dim = dim;
        p.em = s.chars().map(char_em).sum();
        self.add_para(p)
    }

    fn push_unit(&mut self, gap: f32, node: Node) {
        self.c.units.push(Unit { gap, node });
    }

    fn gap(prev_after: &Option<f32>, before: f32) -> f32 {
        prev_after.map_or(TOP_PAD, |a| a.max(before))
    }

    /// 최상위 블록: 목록은 항목별, 코드·표는 조각별로 단위를 만든다.
    fn top_block(&mut self, blk: &Block, prev_after: &mut Option<f32>) {
        let gap = Self::gap(prev_after, before_of(blk));
        match blk {
            Block::List {
                start,
                tight,
                items,
            } => {
                for (i, it) in items.iter().enumerate() {
                    let node = self.list_item(*start, i, it, *tight, 0, false);
                    let g = if i == 0 {
                        gap
                    } else if *tight {
                        TIGHT
                    } else {
                        AFTER
                    };
                    self.push_unit(g, node);
                }
            }
            Block::Code { text, .. } => {
                self.code_units(text, gap, false);
            }
            Block::RawHtml(text) => {
                self.code_units(text, gap, true);
            }
            Block::Table { align, head, rows } => {
                self.table_units(align, head, rows, gap);
            }
            Block::Heading { anchor, .. } => {
                let unit = self.c.units.len();
                let node = self.node(blk, false);
                if let (Node::Heading { para, level }, Block::Heading { .. }) = (&node, blk) {
                    let text = String::from_utf16_lossy(&self.c.paras[*para as usize].text)
                        .replace('\u{2028}', " ")
                        .replace('\u{FFFC}', "");
                    self.c.headings.push(HeadingRef {
                        level: *level,
                        text,
                        anchor: anchor.clone(),
                        unit,
                    });
                }
                self.c.anchors.entry(anchor.clone()).or_insert(unit);
                self.push_unit(gap, node);
            }
            _ => {
                let node = self.node(blk, false);
                self.push_unit(gap, node);
            }
        }
        *prev_after = Some(after_of(blk));
    }

    fn code_units(&mut self, text: &str, gap: f32, dim: bool) {
        let lines: Vec<&str> = text.split('\n').collect();
        let chunks: Vec<&[&str]> = lines.chunks(CODE_CHUNK_LINES).collect();
        let n = chunks.len();
        for (i, ch) in chunks.into_iter().enumerate() {
            let para = self.text_para(ParaKind::Code, &ch.join("\n"), dim);
            self.push_unit(
                if i == 0 { gap } else { 0.0 },
                Node::Code {
                    para,
                    first: i == 0,
                    last: i + 1 == n,
                },
            );
        }
    }

    fn cell_para(&mut self, kind: ParaKind, cell: &[Inline], align: Align, last: bool) -> ParaId {
        let id = self.inline_para(kind, cell, false);
        let p = &mut self.c.paras[id as usize];
        p.align = match align {
            Align::Center => TextAlign::Center,
            Align::Right => TextAlign::Trailing,
            _ => TextAlign::Leading,
        };
        p.join = if last { Join::Line } else { Join::Tab };
        id
    }

    fn row_paras(
        &mut self,
        kind: ParaKind,
        cells: &[Vec<Inline>],
        align: &[Align],
        cols: usize,
    ) -> Vec<ParaId> {
        (0..cols)
            .map(|ci| {
                let empty = Vec::new();
                let cell = cells.get(ci).unwrap_or(&empty);
                let a = align.get(ci).copied().unwrap_or_default();
                self.cell_para(kind, cell, a, ci + 1 == cols)
            })
            .collect()
    }

    fn table_paras(
        &mut self,
        align: &[Align],
        head: &[Vec<Inline>],
        rows: &[Vec<Vec<Inline>>],
    ) -> (u32, Vec<ParaId>, Vec<Vec<ParaId>>) {
        let cols = head
            .len()
            .max(rows.iter().map(Vec::len).max().unwrap_or(0))
            .max(1);
        let hp = self.row_paras(ParaKind::TableHead, head, align, cols);
        let rp: Vec<Vec<ParaId>> = rows
            .iter()
            .map(|r| self.row_paras(ParaKind::TableCell, r, align, cols))
            .collect();
        let mut sample = vec![hp.clone()];
        sample.extend(rp.iter().take(TABLE_SAMPLE_ROWS).cloned());
        let table = self.c.tables.len() as u32;
        self.c.tables.push(TableInfo {
            cols,
            align: align.to_vec(),
            sample,
        });
        (table, hp, rp)
    }

    fn table_units(
        &mut self,
        align: &[Align],
        head: &[Vec<Inline>],
        rows: &[Vec<Vec<Inline>>],
        gap: f32,
    ) {
        let (table, hp, rp) = self.table_paras(align, head, rows);
        if rp.is_empty() {
            self.push_unit(
                gap,
                Node::Table {
                    table,
                    head: Some(hp),
                    rows: Vec::new(),
                    row0: 0,
                    first: true,
                    last: true,
                },
            );
            return;
        }
        let n = rp.len().div_ceil(TABLE_CHUNK_ROWS);
        let mut head = Some(hp);
        for (i, ch) in rp.chunks(TABLE_CHUNK_ROWS).enumerate() {
            self.push_unit(
                if i == 0 { gap } else { 0.0 },
                Node::Table {
                    table,
                    head: head.take(),
                    rows: ch.to_vec(),
                    row0: i * TABLE_CHUNK_ROWS,
                    first: i == 0,
                    last: i + 1 == n,
                },
            );
        }
    }

    fn list_item(
        &mut self,
        start: Option<u64>,
        i: usize,
        it: &ListItem,
        tight: bool,
        depth: usize,
        dim: bool,
    ) -> Node {
        let label = match (it.task, start) {
            (Some(true), _) => "☑".to_string(),
            (Some(false), _) => "☐".to_string(),
            (None, Some(s)) => format!("{}.", s + i as u64),
            (None, None) => ["•", "◦", "▪"][depth % 3].to_string(),
        };
        let marker = self.marker_para(&label, dim);
        let child = self.stack_depth(&it.blocks, tight, dim, depth + 1);
        Node::Item {
            marker,
            child: Box::new(child),
        }
    }

    fn stack(&mut self, blocks: &[Block], tight: bool, dim: bool) -> Node {
        self.stack_depth(blocks, tight, dim, 0)
    }

    fn stack_depth(&mut self, blocks: &[Block], tight: bool, dim: bool, depth: usize) -> Node {
        if blocks.is_empty() {
            let p = self.text_para(ParaKind::Body, "", dim);
            return Node::Para(p);
        }
        let mut v: Vec<(f32, Node)> = Vec::new();
        let mut prev_after: Option<f32> = None;
        for b in blocks {
            let g = match prev_after {
                None => 0.0,
                Some(_) if tight => TIGHT,
                Some(a) => a.max(before_of(b)),
            };
            let node = match b {
                Block::List {
                    start,
                    tight: t,
                    items,
                } => {
                    let inner: Vec<(f32, Node)> = items
                        .iter()
                        .enumerate()
                        .map(|(i, it)| {
                            let gg = if i == 0 {
                                0.0
                            } else if *t {
                                TIGHT
                            } else {
                                AFTER
                            };
                            (gg, self.list_item(*start, i, it, *t, depth, dim))
                        })
                        .collect();
                    Node::Stack(inner)
                }
                _ => self.node(b, dim),
            };
            v.push((g, node));
            prev_after = Some(after_of(b));
        }
        if v.len() == 1 {
            v.pop().unwrap().1
        } else {
            Node::Stack(v)
        }
    }

    /// 블록 하나 → 노드 (중첩 위치용. 쪼개지 않는다)
    fn node(&mut self, b: &Block, dim: bool) -> Node {
        match b {
            Block::Heading { level, inlines, .. } => {
                let para = self.inline_para(ParaKind::Heading(*level), inlines, dim || *level == 6);
                Node::Heading {
                    para,
                    level: *level,
                }
            }
            Block::Paragraph(v) => {
                if let Some((src, alt, link)) = sole_image(v) {
                    let image = self.image(src, alt);
                    return Node::Image {
                        image,
                        link: link.map(str::to_string),
                    };
                }
                Node::Para(self.inline_para(ParaKind::Body, v, dim))
            }
            Block::List { .. } => self.stack_depth(std::slice::from_ref(b), false, dim, 0),
            Block::Quote(v) => Node::Quote(Box::new(self.stack(v, false, true))),
            Block::Code { text, .. } => Node::Code {
                para: self.text_para(ParaKind::Code, text, dim),
                first: true,
                last: true,
            },
            Block::RawHtml(text) => Node::Code {
                para: self.text_para(ParaKind::Code, text, true),
                first: true,
                last: true,
            },
            Block::Table { align, head, rows } => {
                let (table, hp, rp) = self.table_paras(align, head, rows);
                Node::Table {
                    table,
                    head: Some(hp),
                    rows: rp,
                    row0: 0,
                    first: true,
                    last: true,
                }
            }
            Block::Rule => Node::Rule,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leaf_doc::flow::Inline as I;

    fn t(s: &str) -> I {
        I::Text(s.into())
    }

    fn para(v: Vec<I>) -> Block {
        Block::Paragraph(v)
    }

    #[test]
    fn inline_styles_links_objects() {
        let doc = FlowDoc {
            blocks: vec![para(vec![
                t("a"),
                I::Strong(vec![t("bc")]),
                I::Link {
                    href: "x.md".into(),
                    children: vec![t("d"), I::Code("e".into())],
                },
                I::Image {
                    src: "p.png".into(),
                    alt: "".into(),
                },
                I::HardBreak,
                t("f"),
            ])],
            ..Default::default()
        };
        let c = Content::build(&doc);
        let p = &c.paras[0];
        assert_eq!(String::from_utf16_lossy(&p.text), "abcde\u{FFFC}\u{2028}f");
        assert_eq!(
            p.spans,
            vec![
                Span {
                    start: 1,
                    end: 3,
                    style: style::BOLD
                },
                Span {
                    start: 3,
                    end: 4,
                    style: style::LINK
                },
                Span {
                    start: 4,
                    end: 5,
                    style: style::LINK | style::CODE
                },
            ]
        );
        assert_eq!(
            p.links,
            vec![LinkSpan {
                start: 3,
                end: 5,
                href: "x.md".into()
            }]
        );
        assert_eq!(p.objects, vec![InlineObj { pos: 5, image: 0 }]);
        assert_eq!(c.image_units, vec![vec![0]]);
        assert_eq!(p.link_at(4), Some("x.md"));
        assert_eq!(c.text_between((0, 0), (0, 8)), "abcde\nf");
    }

    #[test]
    fn units_split_lists_code_tables() {
        let code: String = (0..(CODE_CHUNK_LINES * 2 + 5))
            .map(|i| format!("l{i}\n"))
            .collect();
        let rows: Vec<Vec<Vec<I>>> = (0..(TABLE_CHUNK_ROWS + 1))
            .map(|i| vec![vec![t(&i.to_string())]])
            .collect();
        let doc = FlowDoc {
            blocks: vec![
                Block::Heading {
                    level: 1,
                    inlines: vec![t("H")],
                    anchor: "h".into(),
                },
                Block::List {
                    start: Some(3),
                    tight: true,
                    items: vec![
                        ListItem {
                            task: None,
                            blocks: vec![para(vec![t("x")])],
                        },
                        ListItem {
                            task: Some(true),
                            blocks: vec![para(vec![t("y")])],
                        },
                    ],
                },
                Block::Code {
                    lang: None,
                    text: code.trim_end().into(),
                },
                Block::Table {
                    align: vec![Align::Right],
                    head: vec![vec![t("h")]],
                    rows,
                },
                para(vec![I::Link {
                    href: "u".into(),
                    children: vec![I::Image {
                        src: "a.png".into(),
                        alt: "A".into(),
                    }],
                }]),
            ],
            ..Default::default()
        };
        let c = Content::build(&doc);
        // 제목 1 + 항목 2 + 코드 3조각 + 표 2조각 + 이미지 1
        assert_eq!(c.units.len(), 9);
        assert_eq!(c.units[0].gap, TOP_PAD);
        assert_eq!(c.units[2].gap, TIGHT);
        assert!(matches!(
            c.units[3].node,
            Node::Code {
                first: true,
                last: false,
                ..
            }
        ));
        assert!(matches!(
            c.units[5].node,
            Node::Code {
                first: false,
                last: true,
                ..
            }
        ));
        assert_eq!(c.units[4].gap, 0.0);
        assert!(matches!(
            &c.units[6].node,
            Node::Table {
                head: Some(_),
                row0: 0,
                ..
            }
        ));
        assert!(matches!(
            &c.units[7].node,
            Node::Table {
                head: None,
                row0: TABLE_CHUNK_ROWS,
                last: true,
                ..
            }
        ));
        assert!(matches!(&c.units[8].node, Node::Image { link: Some(l), .. } if l == "u"));
        assert_eq!(c.headings.len(), 1);
        assert_eq!(c.anchors["h"], 0);
        let Node::Item { marker, .. } = &c.units[1].node else {
            panic!()
        };
        assert_eq!(
            String::from_utf16_lossy(&c.paras[*marker as usize].text),
            "3."
        );
        let Node::Item { marker, .. } = &c.units[2].node else {
            panic!()
        };
        assert_eq!(
            String::from_utf16_lossy(&c.paras[*marker as usize].text),
            "☑"
        );
        // 문단은 문서 순서, para_unit이 단위를 가리킨다
        assert!(c.para_unit.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(c.tables[0].sample.len(), 1 + TABLE_CHUNK_ROWS + 1);
        assert_eq!(
            c.paras[c.tables[0].sample[0][0] as usize].align,
            TextAlign::Trailing
        );
    }

    #[test]
    fn find_and_copy_across_paras() {
        let doc = FlowDoc {
            blocks: vec![
                para(vec![t("Hello World")]),
                Block::List {
                    start: None,
                    tight: true,
                    items: vec![ListItem {
                        task: None,
                        blocks: vec![para(vec![t("world 한글")])],
                    }],
                },
            ],
            ..Default::default()
        };
        let c = Content::build(&doc);
        assert_eq!(c.find("WORLD", 100), vec![(0, 6, 11), (2, 0, 5)]);
        assert_eq!(c.find("한글", 100), vec![(2, 6, 8)]);
        assert_eq!(c.find("world", 1).len(), 1);
        // 글머리표 문단은 공백으로 이어진다
        assert_eq!(c.text_between((0, 6), (2, 5)), "World\n• world");
        assert_eq!(c.end_pos(), (2, 8));
    }

    #[test]
    fn footnotes_become_units_with_anchor() {
        let doc = FlowDoc {
            blocks: vec![para(vec![
                t("a"),
                I::FootnoteRef {
                    label: "n".into(),
                    number: 1,
                },
            ])],
            footnotes: vec![leaf_doc::flow::Footnote {
                label: "n".into(),
                number: 1,
                blocks: vec![para(vec![t("note")])],
            }],
            ..Default::default()
        };
        let c = Content::build(&doc);
        assert_eq!(c.units.len(), 3); // 문단, 구분선, 각주
        assert_eq!(c.anchors["fn-n"], 2);
        assert_eq!(c.paras[0].links[0].href, "#fn-n");
        assert_eq!(String::from_utf16_lossy(&c.paras[0].text), "a[1]");
    }
}

//! MD 포맷 (SPEC §4.1 `leaf-fmt-markdown`, FR-M01·M02·M05): pulldown-cmark → Flow IR.
//! UI·Win32 비의존. 레이아웃은 `leaf-layout`이 맡는다.
#![forbid(unsafe_code)]

mod linkify;

use std::collections::HashMap;

use leaf_doc::flow::{Align, Block, Cell, FlowDoc, Footnote, Inline, ListItem, slugify};
use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, LinkType, MetadataBlockKind, Options, Parser, Tag, TagEnd,
};

/// 이 크기를 넘는 MD는 TXT 보기로 열고 렌더링은 사용자가 고른다 (FR-M06, 설정 `[markdown] render_size_limit_mb`).
pub const RENDER_SIZE_LIMIT: u64 = 8 << 20;

/// 파일 바이트 → 문자열. BOM·UTF-8 검사·`chardetng` 순으로 인코딩을 감지한다 (TXT와 같은 규칙).
/// 돌려주는 두 번째 값은 상태 표시줄용 인코딩 이름.
pub fn decode(bytes: &[u8], ansi_cp: u32) -> (String, &'static str) {
    let te = leaf_fmt_text::encoding::detect(bytes, ansi_cp);
    let body = &bytes[te.bom_len.min(bytes.len())..];
    let (s, _) = te.enc.decode_without_bom_handling(body);
    (s.into_owned(), te.label)
}

pub fn parse(src: &str) -> FlowDoc {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let mut b = Builder {
        events: Parser::new_ext(src, opts).peekable(),
        anchors: HashMap::new(),
        footnote_numbers: HashMap::new(),
        footnote_defs: Vec::new(),
        front_matter: None,
    };
    let blocks = b.blocks(None);
    let mut footnotes: Vec<Footnote> = std::mem::take(&mut b.footnote_defs)
        .into_iter()
        .map(|(label, blocks)| {
            let number = b.footnote_number(&label);
            Footnote {
                label,
                number,
                blocks,
            }
        })
        .collect();
    footnotes.sort_by_key(|f| f.number);
    FlowDoc {
        blocks,
        footnotes,
        front_matter: b.front_matter,
    }
}

struct Builder<'a> {
    events: std::iter::Peekable<Parser<'a>>,
    /// slug → 이미 쓴 횟수 (중복 제목에 `-1`, `-2` …)
    anchors: HashMap<String, u32>,
    footnote_numbers: HashMap<String, u32>,
    footnote_defs: Vec<(String, Vec<Block>)>,
    front_matter: Option<String>,
}

fn is_inline_event(e: &Event) -> bool {
    match e {
        Event::Text(_)
        | Event::Code(_)
        | Event::InlineHtml(_)
        | Event::InlineMath(_)
        | Event::FootnoteReference(_)
        | Event::SoftBreak
        | Event::HardBreak => true,
        Event::Start(t) => matches!(
            t,
            Tag::Emphasis
                | Tag::Strong
                | Tag::Strikethrough
                | Tag::Superscript
                | Tag::Subscript
                | Tag::Link { .. }
                | Tag::Image { .. }
        ),
        _ => false,
    }
}

fn align_of(a: &Alignment) -> Align {
    match a {
        Alignment::None => Align::None,
        Alignment::Left => Align::Left,
        Alignment::Center => Align::Center,
        Alignment::Right => Align::Right,
    }
}

impl Builder<'_> {
    fn footnote_number(&mut self, label: &str) -> u32 {
        let next = self.footnote_numbers.len() as u32 + 1;
        *self
            .footnote_numbers
            .entry(label.to_string())
            .or_insert(next)
    }

    fn unique_anchor(&mut self, text: &str) -> String {
        let base = slugify(text);
        let n = self.anchors.entry(base.clone()).or_insert(0);
        let a = if *n == 0 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        *n += 1;
        a
    }

    /// `end`를 만날 때까지(또는 입력 끝까지) 블록을 모은다. 블록 자리에 바로 나오는 인라인
    /// (빈 줄 없는 목록 항목의 글)은 문단 하나로 묶는다.
    fn blocks(&mut self, end: Option<TagEnd>) -> Vec<Block> {
        let mut out = Vec::new();
        while let Some(ev) = self.events.peek() {
            if let Event::End(e) = ev
                && Some(*e) == end
            {
                self.events.next();
                break;
            }
            if is_inline_event(ev) {
                let inl = self.inlines_until_block();
                if !inl.is_empty() {
                    out.push(Block::Paragraph(inl));
                }
                continue;
            }
            let ev = self.events.next().unwrap();
            match ev {
                Event::Start(tag) => self.block(tag, &mut out),
                Event::Rule => out.push(Block::Rule),
                Event::Html(h) => out.push(Block::RawHtml(h.to_string())),
                Event::DisplayMath(m) => out.push(Block::Code {
                    lang: Some("math".into()),
                    text: m.to_string(),
                }),
                Event::TaskListMarker(_) | Event::End(_) => {}
                _ => {}
            }
        }
        out
    }

    fn block(&mut self, tag: Tag, out: &mut Vec<Block>) {
        match tag {
            Tag::Paragraph => {
                let inl = self.inlines(TagEnd::Paragraph);
                if !inl.is_empty() {
                    out.push(Block::Paragraph(inl));
                }
            }
            Tag::Heading { level, .. } => {
                let inlines = self.inlines(TagEnd::Heading(level));
                let anchor = self.unique_anchor(&Inline::plain_text(&inlines));
                out.push(Block::Heading {
                    level: level as u8,
                    inlines,
                    anchor,
                });
            }
            Tag::BlockQuote(kind) => {
                let inner = self.blocks(Some(TagEnd::BlockQuote(kind)));
                out.push(Block::Quote(inner));
            }
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => {
                        let l = l.split_whitespace().next().unwrap_or("").to_string();
                        (!l.is_empty()).then_some(l)
                    }
                    CodeBlockKind::Indented => None,
                };
                let mut text = self.raw_text(TagEnd::CodeBlock);
                if text.ends_with('\n') {
                    text.pop();
                }
                out.push(Block::Code { lang, text });
            }
            Tag::HtmlBlock => {
                let text = self.raw_text(TagEnd::HtmlBlock);
                let t = text.trim_end();
                // 주석만 있는 블록은 GitHub처럼 보이지 않게 한다
                let only_comment = t.trim_start().starts_with("<!--")
                    && t.ends_with("-->")
                    && t.matches("-->").count() == 1;
                if !t.is_empty() && !only_comment {
                    out.push(Block::RawHtml(t.to_string()));
                }
            }
            Tag::List(start) => {
                let ordered = start.is_some();
                let mut items = Vec::new();
                let mut tight = true;
                loop {
                    match self.events.next() {
                        Some(Event::Start(Tag::Item)) => {
                            let mut task = None;
                            if let Some(Event::TaskListMarker(c)) = self.events.peek() {
                                task = Some(*c);
                                self.events.next();
                            }
                            // 빈 줄 있는 목록은 항목 내용이 문단으로 감싸져 온다
                            if matches!(self.events.peek(), Some(Event::Start(Tag::Paragraph))) {
                                tight = false;
                            }
                            let blocks = self.blocks(Some(TagEnd::Item));
                            items.push(ListItem { task, blocks });
                        }
                        Some(Event::End(TagEnd::List(o))) if o == ordered => break,
                        None => break,
                        _ => {}
                    }
                }
                out.push(Block::List {
                    start,
                    tight,
                    items,
                });
            }
            Tag::FootnoteDefinition(label) => {
                let blocks = self.blocks(Some(TagEnd::FootnoteDefinition));
                self.footnote_defs.push((label.to_string(), blocks));
            }
            Tag::Table(aligns) => {
                let align: Vec<Align> = aligns.iter().map(align_of).collect();
                let mut head = Vec::new();
                let mut rows = Vec::new();
                loop {
                    match self.events.next() {
                        Some(Event::Start(Tag::TableHead)) => {
                            head = self.cells(TagEnd::TableHead);
                        }
                        Some(Event::Start(Tag::TableRow)) => {
                            rows.push(self.cells(TagEnd::TableRow));
                        }
                        Some(Event::End(TagEnd::Table)) | None => break,
                        _ => {}
                    }
                }
                out.push(Block::Table { align, head, rows });
            }
            Tag::MetadataBlock(MetadataBlockKind::YamlStyle) => {
                let t = self.raw_text(TagEnd::MetadataBlock(MetadataBlockKind::YamlStyle));
                self.front_matter = Some(t);
            }
            Tag::MetadataBlock(k) => {
                self.raw_text(TagEnd::MetadataBlock(k));
            }
            Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Item
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell => {
                // 켜지 않은 확장이거나 문맥 밖 태그: 내용만 살린다
                let end = tag.to_end();
                out.extend(self.blocks(Some(end)));
            }
            other => {
                // 블록 자리의 인라인 태그는 is_inline_event에서 걸러지므로 여기 오지 않는다
                let end = other.to_end();
                self.skip_to(end);
            }
        }
    }

    fn cells(&mut self, end: TagEnd) -> Vec<Cell> {
        let mut cells = Vec::new();
        loop {
            match self.events.next() {
                Some(Event::Start(Tag::TableCell)) => cells.push(self.inlines(TagEnd::TableCell)),
                Some(Event::End(e)) if e == end => break,
                None => break,
                _ => {}
            }
        }
        cells
    }

    /// `end`까지의 글자를 그대로 이어 붙인다 (코드 블록, HTML 블록, front matter).
    fn raw_text(&mut self, end: TagEnd) -> String {
        let mut s = String::new();
        for ev in self.events.by_ref() {
            match ev {
                Event::End(e) if e == end => break,
                Event::Text(t) | Event::Html(t) | Event::Code(t) | Event::InlineHtml(t) => {
                    s.push_str(&t)
                }
                _ => {}
            }
        }
        s
    }

    fn skip_to(&mut self, end: TagEnd) {
        let mut depth = 0usize;
        for ev in self.events.by_ref() {
            match ev {
                Event::Start(t) if t.to_end() == end => depth += 1,
                Event::End(e) if e == end => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
    }

    /// 블록 시작·끝을 만날 때까지의 인라인 (빈 줄 없는 목록 항목)
    fn inlines_until_block(&mut self) -> Vec<Inline> {
        let mut out = Vec::new();
        while let Some(ev) = self.events.peek() {
            if !is_inline_event(ev) {
                break;
            }
            let ev = self.events.next().unwrap();
            self.inline(ev, &mut out);
        }
        linkify::linkify(&mut out);
        out
    }

    fn inlines(&mut self, end: TagEnd) -> Vec<Inline> {
        let mut out = self.inlines_raw(end);
        linkify::linkify(&mut out);
        out
    }

    fn inlines_raw(&mut self, end: TagEnd) -> Vec<Inline> {
        let mut out = Vec::new();
        while let Some(ev) = self.events.next() {
            if let Event::End(e) = &ev
                && *e == end
            {
                break;
            }
            self.inline(ev, &mut out);
        }
        out
    }

    fn inline(&mut self, ev: Event, out: &mut Vec<Inline>) {
        match ev {
            Event::Text(t) => push_text(out, &t),
            Event::Code(c) => out.push(Inline::Code(c.to_string())),
            Event::InlineMath(m) => out.push(Inline::Code(format!("${m}$"))),
            Event::SoftBreak => out.push(Inline::SoftBreak),
            Event::HardBreak => out.push(Inline::HardBreak),
            Event::FootnoteReference(label) => {
                let number = self.footnote_number(&label);
                out.push(Inline::FootnoteRef {
                    label: label.to_string(),
                    number,
                });
            }
            Event::InlineHtml(h) => {
                let t = h.trim();
                let lower = t.to_ascii_lowercase();
                if matches!(lower.as_str(), "<br>" | "<br/>" | "<br />") {
                    out.push(Inline::HardBreak);
                } else if !(t.starts_with("<!--") && t.ends_with("-->")) {
                    out.push(Inline::Html(h.to_string()));
                }
            }
            Event::Start(tag) => match tag {
                Tag::Emphasis => {
                    let c = self.inlines_raw(TagEnd::Emphasis);
                    out.push(Inline::Emph(c));
                }
                Tag::Strong => {
                    let c = self.inlines_raw(TagEnd::Strong);
                    out.push(Inline::Strong(c));
                }
                Tag::Strikethrough => {
                    let c = self.inlines_raw(TagEnd::Strikethrough);
                    out.push(Inline::Strike(c));
                }
                Tag::Superscript | Tag::Subscript => {
                    let end = tag.to_end();
                    out.extend(self.inlines_raw(end));
                }
                Tag::Link {
                    link_type,
                    dest_url,
                    ..
                } => {
                    let children = self.inlines_raw(TagEnd::Link);
                    let href = if link_type == LinkType::Email
                        && !dest_url.to_ascii_lowercase().starts_with("mailto:")
                    {
                        format!("mailto:{dest_url}")
                    } else {
                        dest_url.to_string()
                    };
                    out.push(Inline::Link { href, children });
                }
                Tag::Image { dest_url, .. } => {
                    let alt = Inline::plain_text(&self.inlines_raw(TagEnd::Image));
                    out.push(Inline::Image {
                        src: dest_url.to_string(),
                        alt,
                    });
                }
                other => {
                    let end = other.to_end();
                    self.skip_to(end);
                }
            },
            _ => {}
        }
    }
}

/// 이웃한 글자 조각은 합친다 (pulldown-cmark는 `&amp;` 등에서 글자를 쪼개 보낸다).
fn push_text(out: &mut Vec<Inline>, t: &str) {
    if let Some(Inline::Text(prev)) = out.last_mut() {
        prev.push_str(t);
    } else {
        out.push(Inline::Text(t.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Inline {
        Inline::Text(s.into())
    }

    #[test]
    fn headings_paragraphs_and_anchors() {
        let d = parse("# 제목 하나\n\n본문 *기울임* **굵게** ~~취소~~ `코드`\n\n# 제목 하나\n");
        assert_eq!(d.blocks.len(), 3);
        match &d.blocks[0] {
            Block::Heading { level, anchor, .. } => {
                assert_eq!(*level, 1);
                assert_eq!(anchor, "제목-하나");
            }
            b => panic!("{b:?}"),
        }
        assert_eq!(
            d.blocks[1],
            Block::Paragraph(vec![
                t("본문 "),
                Inline::Emph(vec![t("기울임")]),
                t(" "),
                Inline::Strong(vec![t("굵게")]),
                t(" "),
                Inline::Strike(vec![t("취소")]),
                t(" "),
                Inline::Code("코드".into()),
            ])
        );
        match &d.blocks[2] {
            Block::Heading { anchor, .. } => assert_eq!(anchor, "제목-하나-1"),
            b => panic!("{b:?}"),
        }
    }

    #[test]
    fn lists_tight_loose_task_nested() {
        let d = parse("- a\n- [x] b\n  1. c\n  2. d\n\n10. x\n\n11. y\n");
        match &d.blocks[0] {
            Block::List {
                start: None,
                tight: true,
                items,
            } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].blocks, vec![Block::Paragraph(vec![t("a")])]);
                assert_eq!(items[1].task, Some(true));
                assert!(matches!(
                    &items[1].blocks[1],
                    Block::List { start: Some(1), .. }
                ));
            }
            b => panic!("{b:?}"),
        }
        assert!(matches!(
            &d.blocks[1],
            Block::List {
                start: Some(10),
                tight: false,
                ..
            }
        ));
    }

    #[test]
    fn table_code_quote_rule_html() {
        let src = "| a | b |\n|:-|-:|\n| 1 | **2** |\n\n```rust title\nfn main() {}\n```\n\n> q\n\n---\n\n<div>x</div>\n\n<!-- hidden -->\n";
        let d = parse(src);
        match &d.blocks[0] {
            Block::Table { align, head, rows } => {
                assert_eq!(align, &vec![Align::Left, Align::Right]);
                assert_eq!(head, &vec![vec![t("a")], vec![t("b")]]);
                assert_eq!(rows[0][1], vec![Inline::Strong(vec![t("2")])]);
            }
            b => panic!("{b:?}"),
        }
        assert_eq!(
            d.blocks[1],
            Block::Code {
                lang: Some("rust".into()),
                text: "fn main() {}".into()
            }
        );
        assert_eq!(
            d.blocks[2],
            Block::Quote(vec![Block::Paragraph(vec![t("q")])])
        );
        assert_eq!(d.blocks[3], Block::Rule);
        assert_eq!(d.blocks[4], Block::RawHtml("<div>x</div>".into()));
        assert_eq!(d.blocks.len(), 5);
    }

    #[test]
    fn links_images_autolinks_footnotes() {
        let src = "[문서](other.md#sec) ![그림 *a*](img/a.png) <https://x.y> 그리고 https://example.com/a_(b). 메일 <me@x.y>\n\n각주[^n] 다시[^n]\n\n[^n]: 설명\n";
        let d = parse(src);
        let Block::Paragraph(p) = &d.blocks[0] else {
            panic!()
        };
        assert_eq!(
            p[0],
            Inline::Link {
                href: "other.md#sec".into(),
                children: vec![t("문서")]
            }
        );
        assert_eq!(
            p[2],
            Inline::Image {
                src: "img/a.png".into(),
                alt: "그림 a".into()
            }
        );
        assert!(p.contains(&Inline::Link {
            href: "https://example.com/a_(b)".into(),
            children: vec![t("https://example.com/a_(b)")]
        }));
        assert!(p.contains(&Inline::Link {
            href: "mailto:me@x.y".into(),
            children: vec![t("me@x.y")]
        }));
        let Block::Paragraph(p2) = &d.blocks[1] else {
            panic!()
        };
        assert_eq!(
            p2[1],
            Inline::FootnoteRef {
                label: "n".into(),
                number: 1
            }
        );
        assert_eq!(d.footnotes.len(), 1);
        assert_eq!(d.footnotes[0].number, 1);
        assert_eq!(
            d.footnotes[0].blocks,
            vec![Block::Paragraph(vec![t("설명")])]
        );
    }

    #[test]
    fn front_matter_and_br() {
        let d = parse("---\ntitle: x\n---\n\na<br>b\n");
        assert_eq!(d.front_matter.as_deref(), Some("title: x\n"));
        assert_eq!(
            d.blocks,
            vec![Block::Paragraph(vec![t("a"), Inline::HardBreak, t("b")])]
        );
    }

    #[test]
    fn broken_input_does_not_panic() {
        for src in [
            "",
            "```\nunclosed",
            "| a |\n|-|\n| 1 | 2 | 3 |",
            "* [ ]",
            "[^x]\n\n[^x]: [^x]",
            "<div>\n\n*a",
            "> > > - - - 1. # x",
            "\u{0}\u{FEFF}*",
        ] {
            let _ = parse(src);
        }
    }

    #[test]
    fn decode_detects_encodings() {
        let (s, l) = decode(b"\xEF\xBB\xBF# hi", 949);
        assert_eq!((s.as_str(), l), ("# hi", "UTF-8 BOM"));
        let (s, _) = decode("# 한글".as_bytes(), 949);
        assert_eq!(s, "# 한글");
        let (s, l) = decode(
            b"# \xc7\xd1\xb1\xdb \xb9\xae\xbc\xad\xc0\xd4\xb4\xcf\xb4\xd9",
            949,
        );
        assert_eq!((s.as_str(), l), ("# 한글 문서입니다", "CP949"));
    }
}

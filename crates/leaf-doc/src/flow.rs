//! Flow IR (SPEC §4.4): 리플로우되는 블록 문서. MD(향후 csv·epub 등)가 이 모델로 변환된다.
//! 레이아웃은 `leaf-layout`이 맡는다. 여기는 순수 데이터만 둔다.

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlowDoc {
    pub blocks: Vec<Block>,
    /// 각주 정의. 본문 끝에 모아 표시한다 (GFM).
    pub footnotes: Vec<Footnote>,
    /// YAML front matter 원문. v1은 표시하지 않는다 (FR-M08 기본 숨김).
    pub front_matter: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Footnote {
    pub label: String,
    /// 참조 순서대로 매긴 번호(1부터). 참조되지 않은 정의는 정의 순서로 뒤에 붙는다.
    pub number: u32,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading {
        level: u8,
        inlines: Vec<Inline>,
        /// 문서 안에서 유일한 앵커 (GitHub 방식 slug, `#앵커` 링크 대상)
        anchor: String,
    },
    Paragraph(Vec<Inline>),
    List {
        /// 순서 목록의 시작 번호. `None`이면 글머리표 목록.
        start: Option<u64>,
        /// 빈 줄 없는 목록: 항목 사이 간격이 좁다
        tight: bool,
        items: Vec<ListItem>,
    },
    Quote(Vec<Block>),
    Code {
        lang: Option<String>,
        text: String,
    },
    Table {
        align: Vec<Align>,
        head: Vec<Cell>,
        rows: Vec<Vec<Cell>>,
    },
    Rule,
    /// v1: 렌더링하지 않고 원문을 코드처럼 표시 (FR-M05)
    RawHtml(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListItem {
    /// 작업 목록 체크 상태 (`- [x]`)
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

pub type Cell = Vec<Inline>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    None,
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Text(String),
    Strong(Vec<Inline>),
    Emph(Vec<Inline>),
    Strike(Vec<Inline>),
    Code(String),
    Link {
        href: String,
        children: Vec<Inline>,
    },
    Image {
        src: String,
        alt: String,
    },
    SoftBreak,
    HardBreak,
    /// 각주 참조: 정의의 번호와 라벨
    FootnoteRef {
        label: String,
        number: u32,
    },
    /// 인라인 원시 HTML. 원문 그대로 표시한다.
    Html(String),
}

impl Inline {
    /// 서식을 뺀 글자만 (제목 slug, 이미지 alt 등)
    pub fn plain_text(inlines: &[Inline]) -> String {
        let mut s = String::new();
        fn walk(v: &[Inline], s: &mut String) {
            for i in v {
                match i {
                    Inline::Text(t) | Inline::Code(t) => s.push_str(t),
                    Inline::Strong(c) | Inline::Emph(c) | Inline::Strike(c) => walk(c, s),
                    Inline::Link { children, .. } => walk(children, s),
                    Inline::Image { alt, .. } => s.push_str(alt),
                    Inline::SoftBreak | Inline::HardBreak => s.push(' '),
                    Inline::FootnoteRef { number, .. } => s.push_str(&number.to_string()),
                    Inline::Html(_) => {}
                }
            }
        }
        walk(inlines, &mut s);
        s
    }
}

/// GitHub 방식 제목 slug: 소문자, 글자·숫자·`-`·`_`만 남기고 공백은 `-`.
/// 한글 등 유니코드 글자는 그대로 둔다. 중복은 호출하는 쪽이 `-1`, `-2`를 붙인다.
pub fn slugify(text: &str) -> String {
    let mut s = String::with_capacity(text.len());
    for c in text.trim().chars() {
        if c.is_alphanumeric() || c == '_' || c == '-' {
            s.extend(c.to_lowercase());
        } else if c == ' ' {
            s.push('-');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_rules() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  설치 방법 (Windows)  "), "설치-방법-windows");
        assert_eq!(slugify("a_b-c  d"), "a_b-c--d");
        assert_eq!(slugify("C++ & Rust"), "c--rust");
    }

    #[test]
    fn plain_text_flattens() {
        let v = vec![
            Inline::Text("a ".into()),
            Inline::Strong(vec![Inline::Emph(vec![Inline::Text("b".into())])]),
            Inline::Code("c".into()),
            Inline::SoftBreak,
            Inline::Link {
                href: "x".into(),
                children: vec![Inline::Text("d".into())],
            },
        ];
        assert_eq!(Inline::plain_text(&v), "a bc d");
    }
}

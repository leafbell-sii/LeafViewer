//! TXT 포맷 (SPEC §4.1 `leaf-fmt-text`, FR-T): 인코딩 감지, 희소 줄 인덱스, 행 탐색·디코딩, 찾기.
//! UI·Win32 비의존. 큰 파일의 메모리 매핑은 `Source`를 구현해 leaf-win이 넘긴다.
#![forbid(unsafe_code)]

pub mod doc;
pub mod edit;
pub mod encoding;
pub mod index;
pub mod search;

pub use doc::{MAX_ROW_BYTES, Pos, Row, Source, TextDoc};
pub use encoding::TextEncoding;
pub use index::LineIndex;

/// 포맷 레지스트리 등록 정보 (SPEC §4.3). 텍스트는 서명이 없어 내용으로 판별하지 않는다.
pub static FORMAT: leaf_doc::format::FormatInfo = leaf_doc::format::FormatInfo {
    id: "text",
    name: "텍스트",
    extensions: &["txt"],
    view: leaf_doc::format::ViewKind::Text,
    sniff: leaf_doc::format::no_sniff,
};

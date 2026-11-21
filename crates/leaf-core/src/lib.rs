//! LeafViewer 코어 — UI 비의존 순수 Rust (SPEC §3.1, §4.1).
#![forbid(unsafe_code)]

pub mod config;
pub mod fs_tree;
pub mod lru;
pub mod natural;
pub mod perf;
pub mod session;
pub mod tree;
pub mod watch;
pub mod worker;

/// v1 기본 지원 확장자. M1 이후 포맷 레지스트리(§4.3)로 대체된다.
pub const SUPPORTED_EXTENSIONS: &[&str] = &["pdf", "md", "markdown", "txt"];

/// 확장자(소문자, 점 제외)로 지원 여부 판정.
pub fn is_supported_ext(ext: &str) -> bool {
    SUPPORTED_EXTENSIONS
        .iter()
        .any(|e| e.eq_ignore_ascii_case(ext))
}

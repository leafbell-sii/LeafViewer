//! LeafViewer 코어 — UI 비의존 순수 Rust (SPEC §3.1, §4.1).
#![forbid(unsafe_code)]

pub mod config;
pub mod format;
pub mod fs_tree;
pub mod lru;
pub mod natural;
pub mod perf;
pub mod session;
pub mod tree;
pub mod watch;
pub mod worker;

/// 확장자(점 제외, 대소문자 무시)로 지원 여부 판정 — 설치된 포맷 레지스트리(§4.3)에 묻는다.
pub fn is_supported_ext(ext: &str) -> bool {
    format::registry().by_ext(ext).is_some()
}

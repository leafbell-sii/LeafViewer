//! 문서 모델 공통 타입 (SPEC §4.1 `leaf-doc`). UI·Win32 비의존.
#![forbid(unsafe_code)]

pub mod geom;
pub mod paged;
pub mod text;

pub use geom::RectF;

/// 목차(책갈피) 항목 (FR-P07).
#[derive(Debug, Clone, Default)]
pub struct OutlineItem {
    pub title: String,
    pub dest: Option<Dest>,
    pub children: Vec<OutlineItem>,
}

/// 문서 내부 목적지: 쪽 + 선택적 세로 위치(쪽 위에서부터 pt).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dest {
    pub page: u32,
    pub y_pt: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinkTarget {
    Internal(Dest),
    /// 외부 URI. 여는 쪽에서 `http`/`https`/`mailto`만 허용한다 (SPEC §6.3).
    Uri(String),
}

/// 쪽 위의 링크 영역 (pt, 쪽 왼쪽 위 원점).
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub rect: RectF,
    pub target: LinkTarget,
}

/// 외부 링크로 열어도 되는 스킴인지 (SPEC §6.3).
pub fn is_allowed_uri(uri: &str) -> bool {
    let lower = uri.trim().to_ascii_lowercase();
    ["http://", "https://", "mailto:"]
        .iter()
        .any(|s| lower.starts_with(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_whitelist() {
        assert!(is_allowed_uri("https://example.com"));
        assert!(is_allowed_uri(" HTTP://x"));
        assert!(is_allowed_uri("mailto:a@b.c"));
        assert!(!is_allowed_uri("file:///C:/Windows/System32/calc.exe"));
        assert!(!is_allowed_uri("javascript:alert(1)"));
        assert!(!is_allowed_uri("C:\\evil.exe"));
    }
}

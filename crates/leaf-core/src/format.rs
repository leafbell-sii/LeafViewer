//! 포맷 레지스트리 (SPEC §4.3). 컴파일 타임 등록 — 앱이 시작할 때 포맷 크레이트들의
//! `FORMAT` 기술자를 모아 [`install`]한다. 어떤 포맷을 넣을지는 feature flag로 정한다.
//!
//! 트리 필터(FR-F03), MD 링크(앱 안에서 열지 탐색기에서 선택할지), 문서 열기 분기가 모두
//! 여기서 확장자를 묻는다. 새 포맷은 기술자 하나를 등록하면 이 모든 곳에 반영된다.

use std::path::Path;
use std::sync::OnceLock;

use leaf_doc::format::FormatInfo;

/// 내용 판별에 읽을 파일 앞부분 크기
pub const SNIFF_LEN: usize = 1024;

#[derive(Debug, Default)]
pub struct Registry {
    formats: Vec<&'static FormatInfo>,
}

impl Registry {
    pub fn new(formats: Vec<&'static FormatInfo>) -> Self {
        Self { formats }
    }

    pub fn formats(&self) -> &[&'static FormatInfo] {
        &self.formats
    }

    /// 확장자(점 제외, 대소문자 무시). 둘 이상이 같은 확장자를 주장하면 먼저 등록한 쪽
    pub fn by_ext(&self, ext: &str) -> Option<&'static FormatInfo> {
        self.formats.iter().copied().find(|f| f.has_ext(ext))
    }

    pub fn by_path(&self, p: &Path) -> Option<&'static FormatInfo> {
        self.by_ext(p.extension()?.to_str()?)
    }

    pub fn by_id(&self, id: &str) -> Option<&'static FormatInfo> {
        self.formats.iter().copied().find(|f| f.id == id)
    }

    /// 파일 앞부분으로 판별 (확장자를 모를 때)
    pub fn sniff(&self, head: &[u8]) -> Option<&'static FormatInfo> {
        self.formats.iter().copied().find(|f| (f.sniff)(head))
    }
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

/// 앱 시작 때 한 번. 두 번째 호출은 무시한다(먼저 넣은 것이 이긴다).
pub fn install(r: Registry) {
    let _ = REGISTRY.set(r);
}

/// 설치된 레지스트리. 설치 전이면 빈 레지스트리(아무 확장자도 지원하지 않음).
pub fn registry() -> &'static Registry {
    static EMPTY: OnceLock<Registry> = OnceLock::new();
    REGISTRY
        .get()
        .unwrap_or_else(|| EMPTY.get_or_init(Registry::default))
}

#[cfg(test)]
mod tests {
    use super::*;
    use leaf_doc::format::{ViewKind, no_sniff};

    static TXT: FormatInfo = FormatInfo {
        id: "text",
        name: "텍스트",
        extensions: &["txt"],
        view: ViewKind::Text,
        sniff: no_sniff,
    };
    static PDF: FormatInfo = FormatInfo {
        id: "pdf",
        name: "PDF",
        extensions: &["pdf"],
        view: ViewKind::Paged,
        sniff: |h| h.starts_with(b"%PDF-"),
    };

    #[test]
    fn lookup() {
        let r = Registry::new(vec![&TXT, &PDF]);
        assert_eq!(r.by_ext("TXT").unwrap().id, "text");
        assert_eq!(r.by_path(Path::new(r"C:\a\b.Pdf")).unwrap().id, "pdf");
        assert!(r.by_path(Path::new(r"C:\a\noext")).is_none());
        assert!(r.by_ext("png").is_none());
        assert_eq!(r.sniff(b"%PDF-1.7\n").unwrap().id, "pdf");
        assert!(r.sniff(b"hello").is_none());
        assert_eq!(r.by_id("pdf").unwrap().name, "PDF");
    }
}

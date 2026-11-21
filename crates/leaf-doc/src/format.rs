//! 포맷 기술자 (SPEC §4.3). 포맷 크레이트마다 `FORMAT` 상수 하나를 내보내고,
//! 앱이 그것들을 모아 레지스트리(`leaf-core::format::Registry`)를 만든다.
//!
//! 여기에는 Win32에 의존하지 않는 정보(이름·확장자·내용 판별·보기 종류)만 둔다.
//! 실제로 여는 코드는 보기 종류에 따라 `leaf-win`이 고른다.

/// 이 포맷을 어느 보기로 여는지 (SPEC §4.3 `OpenedDoc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    /// 줄 기반·대용량 (txt)
    Text,
    /// 블록 IR·리플로우 (md)
    Flow,
    /// 고정 페이지 (pdf)
    Paged,
    /// 그 밖의 전용 보기. 앱의 공통 경로(`CustomView`)로 붙는다 — 예: 이미지
    Custom,
}

#[derive(Debug)]
pub struct FormatInfo {
    /// 안정적인 식별자 ("markdown"). 세션·로그에 쓴다
    pub id: &'static str,
    /// 사람이 읽는 이름 (상태 표시줄)
    pub name: &'static str,
    /// 소문자, 점 없이
    pub extensions: &'static [&'static str],
    pub view: ViewKind,
    /// 파일 앞부분으로 판별 (확장자가 없거나 모르는 파일용). 판별하지 않는 포맷은 `no_sniff`
    pub sniff: fn(&[u8]) -> bool,
}

/// 내용으로 판별하지 않는 포맷용 (텍스트처럼 서명이 없는 경우)
pub fn no_sniff(_: &[u8]) -> bool {
    false
}

impl FormatInfo {
    pub fn has_ext(&self, ext: &str) -> bool {
        self.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext))
    }
}

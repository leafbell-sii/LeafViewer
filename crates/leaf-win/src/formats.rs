//! 포맷 등록 (SPEC §4.3). 새 포맷은 여기 한 줄과 Cargo feature 하나로 들어온다.
//!
//! - TXT·MD·PDF: 앱이 직접 아는 보기(`ViewKind::Text/Flow/Paged`)
//! - 그 밖: `CustomFormat`을 구현해 [`custom_formats`]에 넣는다 (`ViewKind::Custom`)

use leaf_core::format::Registry;
use leaf_doc::format::{FormatInfo, ViewKind};

use crate::custom::CustomFormat;

/// 전용 보기 포맷 목록 (feature flag로 포함 여부를 정한다)
static CUSTOM: &[&dyn CustomFormat] = &[
    #[cfg(feature = "fmt-image")]
    &crate::imageview::IMAGE,
];

fn custom_formats() -> &'static [&'static dyn CustomFormat] {
    CUSTOM
}

/// 앱 시작 때 한 번: 레지스트리를 설치한다
pub fn install() {
    let mut infos: Vec<&'static FormatInfo> = vec![
        &leaf_fmt_pdf::FORMAT,
        &leaf_fmt_markdown::FORMAT,
        &leaf_fmt_text::FORMAT,
    ];
    infos.extend(custom_formats().iter().map(|f| f.info()));
    debug_assert!(
        infos.iter().all(|f| (f.view == ViewKind::Custom)
            == custom_formats().iter().any(|c| c.info().id == f.id))
    );
    leaf_core::format::install(Registry::new(infos));
}

/// `ViewKind::Custom` 포맷의 여는 코드
pub fn custom(id: &str) -> Option<&'static dyn CustomFormat> {
    custom_formats().iter().copied().find(|f| f.info().id == id)
}

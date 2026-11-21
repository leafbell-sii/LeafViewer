//! 이미지 포맷 (SPEC M5 확장성 검증, §12 Q6). UI·Win32 비의존 부분만 둔다:
//! 형식 판별, EXIF 방향 변환, 확대·스크롤 배치. 디코딩은 `leaf-win`이 WIC로 한다.
//!
//! 이 크레이트가 핵심 코드에 요구하는 것은 레지스트리 등록([`FORMAT`])뿐이다.
//! 보기는 앱의 공통 경로(`ViewKind::Custom`)에 붙는다.
#![forbid(unsafe_code)]

mod layout;
mod orient;

pub use layout::{ImageLayout, PAD, Zoom};
pub use orient::Orientation;

use leaf_doc::format::{FormatInfo, ViewKind};

/// 포맷 레지스트리 등록 정보 (SPEC §4.3). WIC가 OS 코덱으로 읽는 형식만 넣는다
/// (WebP는 Windows 11 기본 코덱). SVG는 WIC가 못 읽는다(미해결 U-14).
pub static FORMAT: FormatInfo = FormatInfo {
    id: "image",
    name: "이미지",
    extensions: &[
        "png", "jpg", "jpeg", "jpe", "jfif", "gif", "bmp", "dib", "webp", "tif", "tiff", "ico",
    ],
    view: ViewKind::Custom,
    sniff,
};

/// 파일 앞부분의 서명으로 이미지인지 판별한다.
pub fn sniff(h: &[u8]) -> bool {
    let at = |i: usize, s: &[u8]| h.len() >= i + s.len() && &h[i..i + s.len()] == s;
    at(0, b"\x89PNG\r\n\x1a\n")
        || at(0, b"\xFF\xD8\xFF")
        || at(0, b"GIF87a")
        || at(0, b"GIF89a")
        || (at(0, b"RIFF") && at(8, b"WEBP"))
        || at(0, b"II*\0")
        || at(0, b"MM\0*")
        // BMP: "BM"만으로는 약하다 → DIB 헤더 크기까지 확인
        || (at(0, b"BM")
            && h.len() >= 18
            && matches!(
                u32::from_le_bytes([h[14], h[15], h[16], h[17]]),
                12 | 40 | 52 | 56 | 64 | 108 | 124
            ))
        // ICO: 예약 0, 형식 1, 개수 1 이상
        || (at(0, b"\0\0\x01\0") && h.len() >= 6 && u16::from_le_bytes([h[4], h[5]]) > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_signatures() {
        assert!(sniff(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"));
        assert!(sniff(b"\xFF\xD8\xFF\xE0\0\x10JFIF"));
        assert!(sniff(b"GIF89a\x01\0"));
        assert!(sniff(b"RIFF\x24\0\0\0WEBPVP8 "));
        assert!(sniff(b"II*\0\x08\0\0\0"));
        let mut bmp = b"BM\0\0\0\0\0\0\0\0\x36\0\0\0".to_vec();
        bmp.extend_from_slice(&40u32.to_le_bytes());
        assert!(sniff(&bmp));
        assert!(sniff(b"\0\0\x01\0\x01\0"));

        assert!(!sniff(b"BMW is a car"));
        assert!(!sniff(b"RIFF\x24\0\0\0WAVEfmt "));
        assert!(!sniff(b"%PDF-1.7"));
        assert!(!sniff(b"\0\0\x01\0\0\0"));
        assert!(!sniff(b""));
    }

    #[test]
    fn registered_extensions() {
        assert!(FORMAT.has_ext("JPG"));
        assert!(FORMAT.has_ext("webp"));
        assert!(!FORMAT.has_ext("svg"));
    }
}

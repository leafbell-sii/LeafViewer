//! 편집용 변환: 파일 바이트 ↔ 편집 문자열 (EDIT 컨트롤은 CRLF 줄바꿈 UTF-16).
//!
//! 저장할 때는 원래 인코딩·BOM·줄바꿈을 그대로 지킨다. 원래 인코딩으로 표현할 수 없는 글자가
//! 있으면 `encode`가 `None`을 돌려주고, 호출자가 UTF-8로 저장할지 묻는다.

use crate::encoding::{self, Family, TextEncoding};

/// 파일의 줄바꿈 방식. 섞여 있으면 처음 나온 것을 따른다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Crlf,
    Lf,
    Cr,
}

/// 편집할 내용
#[derive(Debug)]
pub struct EditText {
    /// 줄바꿈을 모두 CRLF로 바꾼 본문 (EDIT 컨트롤용)
    pub text: String,
    pub enc: TextEncoding,
    pub eol: Eol,
    /// 디코딩 중 잘못된 바이트가 있었다 (그대로 저장하면 대체 문자로 바뀐다)
    pub had_errors: bool,
    /// NUL 글자가 있다 (EDIT 컨트롤은 NUL에서 문자열을 끊는다)
    pub has_nul: bool,
}

pub fn detect_eol(s: &str) -> Eol {
    match s.as_bytes().iter().position(|&b| b == b'\r' || b == b'\n') {
        Some(i) if s.as_bytes()[i] == b'\n' => Eol::Lf,
        Some(i) if s.as_bytes().get(i + 1) == Some(&b'\n') => Eol::Crlf,
        Some(_) => Eol::Cr,
        // 줄바꿈이 없는 파일(새 파일 등)은 Windows 관례
        None => Eol::Crlf,
    }
}

/// 모든 줄바꿈(CRLF·LF·CR)을 `eol`로 바꾼다
pub fn normalize(s: &str, eol: Eol) -> String {
    let nl = match eol {
        Eol::Crlf => "\r\n",
        Eol::Lf => "\n",
        Eol::Cr => "\r",
    };
    let mut out = String::with_capacity(s.len() + s.len() / 32);
    let mut rest = s;
    while let Some(i) = rest.find(['\r', '\n']) {
        out.push_str(&rest[..i]);
        out.push_str(nl);
        let b = rest.as_bytes();
        let skip = if b[i] == b'\r' && b.get(i + 1) == Some(&b'\n') {
            2
        } else {
            1
        };
        rest = &rest[i + skip..];
    }
    out.push_str(rest);
    out
}

/// 파일 바이트 → 편집 문자열. 인코딩은 보기와 같은 방식으로 감지한다 (FR-T01).
pub fn decode(bytes: &[u8], ansi_cp: u32) -> EditText {
    let enc = encoding::detect(bytes, ansi_cp);
    let body = &bytes[enc.bom_len.min(bytes.len())..];
    let (s, had_errors) = enc.enc.decode_without_bom_handling(body);
    EditText {
        eol: detect_eol(&s),
        has_nul: s.contains('\0'),
        text: normalize(&s, Eol::Crlf),
        enc,
        had_errors,
    }
}

/// 편집 문자열 → 파일 바이트 (줄바꿈 `eol`, 원래 BOM 포함). 표현할 수 없는 글자가 있으면 None
pub fn encode(te: &TextEncoding, text: &str, eol: Eol) -> Option<Vec<u8>> {
    let s = normalize(text, eol);
    let bom = te.bom_len > 0;
    let mut out = Vec::with_capacity(s.len() + 3);
    match te.family {
        Family::Utf8 => {
            if bom {
                out.extend_from_slice(b"\xEF\xBB\xBF");
            }
            out.extend_from_slice(s.as_bytes());
        }
        Family::Utf16Le | Family::Utf16Be => {
            let le = te.family == Family::Utf16Le;
            let put = |out: &mut Vec<u8>, u: u16| {
                out.extend_from_slice(&if le { u.to_le_bytes() } else { u.to_be_bytes() })
            };
            out.reserve(s.len());
            if bom {
                put(&mut out, 0xFEFF);
            }
            for u in s.encode_utf16() {
                put(&mut out, u);
            }
        }
        _ => {
            let (b, _, unmappable) = te.enc.encode(&s);
            if unmappable {
                return None;
            }
            out.extend_from_slice(&b);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eol_detect_and_normalize() {
        assert_eq!(detect_eol("a\nb\r\n"), Eol::Lf);
        assert_eq!(detect_eol("a\r\nb\n"), Eol::Crlf);
        assert_eq!(detect_eol("a\rb"), Eol::Cr);
        assert_eq!(detect_eol("abc"), Eol::Crlf);
        assert_eq!(normalize("a\nb\r\nc\rd", Eol::Crlf), "a\r\nb\r\nc\r\nd");
        assert_eq!(normalize("a\r\nb\r\n", Eol::Lf), "a\nb\n");
        assert_eq!(normalize("a\r\n\r\nb", Eol::Cr), "a\r\rb");
        assert_eq!(normalize("한\r\n글", Eol::Lf), "한\n글");
    }

    #[test]
    fn utf8_roundtrip_keeps_lf_and_bom() {
        let src = "\u{FEFF}첫 줄\n둘째 줄\n".as_bytes();
        let t = decode(src, 949);
        assert_eq!(t.text, "첫 줄\r\n둘째 줄\r\n");
        assert_eq!(t.eol, Eol::Lf);
        assert!(!t.had_errors && !t.has_nul);
        assert_eq!(encode(&t.enc, &t.text, t.eol).unwrap(), src);
    }

    #[test]
    fn utf16_roundtrip() {
        for le in [true, false] {
            let mut src = Vec::new();
            for u in "\u{FEFF}가a\r\nb".encode_utf16() {
                src.extend_from_slice(&if le { u.to_le_bytes() } else { u.to_be_bytes() });
            }
            let t = decode(&src, 949);
            assert_eq!(t.text, "가a\r\nb");
            assert_eq!(encode(&t.enc, &t.text, t.eol).unwrap(), src);
        }
    }

    #[test]
    fn cp949_roundtrip_and_unmappable() {
        let (src, _, _) =
            encoding_rs::EUC_KR.encode("한국어 문서입니다. 인코딩 확인용 문장입니다.\n");
        let t = decode(&src, 949);
        assert_eq!(t.enc.enc, encoding_rs::EUC_KR);
        assert_eq!(encode(&t.enc, &t.text, t.eol).unwrap(), &src[..]);
        assert!(encode(&t.enc, "이모지 😀", t.eol).is_none());
    }

    #[test]
    fn nul_is_flagged() {
        assert!(decode(b"a\0b", 949).has_nul);
    }
}

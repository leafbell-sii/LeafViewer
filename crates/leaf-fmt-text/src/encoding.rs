//! 인코딩 감지와 행 단위 디코딩 (FR-T01).
//!
//! 감지 순서: BOM → (BOM 없는 UTF-16 휴리스틱) → UTF-8 유효성 → `chardetng` → 시스템 ANSI 코드페이지.
//! 대용량 파일은 앞 1MB + 가운데·끝 256KB 표본만 본다.

use encoding_rs::Encoding;

/// 글자 경계 계산 방식. 행을 독립적으로 디코딩하려면 행 시작이 항상 글자 경계여야 한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Utf8,
    Utf16Le,
    Utf16Be,
    /// 1바이트 = 1글자 (windows-125x, ISO-8859-x …)
    Single,
    Dbcs(Dbcs),
}

/// 2바이트 계열. 둘째 바이트가 0x40 이상이라 `\r`·`\n`이 글자 중간에 나오지 않는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dbcs {
    /// EUC-KR / CP949(UHC)
    Korean,
    /// GBK / GB18030 (4바이트 형식 포함)
    Gbk,
    Big5,
    ShiftJis,
    EucJp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextEncoding {
    pub enc: &'static Encoding,
    pub family: Family,
    /// 상태 표시줄용 이름
    pub label: &'static str,
    /// 파일 앞 BOM 길이 (본문은 이 위치부터)
    pub bom_len: usize,
}

impl TextEncoding {
    pub fn new(enc: &'static Encoding, bom_len: usize) -> Self {
        let family = family_of(enc);
        let label = if enc == encoding_rs::EUC_KR {
            "CP949"
        } else if bom_len > 0 {
            match family {
                Family::Utf8 => "UTF-8 BOM",
                Family::Utf16Le => "UTF-16 LE BOM",
                _ => "UTF-16 BE BOM",
            }
        } else {
            match family {
                Family::Utf16Le => "UTF-16 LE",
                Family::Utf16Be => "UTF-16 BE",
                _ => enc.name(),
            }
        };
        Self {
            enc,
            family,
            label,
            bom_len,
        }
    }

    /// 코드 단위 바이트 수 (줄바꿈 탐색 정렬용)
    pub fn unit(&self) -> usize {
        match self.family {
            Family::Utf16Le | Family::Utf16Be => 2,
            _ => 1,
        }
    }
}

pub fn family_of(enc: &'static Encoding) -> Family {
    use encoding_rs::*;
    if enc == UTF_8 {
        Family::Utf8
    } else if enc == UTF_16LE {
        Family::Utf16Le
    } else if enc == UTF_16BE {
        Family::Utf16Be
    } else if enc == EUC_KR {
        Family::Dbcs(Dbcs::Korean)
    } else if enc == GBK || enc == GB18030 {
        Family::Dbcs(Dbcs::Gbk)
    } else if enc == BIG5 {
        Family::Dbcs(Dbcs::Big5)
    } else if enc == SHIFT_JIS {
        Family::Dbcs(Dbcs::ShiftJis)
    } else if enc == EUC_JP {
        Family::Dbcs(Dbcs::EucJp)
    } else {
        Family::Single
    }
}

/// `b[i]`에서 시작하는 글자의 바이트 수 (≥ 1, 남은 길이를 넘지 않음).
pub fn char_len(family: Family, b: &[u8], i: usize) -> usize {
    let rest = b.len() - i;
    let n = match family {
        Family::Single => 1,
        Family::Utf8 => {
            let want = match b[i] {
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF4 => 4,
                _ => 1,
            };
            let mut n = 1;
            while n < want && i + n < b.len() && b[i + n] & 0xC0 == 0x80 {
                n += 1;
            }
            n
        }
        Family::Utf16Le | Family::Utf16Be => {
            let unit = |j: usize| -> Option<u16> {
                let p = b.get(j..j + 2)?;
                Some(if family == Family::Utf16Le {
                    u16::from_le_bytes([p[0], p[1]])
                } else {
                    u16::from_be_bytes([p[0], p[1]])
                })
            };
            match (unit(i), unit(i + 2)) {
                (Some(0xD800..=0xDBFF), Some(0xDC00..=0xDFFF)) => 4,
                _ => 2,
            }
        }
        Family::Dbcs(d) => {
            let lead = b[i];
            let next = b.get(i + 1).copied().unwrap_or(0);
            match d {
                Dbcs::Korean | Dbcs::Big5 if (0x81..=0xFE).contains(&lead) => 2,
                Dbcs::Gbk if (0x81..=0xFE).contains(&lead) => {
                    if (0x30..=0x39).contains(&next) {
                        4
                    } else {
                        2
                    }
                }
                Dbcs::ShiftJis if matches!(lead, 0x81..=0x9F | 0xE0..=0xFC) => 2,
                Dbcs::EucJp if lead == 0x8F => 3,
                Dbcs::EucJp if lead == 0x8E || (0xA1..=0xFE).contains(&lead) => 2,
                _ => 1,
            }
        }
    };
    n.clamp(1, rest.max(1))
}

/// `b[..target]` 안에서 `target` 이하의 가장 가까운 글자 경계. `b[0]`은 글자 경계여야 한다.
/// 진행을 보장하기 위해 0은 돌려주지 않는다(첫 글자가 `target`보다 길면 그 글자 끝).
pub fn snap(family: Family, b: &[u8], target: usize) -> usize {
    let target = target.min(b.len());
    let r = match family {
        Family::Single => target,
        Family::Utf8 => {
            let mut t = target;
            while t > 0 && target - t < 3 && b.get(t).is_some_and(|c| c & 0xC0 == 0x80) {
                t -= 1;
            }
            t
        }
        Family::Utf16Le | Family::Utf16Be => {
            let mut t = target & !1;
            if t >= 2 && t < b.len() {
                let u = |j: usize| {
                    if family == Family::Utf16Le {
                        u16::from_le_bytes([b[j], b[j + 1]])
                    } else {
                        u16::from_be_bytes([b[j], b[j + 1]])
                    }
                };
                if (0xDC00..=0xDFFF).contains(&u(t)) && (0xD800..=0xDBFF).contains(&u(t - 2)) {
                    t -= 2;
                }
            }
            t
        }
        Family::Dbcs(_) => {
            let mut i = 0;
            while i < target {
                let n = char_len(family, b, i);
                if i + n > target {
                    break;
                }
                i += n;
            }
            i
        }
    };
    if r == 0 && !b.is_empty() {
        char_len(family, b, 0)
    } else {
        r
    }
}

/// 행 바이트 → UTF-16. 행 시작이 글자 경계이므로 상태 없이 디코딩한다.
pub fn decode(te: &TextEncoding, b: &[u8]) -> Vec<u16> {
    let mut out = Vec::new();
    decode_into(te, b, &mut out);
    out
}

/// `decode`와 같되 `out`을 비우고 다시 쓴다 (찾기처럼 행을 많이 훑을 때 할당을 줄인다).
pub fn decode_into(te: &TextEncoding, b: &[u8], out: &mut Vec<u16>) {
    out.clear();
    match te.family {
        Family::Utf8 => {
            out.reserve(b.len());
            if b.is_ascii() {
                out.extend(b.iter().map(|&c| c as u16));
                return;
            }
            for chunk in b.utf8_chunks() {
                out.extend(chunk.valid().encode_utf16());
                if !chunk.invalid().is_empty() {
                    out.push(0xFFFD);
                }
            }
        }
        Family::Utf16Le | Family::Utf16Be => {
            let (pairs, rest) = b.as_chunks::<2>();
            let le = te.family == Family::Utf16Le;
            out.extend(pairs.iter().map(|p| {
                if le {
                    u16::from_le_bytes(*p)
                } else {
                    u16::from_be_bytes(*p)
                }
            }));
            if !rest.is_empty() {
                out.push(0xFFFD);
            }
        }
        _ => {
            let mut dec = te.enc.new_decoder_without_bom_handling();
            let cap = dec
                .max_utf16_buffer_length(b.len())
                .unwrap_or(b.len() * 2 + 4);
            out.resize(cap, 0);
            let (_, _, written, _) = dec.decode_to_utf16(b, out, true);
            out.truncate(written);
        }
    }
}

/// 시스템 ANSI 코드페이지(`GetACP`) → encoding_rs 인코딩.
pub fn ansi_encoding(cp: u32) -> &'static Encoding {
    use encoding_rs::*;
    match cp {
        949 => EUC_KR,
        932 => SHIFT_JIS,
        936 => GBK,
        950 => BIG5,
        874 => WINDOWS_874,
        65001 => UTF_8,
        1250..=1258 => {
            Encoding::for_label(format!("windows-{cp}").as_bytes()).unwrap_or(WINDOWS_1252)
        }
        _ => WINDOWS_1252,
    }
}

/// chardetng에 줄 최상위 도메인 힌트: 시스템 로캘의 인코딩을 우선 후보로 둔다.
fn tld_for(cp: u32) -> Option<&'static [u8]> {
    match cp {
        949 => Some(b"kr"),
        932 => Some(b"jp"),
        936 => Some(b"cn"),
        950 => Some(b"tw"),
        1251 => Some(b"ru"),
        1253 => Some(b"gr"),
        1254 => Some(b"tr"),
        1255 => Some(b"il"),
        1256 => Some(b"sa"),
        1257 => Some(b"lt"),
        1258 => Some(b"vn"),
        874 => Some(b"th"),
        _ => None,
    }
}

const HEAD_SAMPLE: usize = 1 << 20;
const SIDE_SAMPLE: usize = 256 << 10;

/// (표본, 파일 끝까지 포함하는지)
fn samples(data: &[u8]) -> Vec<(&[u8], bool)> {
    let len = data.len();
    let mut v = vec![(&data[..len.min(HEAD_SAMPLE)], len <= HEAD_SAMPLE)];
    if len > HEAD_SAMPLE + 2 * SIDE_SAMPLE {
        let mid = len / 2;
        v.push((&data[mid..mid + SIDE_SAMPLE], false));
        v.push((&data[len - SIDE_SAMPLE..], true));
    }
    v
}

fn utf16_guess(data: &[u8]) -> Option<&'static Encoding> {
    let head = &data[..data.len().min(4096) & !1];
    let pairs = head.len() / 2;
    if pairs < 16 {
        return None;
    }
    let (mut even0, mut odd0) = (0, 0);
    for p in head.as_chunks::<2>().0 {
        even0 += (p[0] == 0) as usize;
        odd0 += (p[1] == 0) as usize;
    }
    if odd0 * 10 >= pairs * 4 && even0 * 20 < pairs {
        Some(encoding_rs::UTF_16LE)
    } else if even0 * 10 >= pairs * 4 && odd0 * 20 < pairs {
        Some(encoding_rs::UTF_16BE)
    } else {
        None
    }
}

/// 표본이 UTF-8로 유효한지. 표본 경계에서 잘린 글자는 허용한다.
fn utf8_valid(sample: &[u8], at_start: bool, at_end: bool) -> bool {
    let mut s = sample;
    if !at_start {
        let skip = s.iter().take(3).take_while(|c| **c & 0xC0 == 0x80).count();
        s = &s[skip..];
    }
    match std::str::from_utf8(s) {
        Ok(_) => true,
        Err(e) => !at_end && e.error_len().is_none(),
    }
}

/// 표본의 마지막 줄바꿈까지 (잘린 글자 때문에 오류로 판정하지 않도록)
fn trim_to_line(s: &[u8]) -> &[u8] {
    match memchr::memrchr(b'\n', s) {
        Some(p) => &s[..=p],
        None => s,
    }
}

pub fn detect(data: &[u8], ansi_cp: u32) -> TextEncoding {
    use encoding_rs::*;
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return TextEncoding::new(UTF_8, 3);
    }
    if data.starts_with(&[0xFF, 0xFE]) {
        return TextEncoding::new(UTF_16LE, 2);
    }
    if data.starts_with(&[0xFE, 0xFF]) {
        return TextEncoding::new(UTF_16BE, 2);
    }
    if let Some(e) = utf16_guess(data) {
        return TextEncoding::new(e, 0);
    }
    let ss = samples(data);
    if ss
        .iter()
        .enumerate()
        .all(|(i, (s, end))| utf8_valid(s, i == 0, *end))
    {
        return TextEncoding::new(UTF_8, 0);
    }
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    for (s, _) in &ss {
        det.feed(s, false);
    }
    det.feed(&[], true);
    let guess = det.guess(tld_for(ansi_cp), chardetng::Utf8Detection::Deny);
    // 추정 결과로 표본을 오류 없이 디코딩할 수 없으면 "실패"로 보고 ANSI 코드페이지로 (FR-T01)
    let head = trim_to_line(ss[0].0);
    let ok = guess
        .decode_without_bom_handling_and_without_replacement(head)
        .is_some();
    TextEncoding::new(if ok { guess } else { ansi_encoding(ansi_cp) }, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cp949(s: &str) -> Vec<u8> {
        encoding_rs::EUC_KR.encode(s).0.into_owned()
    }

    #[test]
    fn detects_boms_and_utf8() {
        assert_eq!(detect(b"\xEF\xBB\xBFabc", 949).label, "UTF-8 BOM");
        assert_eq!(detect(b"\xFF\xFEa\0", 949).family, Family::Utf16Le);
        assert_eq!(detect(b"\xFE\xFF\0a", 949).family, Family::Utf16Be);
        assert_eq!(detect("한글 텍스트".as_bytes(), 949).label, "UTF-8");
        assert_eq!(detect(b"plain ascii", 949).label, "UTF-8");
    }

    #[test]
    fn detects_utf16_without_bom() {
        let le: Vec<u8> = "hello world, this is utf-16 text"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(detect(&le, 949).family, Family::Utf16Le);
    }

    #[test]
    fn detects_cp949() {
        let text =
            "가나다라마바사 한국어 문서입니다. 인코딩 자동 감지를 시험합니다.\r\n".repeat(20);
        let e = detect(&cp949(&text), 949);
        assert_eq!(e.label, "CP949");
        assert_eq!(e.family, Family::Dbcs(Dbcs::Korean));
    }

    #[test]
    fn decode_roundtrip_and_invalid() {
        let te = TextEncoding::new(encoding_rs::EUC_KR, 0);
        assert_eq!(
            String::from_utf16(&decode(&te, &cp949("한글 abc"))).unwrap(),
            "한글 abc"
        );
        let u8e = TextEncoding::new(encoding_rs::UTF_8, 0);
        assert_eq!(
            String::from_utf16(&decode(&u8e, b"a\xFFb")).unwrap(),
            "a\u{FFFD}b"
        );
    }

    #[test]
    fn snap_keeps_char_boundaries() {
        let s = "가나다".as_bytes(); // 3바이트씩
        assert_eq!(snap(Family::Utf8, s, 4), 3);
        assert_eq!(snap(Family::Utf8, s, 1), 3); // 진행 보장
        let k = cp949("a가나"); // a(1) 가(2) 나(2)
        assert_eq!(snap(Family::Dbcs(Dbcs::Korean), &k, 2), 1);
        assert_eq!(snap(Family::Dbcs(Dbcs::Korean), &k, 3), 3);
        let u: Vec<u8> = "a😀b"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(snap(Family::Utf16Le, &u, 4), 2); // 서로게이트 쌍 가운데 → 앞으로
    }
}

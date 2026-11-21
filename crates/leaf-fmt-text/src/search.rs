//! 문서 내 찾기 (FR-V04): 대소문자 무시. 줄(행)을 넘는 일치는 찾지 않는다.
//!
//! 기본 경로는 행마다 디코딩·소문자화해 비교한다. UTF-8 문서이고 검색어의 대소문자 변형이
//! ASCII 안에서만 생기면(대부분의 검색) 원시 바이트에서 바로 찾는 빠른 경로를 쓴다 — 결과는 같다.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::doc::TextDoc;
use crate::encoding::Family;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// 행 시작 바이트
    pub row: usize,
    /// 행 텍스트의 UTF-16 범위
    pub range: Range<u32>,
}

/// UTF-16 코드 단위별 소문자화. 길이가 바뀌는 변환은 하지 않아 인덱스가 원문과 같다.
pub fn fold(units: &[u16]) -> Vec<u16> {
    let mut v = units.to_vec();
    fold_in_place(&mut v);
    v
}

pub fn fold_in_place(units: &mut [u16]) {
    let t = fold_table();
    for u in units {
        *u = t[*u as usize];
    }
}

/// BMP 전체의 소문자화 표 (128KB, 처음 찾을 때 한 번 만든다). 글자마다 유니코드 표를
/// 찾는 것보다 훨씬 빠르다(1GB 한글 문서 찾기 기준).
fn fold_table() -> &'static [u16] {
    static T: std::sync::OnceLock<Box<[u16]>> = std::sync::OnceLock::new();
    T.get_or_init(|| (0..=u16::MAX).map(fold_unit).collect())
}

fn fold_unit(u: u16) -> u16 {
    if u < 0x80 {
        return (u as u8).to_ascii_lowercase() as u16;
    }
    let Some(c) = char::from_u32(u as u32) else {
        return u; // 서로게이트
    };
    let mut it = c.to_lowercase();
    match (it.next(), it.next()) {
        (Some(l), None) if l.len_utf16() == 1 => l as u32 as u16,
        _ => u,
    }
}

/// 겹치지 않는 일치 위치들. 둘 다 `fold`된 상태여야 한다.
pub fn find_in(hay: &[u16], needle: &[u16]) -> Vec<Range<u32>> {
    let mut out = Vec::new();
    if needle.is_empty() || hay.len() < needle.len() {
        return out;
    }
    let first = needle[0];
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i] == first && hay[i..i + needle.len()] == *needle {
            out.push(i as u32..(i + needle.len()) as u32);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

pub struct SearchOutcome {
    pub hits: usize,
    pub capped: bool,
    pub cancelled: bool,
}

/// 문서 처음부터 찾는다. `on_batch(일치들, 처리한 바이트)`는 일치가 쌓였거나 일정량을 훑을 때마다 불린다.
pub fn search(
    doc: &TextDoc,
    query: &str,
    max_hits: usize,
    cancel: &AtomicBool,
    mut on_batch: impl FnMut(Vec<Hit>, usize),
) -> SearchOutcome {
    const REPORT_BYTES: usize = 8 << 20;
    let needle = fold(&query.encode_utf16().collect::<Vec<_>>());
    let mut out = SearchOutcome {
        hits: 0,
        capped: false,
        cancelled: false,
    };
    if needle.is_empty() {
        return out;
    }
    if let Some((pat, ci)) = byte_pattern(doc, query) {
        return search_bytes(
            doc,
            &pat,
            &ci,
            needle.len() as u32,
            max_hits,
            cancel,
            on_batch,
        );
    }
    let mut batch = Vec::new();
    let mut p = Some(doc.start());
    let mut last_report = doc.start();
    let mut buf = Vec::new();
    while let Some(rs) = p {
        let r = doc.row(rs);
        crate::encoding::decode_into(&doc.enc, &doc.bytes()[r.start..r.end], &mut buf);
        fold_in_place(&mut buf);
        for range in find_in(&buf, &needle) {
            batch.push(Hit { row: rs, range });
            out.hits += 1;
            if out.hits >= max_hits {
                out.capped = true;
                on_batch(std::mem::take(&mut batch), r.end);
                return out;
            }
        }
        p = r.next;
        if r.end - last_report >= REPORT_BYTES || batch.len() >= 1000 {
            if cancel.load(Ordering::Relaxed) {
                out.cancelled = true;
                return out;
            }
            on_batch(std::mem::take(&mut batch), r.end);
            last_report = r.end;
        }
    }
    on_batch(batch, doc.len());
    out
}

/// 빠른 경로용 바이트 패턴: (UTF-8 바이트(ASCII 글자는 소문자), 대소문자 무시 여부).
/// 디코딩 비교와 결과가 같을 때만 Some — 검색어 글자로 소문자화되는 글자가 그 글자 자신
/// (ASCII 글자는 대·소문자 둘)뿐이어야 한다. 예) 'k'는 켈빈 기호(U+212A)도 'k'가 되므로 제외.
fn byte_pattern(doc: &TextDoc, query: &str) -> Option<(Vec<u8>, Vec<bool>)> {
    if doc.enc.family != Family::Utf8 {
        return None;
    }
    let t = fold_table();
    let mut pat = Vec::new();
    let mut ci = Vec::new();
    for c in query.chars() {
        if (c as u32) < 0x10000 {
            let f = t[c as usize];
            let mut pre = (0..=u16::MAX).filter(|&u| t[u as usize] == f);
            if c.is_ascii_alphabetic() {
                if pre.any(|u| u >= 0x80) {
                    return None;
                }
                pat.push(c.to_ascii_lowercase() as u8);
                ci.push(true);
                continue;
            }
            if pre.any(|u| u != c as u16) {
                return None;
            }
        }
        let mut buf = [0u8; 4];
        for b in c.encode_utf8(&mut buf).bytes() {
            pat.push(b);
            ci.push(false);
        }
    }
    Some((pat, ci))
}

fn search_bytes(
    doc: &TextDoc,
    pat: &[u8],
    ci: &[bool],
    len16: u32,
    max_hits: usize,
    cancel: &AtomicBool,
    mut on_batch: impl FnMut(Vec<Hit>, usize),
) -> SearchOutcome {
    const BLOCK: usize = 8 << 20;
    let mut out = SearchOutcome {
        hits: 0,
        capped: false,
        cancelled: false,
    };
    let d = doc.bytes();
    let n = pat.len();
    let eq = |b: &[u8]| {
        b.iter().zip(pat).zip(ci).all(|((h, p), c)| {
            if *c {
                h.to_ascii_lowercase() == *p
            } else {
                h == p
            }
        })
    };
    // 후보 찾기: 대소문자 무시 글자가 없으면 memmem, 있으면 첫 바이트의 두 변형을 memchr2로
    let finder = (!ci.contains(&true)).then(|| memchr::memmem::Finder::new(pat));
    let (b0, b1) = if ci[0] {
        (pat[0], pat[0].to_ascii_uppercase())
    } else {
        (pat[0], pat[0])
    };
    let mut batch = Vec::new();
    let mut buf = Vec::new();
    let mut pos = doc.start();
    let len = d.len();
    while pos + n <= len {
        let block_end = (pos + BLOCK).min(len);
        let hay = &d[pos..(block_end + n - 1).min(len)];
        let mut i = 0;
        while i + n <= hay.len() {
            let found = match &finder {
                Some(f) => f.find(&hay[i..]),
                None => memchr::memchr2(b0, b1, &hay[i..]),
            };
            let Some(off) = found else { break };
            let c = i + off;
            if pos + c >= block_end || c + n > hay.len() {
                break;
            }
            if finder.is_none() && !eq(&hay[c..c + n]) {
                i = c + 1;
                continue;
            }
            let b = pos + c;
            let rs = doc.row_containing(b);
            let r = doc.row(rs);
            if b + n > r.end {
                // 긴 줄을 나눈 행 경계에 걸침: 디코딩 경로와 같게 제외
                i = c + 1;
                continue;
            }
            crate::encoding::decode_into(&doc.enc, &d[r.start..b], &mut buf);
            let s = buf.len() as u32;
            batch.push(Hit {
                row: rs,
                range: s..s + len16,
            });
            out.hits += 1;
            if out.hits >= max_hits {
                out.capped = true;
                on_batch(batch, b + n);
                return out;
            }
            i = c + n;
        }
        pos = block_end;
        if cancel.load(Ordering::Relaxed) {
            out.cancelled = true;
            return out;
        }
        on_batch(std::mem::take(&mut batch), pos);
    }
    on_batch(batch, len);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::TextEncoding;
    use std::sync::Arc;

    #[test]
    fn fold_keeps_length() {
        let s: Vec<u16> = "ÄBC가😀".encode_utf16().collect();
        let f = fold(&s);
        assert_eq!(f.len(), s.len());
        assert_eq!(String::from_utf16(&f).unwrap(), "äbc가😀");
    }

    #[test]
    fn finds_case_insensitive_per_row() {
        let d = TextDoc::new(
            Arc::new(
                "Hello world\nWORLD peace 한글\n한글한글"
                    .as_bytes()
                    .to_vec(),
            ),
            TextEncoding::new(encoding_rs::UTF_8, 0),
        );
        let mut all = Vec::new();
        let o = search(&d, "World", 100, &AtomicBool::new(false), |h, _| {
            all.extend(h)
        });
        assert_eq!(o.hits, 2);
        assert_eq!(
            all,
            vec![
                Hit {
                    row: 0,
                    range: 6..11
                },
                Hit {
                    row: 12,
                    range: 0..5
                }
            ]
        );
        let mut all = Vec::new();
        let o = search(&d, "한글", 2, &AtomicBool::new(false), |h, _| {
            all.extend(h)
        });
        assert!(o.capped);
        assert_eq!(all.len(), 2);
    }

    /// (행 순번, 범위) 목록: 인코딩이 달라도 비교할 수 있게
    fn hits_by_ordinal(d: &TextDoc, q: &str) -> Vec<(usize, Range<u32>)> {
        let mut starts = Vec::new();
        let mut p = Some(d.start());
        while let Some(s) = p {
            starts.push(s);
            p = d.next_row(s);
        }
        let mut all = Vec::new();
        search(d, q, usize::MAX, &AtomicBool::new(false), |h, _| {
            all.extend(h)
        });
        all.into_iter()
            .map(|h| (starts.binary_search(&h.row).unwrap(), h.range))
            .collect()
    }

    #[test]
    fn byte_fast_path_matches_decoding_path() {
        let mut s = String::new();
        for i in 0..300 {
            s.push_str(&format!(
                "{i} NeEdLe needle 바늘 ÄÖ äö \u{212A}ilo kilo 😀x\t\r\n"
            ));
        }
        s.push_str(&"abcneedle".repeat(2000)); // 긴 줄 → 행 경계에 걸치는 일치
        s.push('\n');
        let u8doc = TextDoc::new(
            Arc::new(s.clone().into_bytes()),
            TextEncoding::new(encoding_rs::UTF_8, 0),
        );
        let mut u16 = vec![0xFF, 0xFE];
        u16.extend(s.encode_utf16().flat_map(|u| u.to_le_bytes()));
        let u16doc = TextDoc::new(Arc::new(u16), TextEncoding::new(encoding_rs::UTF_16LE, 2));
        // needle·바늘·숫자는 빠른 경로, kilo(켈빈 기호)·äö는 디코딩 경로
        assert!(byte_pattern(&u8doc, "needle").is_some());
        assert!(byte_pattern(&u8doc, "바늘 1").is_some());
        assert!(byte_pattern(&u8doc, "kilo").is_none());
        assert!(byte_pattern(&u8doc, "äö").is_none());
        for q in [
            "needle", "NEEDLE", "바늘", "17 n", "😀x", "kilo", "äö", "le nee",
        ] {
            // 행 순번은 두 문서에서 같다 (긴 줄 분할은 바이트 기준이라 다를 수 있어 긴 줄 앞까지만)
            let a: Vec<_> = hits_by_ordinal(&u8doc, q)
                .into_iter()
                .filter(|(r, _)| *r < 300)
                .collect();
            let b: Vec<_> = hits_by_ordinal(&u16doc, q)
                .into_iter()
                .filter(|(r, _)| *r < 300)
                .collect();
            assert_eq!(a, b, "query {q}");
            assert!(!a.is_empty(), "query {q}");
        }
        // 긴 줄: 빠른 경로 결과가 행별 디코딩 결과와 같은지 직접 확인
        let mut expect = 0;
        let mut p = Some(u8doc.start());
        while let Some(rs) = p {
            let (r, t) = u8doc.decode_row(rs);
            expect += find_in(
                &fold(&t),
                &fold(&"needle".encode_utf16().collect::<Vec<_>>()),
            )
            .len();
            p = r.next;
        }
        assert_eq!(hits_by_ordinal(&u8doc, "needle").len(), expect);
    }
}

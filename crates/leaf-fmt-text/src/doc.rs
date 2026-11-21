//! 텍스트 문서와 행(row) 탐색 (FR-T02, FR-T06).
//!
//! **행** = 화면에 독립적으로 배치하는 단위. 보통은 논리 줄 하나이고, `MAX_ROW_BYTES`보다 긴 줄은
//! 글자 경계에서 잘라 여러 행으로 나눈다(수 MB짜리 한 줄에서도 멈추지 않게). 행의 위치는 행 시작의
//! 파일 바이트 오프셋으로 나타낸다. 행 경계는 줄 시작에서부터 결정적으로 정해지므로, 행 시작 하나만
//! 알면 다음 행을 구할 수 있다.

use std::sync::{Arc, Mutex};

use crate::encoding::{self, Family, TextEncoding};

/// 행 최대 바이트. 이보다 긴 줄은 표시용으로 나눈다 (FR-T06).
pub const MAX_ROW_BYTES: usize = 4096;

/// 파일 내용. 작은 파일은 `Vec<u8>`, 큰 파일은 메모리 매핑(leaf-win이 구현).
pub trait Source: Send + Sync {
    fn bytes(&self) -> &[u8];
}

impl Source for Vec<u8> {
    fn bytes(&self) -> &[u8] {
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub start: usize,
    /// 본문 끝 (줄바꿈 제외)
    pub end: usize,
    /// 다음 행 시작. 마지막 행이면 None.
    pub next: Option<usize>,
}

impl Row {
    /// 이 행 뒤에 줄바꿈이 있는지 (긴 줄을 나눈 행이면 false)
    pub fn has_newline(&self) -> bool {
        self.next.is_some_and(|n| n > self.end)
    }
}

/// 문서 안 위치: (행 시작 바이트, 행 텍스트의 UTF-16 인덱스). 사전식 순서가 문서 순서다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Pos {
    pub row: usize,
    pub idx: u32,
}

/// 긴 줄 하나의 행 시작 목록 캐시: (줄 시작, 줄 끝, 행 시작들)
type ChunkCache = Option<(usize, usize, Arc<Vec<usize>>)>;

pub struct TextDoc {
    src: Arc<dyn Source>,
    pub enc: TextEncoding,
    chunks: Mutex<ChunkCache>,
}

impl TextDoc {
    pub fn new(src: Arc<dyn Source>, enc: TextEncoding) -> Self {
        Self {
            src,
            enc,
            chunks: Mutex::new(None),
        }
    }

    pub fn bytes(&self) -> &[u8] {
        self.src.bytes()
    }

    /// 본문 시작(BOM 다음)
    pub fn start(&self) -> usize {
        self.enc.bom_len.min(self.len())
    }

    pub fn len(&self) -> usize {
        self.src.bytes().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() <= self.start()
    }

    fn unit(&self) -> usize {
        self.enc.unit()
    }

    /// `p`의 코드 단위가 `\n`/`\r`이면 그 글자
    fn nl_at(&self, p: usize) -> Option<u8> {
        let d = self.bytes();
        let (lo, hi) = match self.enc.family {
            Family::Utf16Le => (*d.get(p)?, *d.get(p + 1)?),
            Family::Utf16Be => (*d.get(p + 1)?, *d.get(p)?),
            _ => (*d.get(p)?, 0),
        };
        (hi == 0 && (lo == b'\n' || lo == b'\r')).then_some(lo)
    }

    /// 바이트 후보 `p`(값이 \n 또는 \r)를 코드 단위 시작으로 바꾼다. 정렬이 안 맞으면 None.
    fn unit_of_candidate(&self, p: usize) -> Option<usize> {
        let s = self.start();
        let u = match self.enc.family {
            Family::Utf16Le => (p - s).is_multiple_of(2).then_some(p)?,
            Family::Utf16Be => ((p - s) % 2 == 1).then(|| p - 1)?,
            _ => p,
        };
        self.nl_at(u).map(|_| u)
    }

    /// `[from, limit)`에서 첫 줄바꿈: (위치, 길이). CRLF는 한 줄바꿈.
    pub fn find_newline(&self, from: usize, limit: usize) -> Option<(usize, usize)> {
        let d = self.bytes();
        let limit = limit.min(d.len());
        let mut i = from;
        while i < limit {
            let p = i + memchr::memchr2(b'\n', b'\r', &d[i..limit])?;
            if let Some(u) = self.unit_of_candidate(p)
                && u >= from
            {
                let n = self.unit();
                let len = if self.nl_at(u) == Some(b'\r') && self.nl_at(u + n) == Some(b'\n') {
                    2 * n
                } else {
                    n
                };
                return Some((u, len));
            }
            i = p + 1;
        }
        None
    }

    /// `pos` 앞쪽(`[start, pos)`)의 마지막 줄바꿈 코드 단위 위치
    fn rfind_newline(&self, pos: usize) -> Option<usize> {
        let d = self.bytes();
        let s = self.start();
        let mut end = pos.min(d.len());
        while end > s {
            let p = s + memchr::memrchr2(b'\n', b'\r', &d[s..end])?;
            if let Some(u) = self.unit_of_candidate(p)
                && u + self.unit() <= pos
            {
                return Some(u);
            }
            end = p;
        }
        None
    }

    /// `pos`가 속한 논리 줄의 시작
    pub fn line_start(&self, pos: usize) -> usize {
        self.rfind_newline(pos)
            .map_or(self.start(), |u| u + self.unit())
    }

    /// 행 시작 `pos`의 행. `pos`는 이 문서가 돌려준 행 시작이어야 한다.
    pub fn row(&self, pos: usize) -> Row {
        let len = self.len();
        let pos = pos.clamp(self.start(), len);
        // 긴 줄 전체를 훑지 않도록 행 최대 길이까지만 줄바꿈을 찾는다
        let window = pos + MAX_ROW_BYTES + 2 * self.unit();
        match self.find_newline(pos, window) {
            Some((nl, n)) if nl - pos <= MAX_ROW_BYTES => Row {
                start: pos,
                end: nl,
                next: Some(nl + n),
            },
            None if len - pos <= MAX_ROW_BYTES => Row {
                start: pos,
                end: len,
                next: None,
            },
            _ => {
                let lim = (pos + MAX_ROW_BYTES + 8).min(len);
                let end =
                    pos + encoding::snap(self.enc.family, &self.bytes()[pos..lim], MAX_ROW_BYTES);
                Row {
                    start: pos,
                    end,
                    next: Some(end),
                }
            }
        }
    }

    pub fn next_row(&self, pos: usize) -> Option<usize> {
        self.row(pos).next
    }

    /// 긴 줄 `[ls, le)`의 행 시작들 (캐시)
    fn chunks(&self, ls: usize, le: usize) -> Arc<Vec<usize>> {
        let mut c = self.chunks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((s, e, v)) = &*c
            && *s == ls
            && *e == le
        {
            return v.clone();
        }
        let mut v = vec![ls];
        let mut p = ls;
        while le - p > MAX_ROW_BYTES {
            let lim = (p + MAX_ROW_BYTES + 8).min(le);
            p += encoding::snap(self.enc.family, &self.bytes()[p..lim], MAX_ROW_BYTES);
            v.push(p);
        }
        let v = Arc::new(v);
        *c = Some((ls, le, v.clone()));
        v
    }

    /// 캐시된 긴 줄 안이면 (줄 시작, 줄 끝)
    fn cached_line(&self, pos: usize) -> Option<(usize, usize, Arc<Vec<usize>>)> {
        let c = self.chunks.lock().unwrap_or_else(|e| e.into_inner());
        match &*c {
            Some((s, e, v)) if *s <= pos && pos <= *e => Some((*s, *e, v.clone())),
            _ => None,
        }
    }

    fn line_end(&self, ls: usize) -> usize {
        self.find_newline(ls, usize::MAX)
            .map_or(self.len(), |(nl, _)| nl)
    }

    /// 줄 `[ls, le)` 안에서 `pos` 이하의 마지막 행 시작
    fn chunk_at_or_before(&self, ls: usize, le: usize, pos: usize) -> usize {
        if le - ls <= MAX_ROW_BYTES {
            return ls;
        }
        let v = self.chunks(ls, le);
        let i = v.partition_point(|&c| c <= pos);
        v[i.saturating_sub(1)]
    }

    /// 이전 행 시작. 맨 앞이면 None.
    pub fn prev_row(&self, pos: usize) -> Option<usize> {
        let s = self.start();
        if pos <= s {
            return None;
        }
        let n = self.unit();
        if pos >= s + n && self.nl_at(pos - n).is_some() {
            // 이전 줄의 마지막 행
            let mut nl = pos - n;
            if self.nl_at(nl) == Some(b'\n') && nl >= s + n && self.nl_at(nl - n) == Some(b'\r') {
                nl -= n;
            }
            let ls = self.line_start(nl);
            if nl - ls <= MAX_ROW_BYTES {
                return Some(ls);
            }
            return Some(self.chunk_at_or_before(ls, nl, nl.saturating_sub(1)));
        }
        // 긴 줄 안의 이어지는 행
        let (ls, le) = match self.cached_line(pos) {
            Some((ls, le, _)) => (ls, le),
            None => {
                let ls = self.line_start(pos);
                (ls, self.line_end(ls))
            }
        };
        Some(self.chunk_at_or_before(ls, le, pos - 1))
    }

    /// 임의의 바이트 위치를 포함하는 행의 시작 (스크롤바 끌기·세션 복원용)
    pub fn row_containing(&self, b: usize) -> usize {
        let s = self.start();
        let n = self.unit();
        let mut b = b.clamp(s, self.len());
        b = s + (b - s) / n * n;
        // CR과 LF 사이면 CR로
        if b >= s + n && self.nl_at(b) == Some(b'\n') && self.nl_at(b - n) == Some(b'\r') {
            b -= n;
        }
        if let Some((ls, le, _)) = self.cached_line(b) {
            return self.chunk_at_or_before(ls, le, b);
        }
        let ls = self.line_start(b);
        if self
            .find_newline(ls, ls + MAX_ROW_BYTES + 2 * n)
            .is_some_and(|(nl, _)| nl - ls <= MAX_ROW_BYTES)
            || self.len() - ls <= MAX_ROW_BYTES
        {
            return ls;
        }
        let le = self.line_end(ls);
        self.chunk_at_or_before(ls, le, b)
    }

    /// 마지막 행 시작
    pub fn last_row(&self) -> usize {
        self.row_containing(self.len())
    }

    pub fn decode(&self, row: &Row) -> Vec<u16> {
        encoding::decode(&self.enc, &self.bytes()[row.start..row.end])
    }

    pub fn decode_row(&self, pos: usize) -> (Row, Vec<u16>) {
        let r = self.row(pos);
        let t = self.decode(&r);
        (r, t)
    }

    /// 범위 텍스트 (복사용). 행 사이 줄바꿈은 CRLF. `cap_bytes`를 넘으면 Err(파일 바이트 수).
    pub fn extract(&self, a: Pos, b: Pos, cap_bytes: usize) -> Result<String, usize> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        if b.row.saturating_sub(a.row) > cap_bytes {
            return Err(b.row - a.row);
        }
        let mut out: Vec<u16> = Vec::new();
        let mut p = Some(a.row);
        while let Some(rs) = p {
            if rs > b.row {
                break;
            }
            let (r, t) = self.decode_row(rs);
            let s = if rs == a.row { a.idx as usize } else { 0 }.min(t.len());
            let e = if rs == b.row { b.idx as usize } else { t.len() }.clamp(s, t.len());
            out.extend_from_slice(&t[s..e]);
            if rs == b.row {
                break;
            }
            if r.has_newline() {
                out.extend_from_slice(&[13, 10]);
            }
            p = r.next;
        }
        Ok(String::from_utf16_lossy(&out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::TextEncoding;

    fn doc(bytes: Vec<u8>, enc: &'static encoding_rs::Encoding, bom: usize) -> TextDoc {
        TextDoc::new(Arc::new(bytes), TextEncoding::new(enc, bom))
    }

    fn rows(d: &TextDoc) -> Vec<(usize, usize)> {
        let mut v = Vec::new();
        let mut p = Some(d.start());
        while let Some(s) = p {
            let r = d.row(s);
            v.push((r.start, r.end));
            p = r.next;
        }
        v
    }

    fn texts(d: &TextDoc) -> Vec<String> {
        rows(d)
            .iter()
            .map(|&(s, _)| String::from_utf16(&d.decode_row(s).1).unwrap())
            .collect()
    }

    #[test]
    fn line_endings_crlf_lf_cr() {
        let d = doc(b"a\r\nbb\ncc\rd\r\n".to_vec(), encoding_rs::UTF_8, 0);
        assert_eq!(texts(&d), vec!["a", "bb", "cc", "d", ""]);
        // 역방향이 정방향과 일치
        let fwd: Vec<usize> = rows(&d).iter().map(|r| r.0).collect();
        let mut back = vec![d.last_row()];
        while let Some(p) = d.prev_row(*back.last().unwrap()) {
            back.push(p);
        }
        back.reverse();
        assert_eq!(back, fwd);
        assert_eq!(d.row_containing(2), 0); // CR LF 사이
        assert_eq!(d.row_containing(4), 3);
    }

    #[test]
    fn no_trailing_newline_and_empty() {
        let d = doc(b"x\ny".to_vec(), encoding_rs::UTF_8, 0);
        assert_eq!(texts(&d), vec!["x", "y"]);
        let e = doc(Vec::new(), encoding_rs::UTF_8, 0);
        assert_eq!(texts(&e), vec![""]);
        assert_eq!(e.prev_row(0), None);
    }

    #[test]
    fn utf16_rows_ignore_misaligned_bytes() {
        // U+0A0D(구르무키 문자)의 바이트 0D 0A는 줄바꿈이 아니다
        let s = "가\u{0A0D}나\r\n둘째";
        let mut b = vec![0xFF, 0xFE];
        b.extend(s.encode_utf16().flat_map(|u| u.to_le_bytes()));
        let d = doc(b, encoding_rs::UTF_16LE, 2);
        assert_eq!(texts(&d), vec!["가\u{0A0D}나", "둘째"]);
        assert_eq!(d.prev_row(d.last_row()), Some(2));
    }

    #[test]
    fn long_line_is_split_and_walkable_both_ways() {
        let mut s = String::from("head\n");
        s.push_str(&"가나다라".repeat(3000)); // 36KB 한 줄
        s.push_str("\ntail");
        let d = doc(s.clone().into_bytes(), encoding_rs::UTF_8, 0);
        let rs = rows(&d);
        assert!(rs.len() > 10);
        assert!(rs.iter().all(|(a, b)| b - a <= MAX_ROW_BYTES));
        assert_eq!(texts(&d).concat(), s.replace('\n', ""));
        let fwd: Vec<usize> = rs.iter().map(|r| r.0).collect();
        let mut back = vec![d.last_row()];
        while let Some(p) = d.prev_row(*back.last().unwrap()) {
            back.push(p);
        }
        back.reverse();
        assert_eq!(back, fwd);
        // 긴 줄 가운데 임의 위치 → 그 위치를 포함하는 행
        let mid = 5 + 20_000;
        let r = d.row_containing(mid);
        assert!(fwd.contains(&r) && r <= mid && d.row(r).end > mid);
    }

    #[test]
    fn extract_joins_rows() {
        let d = doc(b"abc\r\ndef\nghi".to_vec(), encoding_rs::UTF_8, 0);
        let a = Pos { row: 0, idx: 1 };
        let b = Pos { row: 9, idx: 2 };
        assert_eq!(d.extract(b, a, 1 << 20).unwrap(), "bc\r\ndef\r\ngh");
        assert!(d.extract(a, b, 4).is_err());
    }
}

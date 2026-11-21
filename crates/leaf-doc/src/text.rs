//! 쪽 텍스트와 글자 상자: 적중 판정, 선택 영역 사각형, 찾기 (FR-P05, FR-P06).
//! 좌표는 쪽 왼쪽 위 원점의 pt.

use std::ops::Range;

use crate::RectF;

#[derive(Debug, Clone, Default)]
pub struct PageText {
    pub chars: Vec<char>,
    /// `chars`와 같은 길이. PDFium이 생성한 글자(줄바꿈 등)는 빈 사각형.
    pub boxes: Vec<RectF>,
}

impl PageText {
    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// 점 위의 글자 (커서 모양 결정용).
    pub fn char_at(&self, x: f32, y: f32) -> Option<usize> {
        const SLOP: f32 = 1.0;
        self.boxes.iter().position(|b| {
            !b.is_empty()
                && RectF::new(b.l - SLOP, b.t - SLOP, b.r + SLOP, b.b + SLOP).contains(x, y)
        })
    }

    /// 선택용 적중: 가장 가까운 글자의 경계 인덱스. 글자 오른쪽 절반이면 다음 인덱스.
    pub fn caret_at(&self, x: f32, y: f32) -> Option<usize> {
        // 줄(세로 거리)을 먼저 고르고, 그 줄 안에서 가로로 가장 가까운 글자를 고른다.
        let dy = |b: &RectF| (b.t - y).max(0.0).max(y - b.b);
        let dx = |b: &RectF| (b.l - x).max(0.0).max(x - b.r);
        let live = || self.boxes.iter().enumerate().filter(|(_, b)| !b.is_empty());
        let best_dy = live().map(|(_, b)| dy(b)).min_by(f32::total_cmp)?;
        let (i, b) = live()
            .filter(|(_, b)| dy(b) <= best_dy + 0.5)
            .min_by(|(_, a), (_, b)| dx(a).total_cmp(&dx(b)))?;
        Some(if x > (b.l + b.r) * 0.5 { i + 1 } else { i })
    }

    /// 범위의 글자 상자를 줄 단위로 합친 사각형들.
    pub fn range_rects(&self, range: Range<usize>) -> Vec<RectF> {
        let end = range.end.min(self.boxes.len());
        let mut out: Vec<RectF> = Vec::new();
        let mut cur: Option<RectF> = None;
        for b in self.boxes.get(range.start.min(end)..end).unwrap_or(&[]) {
            if b.is_empty() {
                continue;
            }
            cur = Some(match cur {
                Some(c) if same_line(&c, b) => c.union(b),
                Some(c) => {
                    out.push(c);
                    *b
                }
                None => *b,
            });
        }
        out.extend(cur);
        out
    }

    /// 범위의 텍스트. 제어 문자는 줄바꿈·탭만 남긴다.
    pub fn text(&self, range: Range<usize>) -> String {
        let end = range.end.min(self.chars.len());
        self.chars
            .get(range.start.min(end)..end)
            .unwrap_or(&[])
            .iter()
            .filter(|c| !c.is_control() || matches!(c, '\r' | '\n' | '\t'))
            .collect()
    }

    /// 대소문자 무시 찾기. 줄바꿈은 공백으로 보고, 연속 공백은 하나로 본다.
    pub fn find_all(&self, query: &str) -> Vec<Range<usize>> {
        let needle: Vec<char> = normalize(query.chars().map(|c| (c, 0usize)))
            .into_iter()
            .map(|(c, _)| c)
            .collect();
        let needle = trim_spaces(&needle);
        if needle.is_empty() {
            return Vec::new();
        }
        let hay = normalize(self.chars.iter().copied().enumerate().map(|(i, c)| (c, i)));
        let mut out = Vec::new();
        let mut i = 0;
        while i + needle.len() <= hay.len() {
            if hay[i..i + needle.len()]
                .iter()
                .zip(needle)
                .all(|((h, _), n)| h == n)
            {
                let start = hay[i].1;
                let end = hay[i + needle.len() - 1].1 + 1;
                out.push(start..end);
                i += needle.len();
            } else {
                i += 1;
            }
        }
        out
    }
}

/// `find_all`과 같은 규칙의 빠른 사전 검사: 글자 상자를 만들기 전에 쪽을 거른다.
pub fn contains_normalized(hay: &str, query: &str) -> bool {
    let n: Vec<char> = normalize(query.chars().map(|c| (c, 0)))
        .into_iter()
        .map(|(c, _)| c)
        .collect();
    let n = trim_spaces(&n);
    if n.is_empty() {
        return false;
    }
    let h: Vec<char> = normalize(hay.chars().map(|c| (c, 0)))
        .into_iter()
        .map(|(c, _)| c)
        .collect();
    h.windows(n.len()).any(|w| w == n)
}

fn same_line(a: &RectF, b: &RectF) -> bool {
    let h = a.h().max(b.h()).max(0.1);
    (a.cy() - b.cy()).abs() < h * 0.5 && b.l >= a.l - h
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// (글자, 원래 인덱스) 열을 비교용으로 정규화.
fn normalize(it: impl Iterator<Item = (char, usize)>) -> Vec<(char, usize)> {
    let mut out: Vec<(char, usize)> = Vec::new();
    for (c, i) in it {
        if c.is_control() && !c.is_whitespace() {
            continue;
        }
        let c = if c.is_whitespace() { ' ' } else { fold(c) };
        if c == ' ' && out.last().is_some_and(|(p, _)| *p == ' ') {
            continue;
        }
        out.push((c, i));
    }
    out
}

fn trim_spaces(v: &[char]) -> &[char] {
    let s = v.iter().position(|c| *c != ' ').unwrap_or(v.len());
    let e = v.iter().rposition(|c| *c != ' ').map_or(s, |e| e + 1);
    &v[s..e]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 한 줄에 글자당 10pt 폭, 줄 높이 12pt로 배치한 가짜 쪽.
    fn page(lines: &[&str]) -> PageText {
        let mut t = PageText::default();
        for (li, line) in lines.iter().enumerate() {
            let top = li as f32 * 20.0;
            for (ci, c) in line.chars().enumerate() {
                t.chars.push(c);
                let l = ci as f32 * 10.0;
                t.boxes.push(RectF::new(l, top, l + 10.0, top + 12.0));
            }
            if li + 1 < lines.len() {
                t.chars.extend(['\r', '\n']);
                t.boxes.extend([RectF::default(), RectF::default()]);
            }
        }
        t
    }

    #[test]
    fn caret_and_char_hit() {
        let p = page(&["abc", "de"]);
        assert_eq!(p.char_at(15.0, 5.0), Some(1));
        assert_eq!(p.char_at(500.0, 5.0), None);
        assert_eq!(p.caret_at(12.0, 5.0), Some(1)); // b의 왼쪽 절반
        assert_eq!(p.caret_at(18.0, 5.0), Some(2)); // b의 오른쪽 절반
        assert_eq!(p.caret_at(999.0, 25.0), Some(7)); // 둘째 줄 끝
    }

    #[test]
    fn rects_merge_per_line() {
        let p = page(&["abc", "de"]);
        let r = p.range_rects(1..p.len());
        assert_eq!(
            r,
            vec![
                RectF::new(10.0, 0.0, 30.0, 12.0),
                RectF::new(0.0, 20.0, 20.0, 32.0)
            ]
        );
        assert!(p.range_rects(50..60).is_empty());
    }

    #[test]
    fn text_keeps_newlines() {
        let p = page(&["abc", "de"]);
        assert_eq!(p.text(1..p.len()), "bc\r\nde");
    }

    #[test]
    fn find_is_case_insensitive_and_crosses_lines() {
        let p = page(&["Hello World", "world peace"]);
        assert_eq!(p.find_all("WORLD"), vec![6..11, 13..18]);
        // 줄바꿈을 공백 하나로 본다
        assert_eq!(p.find_all("world world"), vec![6..18]);
        assert!(p.find_all("   ").is_empty());
    }

    #[test]
    fn find_hangul() {
        let p = page(&["가나다 라마", "나다"]);
        assert_eq!(p.find_all("나다"), vec![1..3, 8..10]);
    }
}

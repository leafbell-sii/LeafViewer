//! 탐색기와 같은 자연 정렬 (FR-F04).
//!
//! `StrCmpLogicalW`를 코어에서 쓸 수 없으므로(Win32 비의존) 같은 규칙을 순수 Rust로 구현한다:
//! 대소문자 무시, 숫자 구간은 수치로 비교("file2" < "file10").

use std::cmp::Ordering;

pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => break,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                let da = take_digits(&mut ai);
                let db = take_digits(&mut bi);
                let ta = da.trim_start_matches('0');
                let tb = db.trim_start_matches('0');
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(ca), Some(cb)) => {
                let ord = fold(ca).cmp(&fold(cb));
                if ord != Ordering::Equal {
                    return ord;
                }
                ai.next();
                bi.next();
            }
        }
    }
    // 논리적으로 같으면(예: "a01" vs "a1", "A" vs "a") 순서를 안정적으로 만들기 위한 보조 기준.
    a.cmp(b)
}

fn take_digits(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut s = String::new();
    while let Some(c) = it.peek().copied() {
        if !c.is_ascii_digit() {
            break;
        }
        s.push(c);
        it.next();
    }
    s
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(mut v: Vec<&str>) -> Vec<&str> {
        v.sort_by(|a, b| natural_cmp(a, b));
        v
    }

    #[test]
    fn numbers_compare_numerically() {
        assert_eq!(
            sorted(vec!["file10.txt", "file2.txt", "file1.txt"]),
            vec!["file1.txt", "file2.txt", "file10.txt"]
        );
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(sorted(vec!["b", "A", "a", "B"]), vec!["A", "a", "B", "b"]);
    }

    #[test]
    fn leading_zeros_and_prefix() {
        assert_eq!(natural_cmp("a1", "a01"), Ordering::Greater); // 보조 기준
        assert_eq!(natural_cmp("abc", "abcd"), Ordering::Less);
        assert_eq!(natural_cmp("x9", "x10"), Ordering::Less);
    }

    #[test]
    fn hangul() {
        assert_eq!(
            sorted(vec!["하늘 2장", "가나", "하늘 10장"]),
            vec!["가나", "하늘 2장", "하늘 10장"]
        );
    }

    #[test]
    fn huge_numbers_do_not_overflow() {
        assert_eq!(
            natural_cmp("v99999999999999999999999", "v100000000000000000000000"),
            Ordering::Less
        );
    }
}

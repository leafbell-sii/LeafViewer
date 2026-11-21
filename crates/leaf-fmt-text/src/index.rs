//! 희소 줄 인덱스 (SPEC §4.5): 1,024줄마다 줄 시작 오프셋 하나. 2천만 줄도 약 160KB.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::doc::TextDoc;

pub const CHECKPOINT_LINES: u64 = 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineIndex {
    /// `checkpoints[k]` = 줄 `k * 1024`의 시작 바이트
    pub checkpoints: Vec<u64>,
    /// 줄 수 (= 줄바꿈 수 + 1)
    pub lines: u64,
}

impl LineIndex {
    /// 처음부터 끝까지 훑어 만든다. `progress(처리한 바이트)`는 대략 16MB마다 불린다.
    /// 취소되면 None.
    pub fn build(
        doc: &TextDoc,
        cancel: &AtomicBool,
        mut progress: impl FnMut(usize),
    ) -> Option<LineIndex> {
        const STEP: usize = 16 << 20;
        let len = doc.len();
        let mut cps = vec![doc.start() as u64];
        let mut lines: u64 = 1;
        let mut p = doc.start();
        let mut next_report = p + STEP;
        // 구간을 나눠 찾아서 취소·진행 보고를 한다
        while p < len {
            let limit = (p + STEP).min(len);
            match doc.find_newline(p, limit + 4) {
                Some((nl, n)) if nl < limit => {
                    p = nl + n;
                    if lines.is_multiple_of(CHECKPOINT_LINES) {
                        cps.push(p as u64);
                    }
                    lines += 1;
                }
                _ => p = limit,
            }
            if p >= next_report {
                if cancel.load(Ordering::Relaxed) {
                    return None;
                }
                progress(p);
                next_report = p + STEP;
            }
        }
        Some(LineIndex {
            checkpoints: cps,
            lines,
        })
    }

    /// 행 시작 `pos`가 속한 줄 번호(0부터)
    pub fn line_of(&self, doc: &TextDoc, pos: usize) -> u64 {
        let k = self
            .checkpoints
            .partition_point(|&c| c as usize <= pos)
            .saturating_sub(1);
        let mut line = k as u64 * CHECKPOINT_LINES;
        let mut p = self.checkpoints.get(k).copied().unwrap_or(0) as usize;
        while let Some((nl, n)) = doc.find_newline(p, pos) {
            line += 1;
            p = nl + n;
        }
        line.min(self.lines.saturating_sub(1))
    }

    /// 줄 번호(0부터) → 줄 시작 바이트
    pub fn line_start(&self, doc: &TextDoc, line: u64) -> usize {
        let line = line.min(self.lines.saturating_sub(1));
        let k = (line / CHECKPOINT_LINES) as usize;
        let mut p = self.checkpoints.get(k).copied().unwrap_or(0) as usize;
        for _ in 0..line % CHECKPOINT_LINES {
            match doc.find_newline(p, usize::MAX) {
                Some((nl, n)) => p = nl + n,
                None => break,
            }
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::TextEncoding;
    use std::sync::Arc;

    #[test]
    fn index_lines_and_lookup() {
        let mut s = String::new();
        for i in 0..5000 {
            s.push_str(&format!("line {i}"));
            s.push_str(if i % 3 == 0 { "\r\n" } else { "\n" });
        }
        let d = TextDoc::new(
            Arc::new(s.into_bytes()),
            TextEncoding::new(encoding_rs::UTF_8, 0),
        );
        let ix = LineIndex::build(&d, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(ix.lines, 5001);
        assert_eq!(ix.checkpoints.len(), 5);
        for line in [0u64, 1, 1023, 1024, 1025, 4999, 5000] {
            let p = ix.line_start(&d, line);
            assert_eq!(ix.line_of(&d, p), line);
            if line < 5000 {
                let (_, t) = d.decode_row(p);
                assert_eq!(String::from_utf16(&t).unwrap(), format!("line {line}"));
            }
        }
    }

    #[test]
    fn cancel_returns_none() {
        let d = TextDoc::new(
            Arc::new(vec![b'\n'; 40 << 20]),
            TextEncoding::new(encoding_rs::UTF_8, 0),
        );
        assert!(LineIndex::build(&d, &AtomicBool::new(true), |_| {}).is_none());
    }
}

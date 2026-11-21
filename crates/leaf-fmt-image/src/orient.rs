//! EXIF 방향(태그 274). WIC는 방향을 적용하지 않으므로 직접 돌린다.
//!
//! 큰 사진의 일부만 디코딩할 때(고배율)도 쓸 수 있게, "보이는 방향" 좌표를 저장 방향
//! 좌표로 옮기는 함수([`Orientation::src_rect`])와 디코딩한 조각을 돌리는 함수
//! ([`Orientation::apply`])를 따로 둔다.

/// 1 = 그대로, 2 = 좌우 반전, 3 = 180°, 4 = 상하 반전, 5 = 전치, 6 = 시계 90°, 7 = 반전치, 8 = 반시계 90°
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Orientation(u8);

impl Orientation {
    pub const NORMAL: Self = Self(1);

    /// 범위 밖 값은 "그대로"로 본다
    pub fn from_exif(v: u16) -> Self {
        if (1..=8).contains(&v) {
            Self(v as u8)
        } else {
            Self::NORMAL
        }
    }

    pub fn value(self) -> u8 {
        self.0.max(1)
    }

    pub fn is_identity(self) -> bool {
        self.value() == 1
    }

    /// 가로·세로가 바뀌는 방향(5–8)
    pub fn swaps_axes(self) -> bool {
        self.value() >= 5
    }

    /// 저장 크기 → 보이는 크기
    pub fn display_size(self, sw: u32, sh: u32) -> (u32, u32) {
        if self.swaps_axes() {
            (sh, sw)
        } else {
            (sw, sh)
        }
    }

    /// 보이는 좌표 (x, y) → 저장 좌표 (u, v). `ow`·`oh`는 보이는 크기
    fn to_src(self, x: u32, y: u32, ow: u32, oh: u32) -> (u32, u32) {
        let (rx, ry) = (ow - 1 - x, oh - 1 - y);
        match self.value() {
            2 => (rx, y),
            3 => (rx, ry),
            4 => (x, ry),
            5 => (y, x),
            6 => (y, rx),
            7 => (ry, rx),
            8 => (ry, x),
            _ => (x, y),
        }
    }

    /// 보이는 방향의 사각형 `(x, y, w, h)` → 그 픽셀들을 담는 저장 방향의 사각형
    pub fn src_rect(self, r: (u32, u32, u32, u32), ow: u32, oh: u32) -> (u32, u32, u32, u32) {
        let (x, y, w, h) = r;
        if w == 0 || h == 0 {
            return (0, 0, 0, 0);
        }
        let a = self.to_src(x, y, ow, oh);
        let b = self.to_src(x + w - 1, y + h - 1, ow, oh);
        let (u0, u1) = (a.0.min(b.0), a.0.max(b.0));
        let (v0, v1) = (a.1.min(b.1), a.1.max(b.1));
        (u0, v0, u1 - u0 + 1, v1 - v0 + 1)
    }

    /// 저장 방향의 BGRA 조각(`sw`×`sh`, stride = sw × 4)을 보이는 방향으로 돌린다.
    /// 돌린 조각의 크기는 [`display_size`](Self::display_size)다.
    pub fn apply(self, src: &[u8], sw: u32, sh: u32) -> Vec<u8> {
        let (sw, sh) = (sw as usize, sh as usize);
        debug_assert_eq!(src.len(), sw * sh * 4);
        if self.is_identity() {
            return src.to_vec();
        }
        let (ow, oh) = self.display_size(sw as u32, sh as u32);
        let mut out = vec![0u8; src.len()];
        for y in 0..oh {
            for x in 0..ow {
                let (u, v) = self.to_src(x, y, ow, oh);
                let s = (v as usize * sw + u as usize) * 4;
                let d = (y as usize * ow as usize + x as usize) * 4;
                out[d..d + 4].copy_from_slice(&src[s..s + 4]);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3×2 저장 이미지: 픽셀 값 = 행*3+열 (B 채널에 넣는다)
    ///   0 1 2
    ///   3 4 5
    fn sample() -> Vec<u8> {
        (0..6u8).flat_map(|i| [i, 0, 0, 255]).collect()
    }

    fn rows(o: Orientation) -> Vec<Vec<u8>> {
        let out = o.apply(&sample(), 3, 2);
        let (ow, _) = o.display_size(3, 2);
        out.chunks(ow as usize * 4)
            .map(|r| r.chunks(4).map(|p| p[0]).collect())
            .collect()
    }

    #[test]
    fn all_eight() {
        let o = Orientation::from_exif;
        assert_eq!(rows(o(1)), [vec![0, 1, 2], vec![3, 4, 5]]);
        assert_eq!(rows(o(2)), [vec![2, 1, 0], vec![5, 4, 3]]);
        assert_eq!(rows(o(3)), [vec![5, 4, 3], vec![2, 1, 0]]);
        assert_eq!(rows(o(4)), [vec![3, 4, 5], vec![0, 1, 2]]);
        // 전치
        assert_eq!(rows(o(5)), [vec![0, 3], vec![1, 4], vec![2, 5]]);
        // 시계 방향 90°: 왼쪽 열(0,3)이 맨 위 행이 되고, 아래쪽 행이 왼쪽으로
        assert_eq!(rows(o(6)), [vec![3, 0], vec![4, 1], vec![5, 2]]);
        assert_eq!(rows(o(7)), [vec![5, 2], vec![4, 1], vec![3, 0]]);
        // 반시계 90°
        assert_eq!(rows(o(8)), [vec![2, 5], vec![1, 4], vec![0, 3]]);
        assert_eq!(o(0), Orientation::NORMAL);
        assert_eq!(o(9), Orientation::NORMAL);
    }

    #[test]
    fn src_rect_covers_region() {
        // 보이는 방향에서 일부를 잘라 돌린 결과 = 전체를 돌린 뒤 같은 부분을 자른 결과
        let (sw, sh) = (7u32, 5u32);
        let src: Vec<u8> = (0..sw * sh)
            .flat_map(|i| [(i % 256) as u8, (i / 256) as u8, 0, 255])
            .collect();
        for v in 1..=8 {
            let o = Orientation::from_exif(v);
            let (ow, oh) = o.display_size(sw, sh);
            let full = o.apply(&src, sw, sh);
            let r = (1, 2, ow - 2, oh - 3);
            let (u, vv, w, h) = o.src_rect(r, ow, oh);
            // 저장 방향 조각 잘라내기
            let mut piece = Vec::new();
            for yy in vv..vv + h {
                let s = ((yy * sw + u) * 4) as usize;
                piece.extend_from_slice(&src[s..s + (w * 4) as usize]);
            }
            let rot = o.apply(&piece, w, h);
            let (pw, ph) = o.display_size(w, h);
            assert_eq!((pw, ph), (r.2, r.3), "orientation {v}");
            for yy in 0..ph {
                for xx in 0..pw {
                    let a = ((yy * pw + xx) * 4) as usize;
                    let b = (((r.1 + yy) * ow + r.0 + xx) * 4) as usize;
                    assert_eq!(
                        rot[a..a + 4],
                        full[b..b + 4],
                        "orientation {v} at {xx},{yy}"
                    );
                }
            }
        }
    }
}

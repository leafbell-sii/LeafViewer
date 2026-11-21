/// 축 정렬 사각형. 좌표계는 쓰는 곳이 정한다(쪽 pt 또는 DIP). y는 아래로 증가.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RectF {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl RectF {
    pub const fn new(l: f32, t: f32, r: f32, b: f32) -> Self {
        Self { l, t, r, b }
    }

    pub fn w(&self) -> f32 {
        self.r - self.l
    }

    pub fn h(&self) -> f32 {
        self.b - self.t
    }

    pub fn cy(&self) -> f32 {
        (self.t + self.b) * 0.5
    }

    pub fn is_empty(&self) -> bool {
        self.r <= self.l || self.b <= self.t
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.l && x <= self.r && y >= self.t && y <= self.b
    }

    pub fn union(&self, o: &RectF) -> RectF {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        RectF::new(
            self.l.min(o.l),
            self.t.min(o.t),
            self.r.max(o.r),
            self.b.max(o.b),
        )
    }

    pub fn intersects(&self, o: &RectF) -> bool {
        self.l < o.r && o.l < self.r && self.t < o.b && o.t < self.b
    }

    /// 점까지의 거리 제곱 (안에 있으면 0)
    pub fn dist2(&self, x: f32, y: f32) -> f32 {
        let dx = (self.l - x).max(0.0).max(x - self.r);
        let dy = (self.t - y).max(0.0).max(y - self.b);
        dx * dx + dy * dy
    }
}

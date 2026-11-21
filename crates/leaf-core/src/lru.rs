//! 바이트 예산 LRU (SPEC §4.5 "모든 캐시는 예산을 가진다").
//! 항목 수가 수백 개 수준이라 접근 시각 스탬프 + 선형 퇴출로 충분하다.

use std::collections::HashMap;
use std::hash::Hash;

pub struct BudgetLru<K, V> {
    map: HashMap<K, Entry<V>>,
    budget: usize,
    used: usize,
    clock: u64,
}

struct Entry<V> {
    value: V,
    cost: usize,
    stamp: u64,
}

impl<K: Eq + Hash + Clone, V> BudgetLru<K, V> {
    pub fn new(budget: usize) -> Self {
        Self {
            map: HashMap::new(),
            budget,
            used: 0,
            clock: 0,
        }
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    pub fn used(&self) -> usize {
        self.used
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
        self.evict(&[]);
    }

    pub fn contains(&self, k: &K) -> bool {
        self.map.contains_key(k)
    }

    /// 조회하면서 최근 사용으로 표시한다.
    pub fn get(&mut self, k: &K) -> Option<&V> {
        self.clock += 1;
        let clock = self.clock;
        self.map.get_mut(k).map(|e| {
            e.stamp = clock;
            &e.value
        })
    }

    /// 최근 사용 표시 없이 조회.
    pub fn peek(&self, k: &K) -> Option<&V> {
        self.map.get(k).map(|e| &e.value)
    }

    /// 넣고 예산을 넘으면 오래된 것부터 버린다. `keep`에 든 키는 이번 퇴출에서 보호한다
    /// (예: 지금 화면에 보이는 타일).
    pub fn insert(&mut self, k: K, value: V, cost: usize, keep: &[K]) {
        self.clock += 1;
        if let Some(old) = self.map.insert(
            k,
            Entry {
                value,
                cost,
                stamp: self.clock,
            },
        ) {
            self.used -= old.cost;
        }
        self.used += cost;
        self.evict(keep);
    }

    pub fn remove(&mut self, k: &K) -> Option<V> {
        self.map.remove(k).map(|e| {
            self.used -= e.cost;
            e.value
        })
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.used = 0;
    }

    pub fn retain(&mut self, mut f: impl FnMut(&K) -> bool) {
        let used = &mut self.used;
        self.map.retain(|k, e| {
            let keep = f(k);
            if !keep {
                *used -= e.cost;
            }
            keep
        });
    }

    fn evict(&mut self, keep: &[K]) {
        while self.used > self.budget {
            let victim = self
                .map
                .iter()
                .filter(|(k, _)| !keep.contains(k))
                .min_by_key(|(_, e)| e.stamp)
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    self.remove(&k);
                }
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recent_within_budget() {
        let mut c = BudgetLru::new(100);
        c.insert(1, "a", 40, &[]);
        c.insert(2, "b", 40, &[]);
        c.get(&1); // 1을 최근으로
        c.insert(3, "c", 40, &[]); // 120 > 100 → 2 퇴출
        assert!(c.contains(&1) && !c.contains(&2) && c.contains(&3));
        assert_eq!(c.used(), 80);
    }

    #[test]
    fn keep_protects_and_replace_updates_cost() {
        let mut c = BudgetLru::new(50);
        c.insert(1, (), 30, &[]);
        c.insert(2, (), 30, &[1, 2]); // 둘 다 보호 → 예산 초과 허용
        assert_eq!(c.len(), 2);
        c.insert(2, (), 10, &[]);
        assert_eq!(c.used(), 40);
        c.set_budget(15);
        assert_eq!(c.len(), 1);
        c.retain(|_| false);
        assert_eq!((c.len(), c.used()), (0, 0));
    }
}

//! 폴더 트리 모델 (SPEC §3.4 "트리 모델과 뷰 분리", §4.5 "펼친 폴더의 자식만 메모리에").
//! 뷰(leaf-win의 가상 리스트)는 `visible()`이 돌려준 행만 그린다 — 컨트롤에 항목을 넣지 않으므로
//! 1만 항목 폴더도 펼치는 비용이 목록 읽기 + 정렬뿐이다 (Q9, NFR-P08).
//!
//! 노드 번호는 루트를 바꾸기 전까지 바뀌지 않는다. 지워진 노드는 `alive = false`로 남는다
//! (뷰가 들고 있는 번호가 엉뚱한 노드를 가리키지 않도록).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::fs_tree::Entry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState {
    NotLoaded,
    Loading,
    Loaded,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Node {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub state: LoadState,
    pub expanded: bool,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub alive: bool,
}

/// 보이는 행 하나
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisRow {
    Node {
        idx: usize,
        depth: u16,
    },
    /// 폴더 아래 "불러오는 중…" / 오류 문구 행
    Status {
        parent: usize,
        depth: u16,
    },
}

#[derive(Default)]
pub struct TreeModel {
    pub nodes: Vec<Node>,
    /// 루트가 바뀔 때마다 증가. 이전 루트의 워커 결과를 버린다.
    pub generation: u64,
}

fn eq_name(a: &OsStr, b: &OsStr) -> bool {
    // Windows 파일 이름은 대소문자를 구분하지 않는다
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

impl TreeModel {
    pub fn set_root(&mut self, root: &Path) {
        self.generation += 1;
        self.nodes.clear();
        let name = root
            .file_name()
            .unwrap_or(root.as_os_str())
            .to_string_lossy()
            .into_owned();
        self.nodes.push(Node {
            path: root.to_path_buf(),
            name,
            is_dir: true,
            state: LoadState::NotLoaded,
            expanded: false,
            parent: None,
            children: Vec::new(),
            alive: true,
        });
    }

    pub fn root(&self) -> Option<&Node> {
        self.nodes.first()
    }

    pub fn get(&self, idx: usize) -> Option<&Node> {
        self.nodes.get(idx).filter(|n| n.alive)
    }

    /// 펼침 표시를 그릴지: 읽기 전 폴더는 "있을 수 있음"(FR-F02)
    pub fn expandable(&self, idx: usize) -> bool {
        match self.get(idx) {
            Some(n) if n.is_dir => match n.state {
                LoadState::Loaded => !n.children.is_empty(),
                _ => true,
            },
            _ => false,
        }
    }

    /// 펼치기/접기. 펼쳤는데 아직 읽지 않은 폴더면 `Loading`으로 바꾸고 true(읽기 요청 필요).
    pub fn set_expanded(&mut self, idx: usize, expanded: bool) -> bool {
        let Some(n) = self.nodes.get_mut(idx).filter(|n| n.alive && n.is_dir) else {
            return false;
        };
        n.expanded = expanded;
        if expanded && n.state == LoadState::NotLoaded {
            n.state = LoadState::Loading;
            return true;
        }
        false
    }

    /// 폴더 목록 결과 반영. 처음 읽기든 다시 읽기(파일 감시)든 같다: 이름이 같은 자식은 그대로 두고
    /// (펼침 상태·번호 유지) 없어진 자식은 지우고 새 자식을 끼운다. `entries`는 이미 정렬돼 있다.
    /// 새로 생긴 자식 노드 번호를 돌려준다.
    pub fn apply_listing(&mut self, idx: usize, result: Result<Vec<Entry>, String>) -> Vec<usize> {
        if self.get(idx).is_none() {
            return Vec::new();
        }
        let entries = match result {
            Ok(e) => e,
            Err(e) => {
                let old = std::mem::take(&mut self.nodes[idx].children);
                for c in old {
                    self.kill(c);
                }
                self.nodes[idx].state = LoadState::Failed(e);
                return Vec::new();
            }
        };
        let parent_path = self.nodes[idx].path.clone();
        let old = std::mem::take(&mut self.nodes[idx].children);
        let mut children = Vec::with_capacity(entries.len());
        let mut added = Vec::new();
        // 자식 수가 많을 수 있으므로(1만+) 이름 → 기존 노드를 한 번에 대응시킨다
        let mut by_name: std::collections::HashMap<String, usize> = old
            .iter()
            .map(|&c| (self.nodes[c].name.to_lowercase(), c))
            .collect();
        for e in entries {
            let key = e.name.to_string_lossy().to_lowercase();
            match by_name.remove(&key) {
                Some(c) if self.nodes[c].is_dir == e.is_dir => {
                    // 대소문자만 바뀐 이름 변경도 반영
                    let n = &mut self.nodes[c];
                    if n.name != e.name.to_string_lossy() {
                        n.name = e.name.to_string_lossy().into_owned();
                        n.path = parent_path.join(&e.name);
                    }
                    children.push(c);
                }
                other => {
                    if let Some(c) = other {
                        by_name.insert(key, c); // 종류가 바뀐 항목: 옛 노드는 아래에서 지운다
                    }
                    let i = self.nodes.len();
                    self.nodes.push(Node {
                        path: parent_path.join(&e.name),
                        name: e.name.to_string_lossy().into_owned(),
                        is_dir: e.is_dir,
                        state: LoadState::NotLoaded,
                        expanded: false,
                        parent: Some(idx),
                        children: Vec::new(),
                        alive: true,
                    });
                    children.push(i);
                    added.push(i);
                }
            }
        }
        // 남은 것 = 없어진 항목
        for c in by_name.into_values() {
            self.kill(c);
        }
        let n = &mut self.nodes[idx];
        n.children = children;
        n.state = LoadState::Loaded;
        added
    }

    fn kill(&mut self, idx: usize) {
        let mut stack = vec![idx];
        while let Some(i) = stack.pop() {
            let n = &mut self.nodes[i];
            n.alive = false;
            stack.extend(std::mem::take(&mut n.children));
        }
    }

    /// 펼친 폴더를 따라 보이는 행 목록
    pub fn visible(&self) -> Vec<VisRow> {
        let mut out = Vec::new();
        if self.nodes.is_empty() {
            return out;
        }
        let mut stack: Vec<(usize, u16)> = vec![(0, 0)];
        while let Some((i, d)) = stack.pop() {
            out.push(VisRow::Node { idx: i, depth: d });
            let n = &self.nodes[i];
            if !(n.is_dir && n.expanded) {
                continue;
            }
            match n.state {
                LoadState::Loading | LoadState::Failed(_) => out.push(VisRow::Status {
                    parent: i,
                    depth: d + 1,
                }),
                _ => stack.extend(n.children.iter().rev().map(|&c| (c, d + 1))),
            }
        }
        out
    }

    pub fn find_child(&self, idx: usize, name: &OsStr) -> Option<usize> {
        self.get(idx)?
            .children
            .iter()
            .copied()
            .find(|&c| eq_name(OsStr::new(&self.nodes[c].name), name))
    }

    /// 경로에 해당하는 (읽어 둔) 노드
    pub fn find_path(&self, p: &Path) -> Option<usize> {
        let rel = p.strip_prefix(&self.root()?.path).ok()?;
        let mut cur = 0;
        for c in rel.iter() {
            cur = self.find_child(cur, c)?;
        }
        Some(cur)
    }

    /// 다 읽어 둔(또는 읽다 실패한) 살아 있는 폴더 — 파일 감시 "모두 다시 읽기"용
    pub fn listed_dirs(&self) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|&i| {
                let n = &self.nodes[i];
                n.alive && n.is_dir && matches!(n.state, LoadState::Loaded | LoadState::Failed(_))
            })
            .collect()
    }

    /// 펼쳐 둔 폴더 경로 (세션 저장용, 루트 제외)
    pub fn expanded_paths(&self) -> Vec<PathBuf> {
        self.nodes
            .iter()
            .skip(1)
            .filter(|n| n.alive && n.is_dir && n.expanded && self.ancestors_expanded(n))
            .map(|n| n.path.clone())
            .collect()
    }

    fn ancestors_expanded(&self, n: &Node) -> bool {
        let mut p = n.parent;
        while let Some(i) = p {
            if !self.nodes[i].expanded {
                return false;
            }
            p = self.nodes[i].parent;
        }
        true
    }

    pub fn parent_of(&self, idx: usize) -> Option<usize> {
        self.get(idx)?.parent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn ents(v: &[(&str, bool)]) -> Vec<Entry> {
        v.iter()
            .map(|&(n, d)| Entry {
                name: OsString::from(n),
                is_dir: d,
            })
            .collect()
    }

    fn names(m: &TreeModel) -> Vec<String> {
        m.visible()
            .into_iter()
            .map(|r| match r {
                VisRow::Node { idx, depth } => {
                    format!("{}{}", " ".repeat(depth as usize), m.nodes[idx].name)
                }
                VisRow::Status { depth, .. } => format!("{}…", " ".repeat(depth as usize)),
            })
            .collect()
    }

    #[test]
    fn expand_list_and_diff() {
        let mut m = TreeModel::default();
        m.set_root(Path::new(r"C:\R"));
        assert!(m.expandable(0));
        assert!(m.set_expanded(0, true));
        assert_eq!(names(&m), ["R", " …"]);
        m.apply_listing(
            0,
            Ok(ents(&[("a", true), ("b.md", false), ("c.txt", false)])),
        );
        assert_eq!(names(&m), ["R", " a", " b.md", " c.txt"]);
        let a = m.find_child(0, OsStr::new("A")).unwrap();
        assert!(m.set_expanded(a, true));
        m.apply_listing(a, Ok(ents(&[("x.pdf", false)])));
        assert_eq!(names(&m), ["R", " a", "  x.pdf", " b.md", " c.txt"]);
        assert_eq!(m.find_path(Path::new(r"C:\R\a\x.pdf")), Some(a + 3));
        assert_eq!(m.expanded_paths(), [PathBuf::from(r"C:\R\a")]);

        // 다시 읽기: b.md 삭제, d.md 추가, A 폴더 대소문자 변경 → 펼침·번호 유지
        let added = m.apply_listing(
            0,
            Ok(ents(&[("A", true), ("c.txt", false), ("d.md", false)])),
        );
        assert_eq!(added.len(), 1);
        assert_eq!(names(&m), ["R", " A", "  x.pdf", " c.txt", " d.md"]);
        assert_eq!(m.find_child(0, OsStr::new("a")), Some(a));
        assert_eq!(m.nodes[a].path, PathBuf::from(r"C:\R\A"));
        assert_eq!(m.listed_dirs(), [0, a]);

        // 폴더가 같은 이름의 파일로 바뀜 → 옛 하위 노드는 죽는다
        m.apply_listing(0, Ok(ents(&[("A", false)])));
        assert_eq!(names(&m), ["R", " A"]);
        assert!(m.get(a).is_none());
        assert!(!m.expandable(m.find_child(0, OsStr::new("A")).unwrap()));

        // 읽기 실패
        m.apply_listing(0, Err("거부됨".into()));
        assert_eq!(names(&m), ["R", " …"]);
        assert_eq!(m.nodes[0].state, LoadState::Failed("거부됨".into()));
    }

    #[test]
    fn collapsed_children_hidden_and_not_saved() {
        let mut m = TreeModel::default();
        m.set_root(Path::new(r"C:\R"));
        m.set_expanded(0, true);
        m.apply_listing(0, Ok(ents(&[("a", true)])));
        m.set_expanded(1, true);
        m.apply_listing(1, Ok(ents(&[("b", true)])));
        m.set_expanded(2, true);
        m.apply_listing(2, Ok(ents(&[])));
        assert!(!m.expandable(2));
        m.set_expanded(1, false);
        assert_eq!(names(&m), ["R", " a"]);
        assert!(m.expanded_paths().is_empty());
        // 다시 펼치면 이미 읽었으므로 읽기 요청 없음
        assert!(!m.set_expanded(1, true));
        assert_eq!(m.expanded_paths().len(), 2);
    }

    #[test]
    fn large_folder_listing_is_fast() {
        let mut m = TreeModel::default();
        m.set_root(Path::new(r"C:\R"));
        m.set_expanded(0, true);
        let v: Vec<Entry> = (0..20_000)
            .map(|i| Entry {
                name: OsString::from(format!("file{i:05}.txt")),
                is_dir: false,
            })
            .collect();
        let t = std::time::Instant::now();
        m.apply_listing(0, Ok(v.clone()));
        m.apply_listing(0, Ok(v));
        assert_eq!(m.visible().len(), 20_001);
        assert!(t.elapsed().as_millis() < 500, "{:?}", t.elapsed());
    }
}

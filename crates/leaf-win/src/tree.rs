//! 폴더 트리 모델 (SPEC §3.4 "트리 모델과 뷰 분리", §4.5 "파일명은 Rust 모델에만").
//! TreeView 항목의 lParam = 이 모델의 노드 인덱스. 텍스트·자식 여부는 콜백으로 조회된다.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadState {
    NotLoaded,
    Loading,
    Loaded { has_children: bool },
    Failed,
}

pub struct Node {
    pub path: PathBuf,
    /// NUL로 끝나는 UTF-16 표시 이름. 힙 버퍼라 `nodes`가 재할당돼도 주소가 유지된다.
    pub name: Vec<u16>,
    pub is_dir: bool,
    pub state: LoadState,
    /// HTREEITEM 값
    pub item: isize,
    /// 로딩 중 표시용 자리표시자 항목
    pub placeholder: isize,
}

#[derive(Default)]
pub struct TreeModel {
    pub nodes: Vec<Node>,
    /// 루트가 바뀔 때마다 증가. 이전 루트의 워커 결과를 버린다.
    pub generation: u64,
}

impl TreeModel {
    pub fn get(&self, idx: usize) -> Option<&Node> {
        self.nodes.get(idx)
    }

    /// TreeView가 "펼침 버튼"을 그릴지: 읽기 전 폴더는 "있을 수 있음"(FR-F02).
    pub fn has_children(&self, idx: usize) -> bool {
        match self.nodes.get(idx) {
            Some(n) if n.is_dir => match n.state {
                LoadState::Loaded { has_children } => has_children,
                _ => true,
            },
            _ => false,
        }
    }
}

//! 파일 감시 (FR-F05, FR-V06, FR-A07). `notify`(Windows에서는 ReadDirectoryChangesW)를 감싼다.
//! 디바운스는 받는 쪽(UI)이 한다 — 여기서는 경로만 넘긴다.

use std::path::{Path, PathBuf};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// 바뀐(생성·수정·삭제·이름 변경) 경로들
    Changed(Vec<PathBuf>),
    /// 이벤트가 넘쳐 일부를 놓쳤다 → 감시 중인 것을 모두 다시 읽어야 한다
    Rescan,
}

/// 감시 대상: (경로, 하위 폴더 포함 여부)
pub type Target = (PathBuf, bool);

pub struct Watcher {
    inner: Option<RecommendedWatcher>,
    watched: Vec<Target>,
}

impl Watcher {
    /// 콜백은 감시 스레드에서 불린다.
    pub fn new(cb: impl Fn(WatchEvent) + Send + 'static) -> Self {
        let inner =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
                Ok(ev) if ev.need_rescan() => cb(WatchEvent::Rescan),
                Ok(ev) => {
                    if !matches!(ev.kind, EventKind::Access(_)) && !ev.paths.is_empty() {
                        cb(WatchEvent::Changed(ev.paths));
                    }
                }
                // 감시하던 폴더가 사라지는 등. 다시 읽어 실제 상태를 반영한다
                Err(_) => cb(WatchEvent::Rescan),
            })
            .ok();
        Self {
            inner,
            watched: Vec::new(),
        }
    }

    /// 감시 대상을 통째로 바꾼다(차이만 반영). 감시할 수 없는 경로(권한·끊긴 드라이브)는 건너뛰고
    /// 그 오류 문구들을 돌려준다.
    pub fn set(&mut self, targets: &[Target]) -> Vec<String> {
        let Some(w) = self.inner.as_mut() else {
            return vec!["파일 감시를 시작할 수 없습니다".into()];
        };
        let mut errors = Vec::new();
        self.watched.retain(|t| {
            let keep = targets.contains(t);
            if !keep {
                let _ = w.unwatch(&t.0);
            }
            keep
        });
        for t in targets {
            if self.watched.contains(t) {
                continue;
            }
            let mode = if t.1 {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            match w.watch(&t.0, mode) {
                Ok(()) => self.watched.push(t.clone()),
                Err(e) => errors.push(format!("{}: {e}", t.0.display())),
            }
        }
        errors
    }

    pub fn is_watching(&self, p: &Path) -> bool {
        self.watched.iter().any(|t| t.0 == p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn reports_changes_in_watched_folder() {
        let dir = std::env::temp_dir().join(format!("leaf-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut w = Watcher::new(move |e| {
            let _ = tx.send(e);
        });
        assert!(w.set(&[(dir.clone(), true)]).is_empty());
        assert!(w.is_watching(&dir));
        std::fs::write(dir.join("sub").join("a.txt"), b"x").unwrap();
        let want = dir.join("sub").join("a.txt");
        let mut seen = false;
        while let Ok(e) = rx.recv_timeout(Duration::from_secs(5)) {
            if let WatchEvent::Changed(ps) = e
                && ps.iter().any(|p| p == &want)
            {
                seen = true;
                break;
            }
        }
        assert!(seen);
        // 없는 경로는 오류로 돌려주고 죽지 않는다
        assert_eq!(w.set(&[(dir.join("nope"), false)]).len(), 1);
        assert!(!w.is_watching(&dir));
        drop(w);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

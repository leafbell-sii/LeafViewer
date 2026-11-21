//! 세션 저장·복원 (FR-A02 기초, FR-V09). `%LOCALAPPDATA%\LeafViewer\session.toml` — 앱이 관리.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 파일별 마지막 위치를 기억할 최대 개수 (FR-V09).
pub const MAX_RECENT: usize = 50;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Session {
    pub root: Option<PathBuf>,
    pub open_file: Option<PathBuf>,
    pub window: Option<WindowState>,
    /// 최근 연 파일 위치. 앞쪽이 최신.
    pub recent: Vec<FilePos>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct WindowState {
    /// 복원(normal) 상태 사각형, 물리 픽셀
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub maximized: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FilePos {
    pub path: PathBuf,
    /// 쪽 번호(0부터)
    pub page: u32,
    /// 쪽 위에서부터 화면 맨 위까지(pt)
    pub y_pt: f32,
    /// "fit-width" | "fit-page" | 배율(1.0 = 100%)
    pub zoom: String,
    /// TXT: 화면 맨 위 행의 파일 바이트 오프셋 (이때 `y_pt`는 그 행 안의 DIP)
    pub offset: u64,
}

pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("LeafViewer").join("session.toml"))
}

impl Session {
    /// 파일이 없거나 깨졌으면 빈 세션 (앱이 죽지 않는다).
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// 임시 파일에 쓰고 바꿔치기해 중간에 끊겨도 기존 세션이 깨지지 않게 한다.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }

    pub fn remember(&mut self, pos: FilePos) {
        self.recent.retain(|p| !same_path(&p.path, &pos.path));
        self.recent.insert(0, pos);
        self.recent.truncate(MAX_RECENT);
    }

    pub fn position_of(&self, path: &Path) -> Option<&FilePos> {
        self.recent.iter().find(|p| same_path(&p.path, path))
    }
}

/// Windows 경로는 대소문자를 구분하지 않는다.
fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_recent_order() {
        let dir = std::env::temp_dir().join(format!("leaf-session-{}", std::process::id()));
        let path = dir.join("session.toml");
        let mut s = Session {
            root: Some(PathBuf::from(r"C:\Docs")),
            open_file: Some(PathBuf::from(r"C:\Docs\가.pdf")),
            window: Some(WindowState {
                x: 10,
                y: 20,
                w: 800,
                h: 600,
                maximized: true,
            }),
            recent: vec![],
        };
        for i in 0..(MAX_RECENT + 5) {
            s.remember(FilePos {
                path: PathBuf::from(format!(r"C:\Docs\{i}.pdf")),
                page: i as u32,
                y_pt: 1.5,
                zoom: "fit-width".into(),
                offset: i as u64,
            });
        }
        s.remember(FilePos {
            path: PathBuf::from(r"c:\docs\10.PDF"),
            page: 99,
            ..Default::default()
        });
        assert_eq!(s.recent.len(), MAX_RECENT);
        assert_eq!(
            s.position_of(Path::new(r"C:\Docs\10.pdf")).unwrap().page,
            99
        );

        s.save(&path).unwrap();
        assert_eq!(Session::load(&path), s);
        std::fs::write(&path, "garbage = [").unwrap();
        assert_eq!(Session::load(&path), Session::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

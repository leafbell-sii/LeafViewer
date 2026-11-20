//! 폴더 한 단계 읽기 (FR-F02 지연 로딩, FR-F03 필터, FR-F04 정렬).
//! UI 스레드에서 호출하지 않는다 — 워커에서 실행한다 (FR-F08).

use std::ffi::OsString;
use std::io;
use std::path::Path;

use crate::natural::natural_cmp;

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: OsString,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ListOptions {
    pub show_all_files: bool,
    pub show_hidden: bool,
}

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;

/// `dir`의 직계 자식을 읽어 "폴더 먼저 + 자연 정렬"로 돌려준다.
/// 개별 항목의 메타데이터 오류는 건너뛴다(목록 전체를 실패시키지 않음).
pub fn list_dir(dir: &Path, opts: ListOptions) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for ent in std::fs::read_dir(dir)? {
        let Ok(ent) = ent else { continue };
        let Ok(meta) = ent.metadata() else { continue };
        if !opts.show_hidden && is_hidden(&meta) {
            continue;
        }
        let is_dir = meta.is_dir();
        let name = ent.file_name();
        if !is_dir && !opts.show_all_files {
            let ok = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(crate::is_supported_ext);
            if !ok {
                continue;
            }
        }
        out.push(Entry { name, is_dir });
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| natural_cmp(&a.name.to_string_lossy(), &b.name.to_string_lossy()))
    });
    Ok(out)
}

#[cfg(windows)]
fn is_hidden(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
}

#[cfg(not(windows))]
fn is_hidden(_meta: &std::fs::Metadata) -> bool {
    let _ = (FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirs_first_filtered_and_sorted() {
        let root = std::env::temp_dir().join(format!("leaf-core-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("b_dir")).unwrap();
        std::fs::create_dir_all(root.join("a_dir")).unwrap();
        for f in [
            "doc10.pdf",
            "doc2.pdf",
            "note.md",
            "image.png",
            "readme.TXT",
        ] {
            std::fs::write(root.join(f), b"x").unwrap();
        }

        let names = |opts| {
            list_dir(&root, opts)
                .unwrap()
                .into_iter()
                .map(|e| e.name.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            names(ListOptions::default()),
            [
                "a_dir",
                "b_dir",
                "doc2.pdf",
                "doc10.pdf",
                "note.md",
                "readme.TXT"
            ]
        );
        assert!(
            names(ListOptions {
                show_all_files: true,
                ..Default::default()
            })
            .contains(&"image.png".to_string())
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_dir_is_error_not_panic() {
        assert!(list_dir(Path::new(r"Z:\definitely\not\here"), ListOptions::default()).is_err());
    }
}

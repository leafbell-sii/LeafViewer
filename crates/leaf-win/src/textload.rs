//! TXT 파일 열기: 작은 파일은 메모리로 읽고 핸들을 바로 닫고, 큰 파일은 메모리 매핑 (SPEC §4.5).
//! 매핑은 unsafe가 필요해 leaf-win에 둔다(`leaf-fmt-text`는 `Source` trait만 안다).

use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::sync::Arc;

use leaf_fmt_text::{Source, TextDoc, encoding};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Globalization::GetACP;
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READONLY,
    UnmapViewOfFile,
};

struct MappedFile {
    base: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

// 읽기 전용 뷰. 매핑된 파일은 다른 프로세스가 크기를 바꿀 수 없으므로(ERROR_USER_MAPPED_FILE)
// 뷰가 살아 있는 동안 slice는 유효하다.
unsafe impl Send for MappedFile {}
unsafe impl Sync for MappedFile {}

impl Source for MappedFile {
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.base.Value as *const u8, self.len) }
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.base);
        }
    }
}

fn map_file(f: &std::fs::File, len: u64) -> Result<MappedFile, String> {
    unsafe {
        let h = CreateFileMappingW(HANDLE(f.as_raw_handle()), None, PAGE_READONLY, 0, 0, None)
            .map_err(|e| e.to_string())?;
        let base = MapViewOfFile(h, FILE_MAP_READ, 0, 0, 0);
        // 뷰가 매핑 객체를 붙잡고 있으므로 핸들은 바로 닫는다
        let _ = CloseHandle(h);
        if base.Value.is_null() {
            return Err(windows::core::Error::from_thread().to_string());
        }
        Ok(MappedFile {
            base,
            len: len as usize,
        })
    }
}

pub struct Loaded {
    pub doc: Arc<TextDoc>,
    pub size: u64,
    pub mapped: bool,
}

/// 워커에서 호출. std의 `File::open`은 READ|WRITE|DELETE 공유로 연다 (§6.2).
/// `mmap_threshold` 이상이면 메모리 매핑 (설정 `[text] mmap_threshold_mb`, §4.5).
pub fn load(path: &Path, mmap_threshold: u64) -> Result<Loaded, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let size = f.metadata().map_err(|e| e.to_string())?.len();
    let mapped = size >= mmap_threshold;
    let src: Arc<dyn Source> = if mapped {
        Arc::new(map_file(&f, size)?)
    } else {
        let mut buf = Vec::with_capacity(size as usize);
        f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        Arc::new(buf)
    };
    drop(f);
    let enc = encoding::detect(src.bytes(), unsafe { GetACP() });
    Ok(Loaded {
        doc: Arc::new(TextDoc::new(src, enc)),
        size,
        mapped,
    })
}

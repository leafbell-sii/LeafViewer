//! `leafviewer.exe [폴더|파일]` (FR-A01)
#![windows_subsystem = "windows"]
#![forbid(unsafe_code)]

use std::path::PathBuf;

fn main() {
    let path = std::env::args_os()
        .skip(1)
        .find(|a| !a.to_string_lossy().starts_with("--"))
        .map(PathBuf::from);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    let bench_scroll = std::env::args_os().any(|a| a == "--bench-scroll");
    let new_instance = std::env::args_os().any(|a| a == "--new-instance");
    let opts = leaf_win::StartOptions {
        path,
        bench_scroll,
        new_instance,
        pdfium_path: exe_dir.join("pdfium.dll"),
    };
    if let Err(e) = leaf_win::run(opts) {
        // GUI 서브시스템이라 콘솔이 없다 → 로그 파일에 남긴다
        leaf_core_log(&format!("fatal: {e}"));
        std::process::exit(1);
    }
}

fn leaf_core_log(msg: &str) {
    if let Some(dir) =
        std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("LeafViewer").join("logs"))
    {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("fatal.log"), msg);
    }
}

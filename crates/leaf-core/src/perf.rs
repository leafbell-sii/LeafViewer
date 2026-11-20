//! 성능 측정 로그 (SPEC §10 `perf/`). `%LOCALAPPDATA%\LeafViewer\logs\perf.log`에
//! `key=value` 한 줄씩 덧붙인다. 측정 스크립트가 이 파일을 읽는다.

use std::io::Write;
use std::path::PathBuf;

pub fn log_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("LeafViewer").join("logs"))
}

pub fn record(key: &str, value: impl std::fmt::Display) {
    let Some(dir) = log_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("perf.log"))
    {
        let _ = writeln!(f, "pid={} {key}={value}", std::process::id());
    }
}

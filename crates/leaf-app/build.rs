//! 앱 매니페스트 임베드: Per-Monitor V2 DPI, longPathAware, Common Controls v6 (SPEC §9).
//! 앱 아이콘 리소스(ID 1) 임베드: `leafviewer.rc` → `assets/leafviewer.ico`.
fn main() {
    let dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let manifest = dir.join("leafviewer.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!(
        "cargo:rerun-if-changed={}",
        dir.join("../../assets/leafviewer.ico").display()
    );
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("leafviewer.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}

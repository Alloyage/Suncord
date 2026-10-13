use std::path::PathBuf;
use std::hash::{Hash, Hasher};

fn main() {
    invalidate_icon_cache();
    tauri_build::build();
    build_opus();
}

/// Windows caches application icons aggressively. Embedding a revision from the
/// ICO makes a changed icon invalidate the local WebView/bundle build artifact.
fn invalidate_icon_cache() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest.join("icons").join("icon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let bytes = std::fs::read(&icon).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    println!("cargo:rustc-env=SUNCORD_ICON_REVISION={:x}", hasher.finish());
}

fn build_opus() {
    ensure_cmake_on_path();
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let source = manifest.join("third_party").join("opus");
    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rerun-if-changed={}",
        source.join("CMakeLists.txt").display()
    );
    let destination = cmake::Config::new(&source)
        .define("OPUS_BUILD_PROGRAMS", "OFF")
        .define("OPUS_BUILD_TESTING", "OFF")
        .define("OPUS_BUILD_SHARED_LIBRARY", "OFF")
        .define("OPUS_INSTALL_PKG_CONFIG_MODULE", "OFF")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("OPUS_ENABLE_FLOAT_API", "ON")
        .profile("Release")
        .build();
    let lib_dir = destination.join("lib");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=opus");
}

fn ensure_cmake_on_path() {
    let candidates = [
        r"C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin",
        r"C:\Program Files\Microsoft Visual Studio\2022\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin",
        r"C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin",
        r"C:\Program Files\CMake\bin",
    ];
    let Ok(path) = std::env::var("PATH") else {
        return;
    };
    for candidate in candidates {
        if PathBuf::from(candidate).join("cmake.exe").exists() {
            std::env::set_var("PATH", format!("{candidate};{path}"));
            return;
        }
    }
}

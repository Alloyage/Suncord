use std::hash::{Hash, Hasher};
use std::path::PathBuf;

fn main() {
    invalidate_icon_cache();
    tauri_build::build();
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

use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=SUNCORD_ASSETS");
    println!("cargo:rerun-if-env-changed=PYTHON");
    for path in [
        "tools/assets.py",
        "assets/sources.json",
        "assets/emoji/emojis.tsv",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let assets = env::var_os("SUNCORD_ASSETS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("assets");
            let python = env::var_os("PYTHON")
                .unwrap_or_else(|| if cfg!(windows) { "python" } else { "python3" }.into());
            let status = Command::new(python)
                .arg("tools/assets.py")
                .arg(&output)
                .status()
                .expect("Asset preparation requires Python with fonttools and brotli");
            assert!(status.success(), "Could not prepare bundled assets");
            output
        });
    println!("cargo:rustc-env=SUNCORD_ASSETS={}", assets.display());
}

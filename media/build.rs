fn main() {
    println!("cargo:rerun-if-changed=src/dave_identity.cpp");
    println!("cargo:rerun-if-env-changed=SUNCORD_DAVE_DIR");
    let mut build = cc::Build::new();
    if let Some(directory) = std::env::var_os("SUNCORD_DAVE_DIR") {
        build.include(std::path::PathBuf::from(directory).join("include"));
    }
    build
        .cpp(true)
        .std("c++17")
        .opt_level(2)
        .file("src/dave_identity.cpp")
        .compile("suncord_dave_identity");
}

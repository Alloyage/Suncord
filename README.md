# Suncord

A Discord client for Linux and Windows with a browser interface and a local Rust backend.

Run with `cargo run --release`

<details>
<summary>Build requirements</summary>

- Rust and a C++ compiler. Use the MSVC toolchain on Windows.
- GStreamer 1.28 runtime and development packages, plus `pkg-config`. Use the MSVC packages on Windows and add their `bin` directory to `PATH`. Set `PKG_CONFIG_PATH` to GStreamer's `lib/pkgconfig` directory if it is not detected.
- The [libdave 1.2.1 SDK](https://github.com/discord/libdave/releases/tag/v1.2.1%2Fcpp). Set `SUNCORD_DAVE_DIR` to the extracted folder. Add its `bin` directory to `PATH` on Windows, or its `lib` directory to `LD_LIBRARY_PATH` on Linux.
- Python with `fonttools` and `brotli` for preparing bundled fonts: `python -m pip install fonttools brotli`.

</details>

This is an unofficial client. Discord may restrict accounts using third-party clients.

fn main() {
    // `tauri-build` requires every path in `bundle.resources` to exist when the
    // crate is compiled, and `binaries` is where the Tauri CLI stages aisshd and
    // aissh-mcp. Without this, a plain `cargo build` or `cargo test` on a fresh
    // checkout fails with "resource path `binaries` doesn't exist", because only
    // a bundling build runs the staging script first.
    //
    // The directory is created empty, stays out of version control, and is
    // emptied and repopulated by `scripts/prepare-helpers.mjs` before any bundle,
    // so nothing stray is ever packaged. Cargo runs build scripts from the
    // package root, so the relative path is the crate's own directory.
    let binaries = std::path::Path::new("binaries");
    if !binaries.is_dir() {
        if let Err(error) = std::fs::create_dir_all(binaries) {
            println!(
                "cargo:warning=cannot create {}: {error}",
                binaries.display()
            );
        }
    }

    // The Windows resource list also names WebView2Loader.dll, because
    // `webview2-com-sys` links the loader dynamically under every toolchain but
    // MSVC. `tauri-build` copies a real one next to the binary, and
    // `scripts/prepare-helpers.mjs` stages that copy for the bundler. A plain
    // `cargo build` runs neither, so an empty placeholder keeps the resource
    // check satisfied; it is replaced before anything is bundled.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let loader = std::path::Path::new("WebView2Loader.dll");
        if !loader.is_file() {
            if let Err(error) = std::fs::write(loader, []) {
                println!("cargo:warning=cannot create {}: {error}", loader.display());
            }
        }
    }

    tauri_build::build()
}

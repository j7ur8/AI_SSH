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
            println!("cargo:warning=cannot create {}: {error}", binaries.display());
        }
    }

    tauri_build::build()
}

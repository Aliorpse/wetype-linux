//! Regenerate `frontends/fcitx5/include/wetype/wetype.h` from the Rust C ABI.
//!
//! Two callers:
//!
//! * the Meson build, which passes the output path explicitly and runs this
//!   before any C++ is compiled, so the checked-in header cannot go stale;
//! * a manual `cargo run -p wetype-ffi --features cbindgen --bin
//!   wetype-ffi-header` (no argument, writes the default path) to refresh the
//!   checked-in copy without building the frontend.
//!
//! The file is only rewritten when the rendered content changed: Meson invokes
//! this on every build, and touching the header would recompile the whole
//! addon through its depfile.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR")?;
    let out = match std::env::args_os().nth(1) {
        Some(path) => std::path::PathBuf::from(path),
        None => std::path::Path::new(&crate_dir)
            .join("../..")
            .join("frontends/fcitx5/include/wetype/wetype.h"),
    };

    let config = cbindgen::Config {
        language: cbindgen::Language::C,
        cpp_compat: true,
        include_guard: Some("WETYPE_H".into()),
        documentation: true,
        ..Default::default()
    };

    let bindings = cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(config)
        .generate()?;

    let mut rendered = Vec::new();
    bindings.write(&mut rendered);
    if std::fs::read(&out).ok().as_deref() == Some(rendered.as_slice()) {
        return Ok(());
    }
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    bindings.write_to_file(&out);
    println!("wrote {}", out.display());
    Ok(())
}

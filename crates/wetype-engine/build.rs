//! Feed the pinned WeType version to the engine host.
//!
//! `InitParam+0x58` carries the application version the engine believes it is
//! running inside. It has to match the DLL and the dictionaries it loads, so it
//! comes from `wetype.lock` - the same file `scripts/fetch_wetype.sh` downloads
//! from - instead of being one more literal that silently drifts from the
//! payload.

use std::path::PathBuf;

fn main() {
    let lock = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("wetype.lock");
    println!("cargo:rerun-if-changed={}", lock.display());

    let text = std::fs::read_to_string(&lock)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", lock.display()));
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", lock.display()));
    let version = parsed
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("{} has no string \"version\" field", lock.display()));

    println!("cargo:rustc-env=WETYPE_APP_VERSION={version}");
}

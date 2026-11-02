//! Engine bring-up: `wxime_initialize` + `wxime_config_dict`.
//!
//! Every value here comes from `RECIPE.md` and is not guesswork:
//!
//! * `InitParam` is 0xD0 bytes and needs the work directory at +0x18, the log
//!   callback at +0x00, `platform = 6` at +0x50, `3` at +0xC0, and three
//!   `{char*, u32 len}` identity strings. Without the last two groups
//!   `create_session` dies with a null dereference deep inside the engine.
//! * `DictInfo` is 24 bytes: `{u64 handle, const char* name, i32 type, i32 version}`.
//!   The engine's registry key is the **low byte** of `type`, which is why the
//!   three `swbd*.bin` wubi tables share `0x80003`.
//! * Cell dictionaries are rejected by `config_dict`; they must go through
//!   `wxime_add_cell_dict`.
//!
//! Nothing past this point is hard-coded: the dictionary list is read from the
//! shipped `BuildIn/index.json`, and every function is resolved by name.

use std::ffi::{CStr, CString, c_char, c_void};
use std::os::raw::c_int;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::api::EngineApi;

/// Application version reported to the engine (`InitParam+0x58`).
///
/// Comes from `wetype.lock` at build time (see `build.rs`), so it cannot drift
/// from the payload `scripts/fetch_wetype.sh` downloads.
static APP_VERSION: &[u8] = concat!(env!("WETYPE_APP_VERSION"), "\0").as_bytes();
static DEVICE_NAME: &[u8] = b"LINUX\0";
static OS_VERSION: &[u8] = b"10.0.19045\0";

/// One entry of `BuildIn/index.json`, reduced to what the engine needs.
#[derive(Debug, PartialEq, Eq)]
struct DictEntry {
    name: String,
    ty: i32,
    version: i32,
    is_cell: bool,
}

/// Engine log sink, `InitParam+0x00`.
///
/// Called from engine worker threads, so it only touches `tracing`, which is
/// thread-safe by construction. Levels are demoted: the engine is chatty and its
/// WARN lines are informational for us.
unsafe extern "C" fn log_cb(level: c_int, msg: *const c_char) {
    if msg.is_null() {
        return;
    }
    let text = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
    match level {
        0 | 1 => tracing::debug!(target: "engine", "{text}"),
        _ => tracing::trace!(target: "engine", "{text}"),
    }
}

/// 24-byte `DictInfo`, layout pinned by the host's own `DictConfig` builder.
#[repr(C)]
struct DictInfo {
    handle: u64,
    name: *const c_char,
    ty: i32,
    version: i32,
}

#[repr(C)]
struct DictConfig {
    base: *const DictInfo,
    count: u32,
    _pad: u32,
    extra: *const c_char,
}

/// Leak a string so the engine can keep the pointer forever.
///
/// The engine stores these pointers for the lifetime of the process; handing it
/// borrowed memory would be a use-after-free the moment a `String` goes out of
/// scope.
fn leak(s: impl Into<Vec<u8>>) -> Result<&'static CStr> {
    let c = CString::new(s).context("engine string contains a NUL byte")?;
    Ok(Box::leak(c.into_boxed_c_str()))
}

/// Write `{char* s, u32 len}` at `off` / `off + 8` inside `cfg`.
fn set_str(cfg: &mut [u64], off: usize, s: &'static [u8]) {
    let base = cfg.as_mut_ptr() as *mut u8;
    // SAFETY: `off + 12` is inside the 0xD0-byte struct for the offsets we use.
    unsafe {
        std::ptr::write_unaligned(base.add(off) as *mut u64, s.as_ptr() as u64);
        std::ptr::write_unaligned(base.add(off + 8) as *mut u32, (s.len() - 1) as u32);
    }
}

/// Bring the engine up and register the shipped dictionaries.
pub fn initialize(api: &EngineApi, install_dir: &Path, work_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(work_dir)
        .with_context(|| format!("cannot create work dir {}", work_dir.display()))?;
    let work = leak(work_dir.to_string_lossy().into_owned().into_bytes())?;

    // sizeof(InitParam) == 0xD0, confirmed by the host's own
    // `wxime_size_check(0xD0, 0x138)` call.
    let mut cfg = [0u64; 0xD0 / 8];
    cfg[0] = log_cb as *const () as usize as u64;
    // +0x18: work dir. The engine strlen()s it, normalises '\' to '/', and
    // writes the result back over this field, so it must be writable memory we
    // keep alive - hence the leak.
    cfg[3] = work.as_ptr() as u64;
    unsafe {
        let base = cfg.as_mut_ptr() as *mut u8;
        std::ptr::write_unaligned(base.add(0x50) as *mut u32, 6); // platform: Windows
        std::ptr::write_unaligned(base.add(0xC0) as *mut u32, 3); // package config
    }
    set_str(&mut cfg, 0x58, APP_VERSION);
    set_str(&mut cfg, 0x68, DEVICE_NAME);
    set_str(&mut cfg, 0x78, OS_VERSION);

    tracing::debug!("calling wxime_initialize");
    // SAFETY: `cfg` is a 0xD0-byte InitParam laid out as documented.
    unsafe { (api.initialize)(cfg.as_mut_ptr() as *mut c_void) };
    tracing::info!("engine initialized");

    if let Some(uin) = std::env::var_os("WETYPE_UIN") {
        let uin = leak(uin.to_string_lossy().into_owned().into_bytes())?;
        // SAFETY: (ptr, len) pair, engine copies it.
        let ok = unsafe { (api.reset_user_id)(uin.as_ptr(), (uin.to_bytes().len()) as u32) };
        tracing::info!(ok, "reset_user_id");
    }

    configure_dicts(api, install_dir, work_dir)
}

/// Build and configure the dictionary set from `BuildIn/index.json`.
pub fn configure_dicts(api: &EngineApi, install_dir: &Path, user_dir: &Path) -> Result<()> {
    let index_path = install_dir.join("BuildIn").join("index.json");
    let json = std::fs::read_to_string(&index_path)
        .with_context(|| format!("cannot read {}", index_path.display()))?;
    let entries = parse_index(&json)?;
    if entries.is_empty() {
        bail!("{} yielded no dictionary entries", index_path.display());
    }

    let mut infos: Vec<DictInfo> = Vec::new();
    let mut cells: Vec<(&'static CStr, u32)> = Vec::new();
    for e in &entries {
        let path = if std::env::var_os("WETYPE_DICT_RELATIVE").is_some() {
            // The probe-verified shape: relative to the engine's cwd.
            format!("BuildIn/{}", e.name)
        } else {
            install_dir
                .join("BuildIn")
                .join(&e.name)
                .to_string_lossy()
                .into_owned()
        };
        let cpath = leak(path.into_bytes())?;
        if e.is_cell {
            cells.push((cpath, e.ty as u32));
            continue;
        }
        infos.push(DictInfo {
            handle: 0,
            name: cpath.as_ptr(),
            ty: e.ty,
            version: e.version,
        });
    }

    let user = leak(user_dir.to_string_lossy().into_owned().into_bytes())?;
    let config = DictConfig {
        base: infos.as_ptr(),
        count: infos.len() as u32,
        _pad: 0,
        extra: user.as_ptr(),
    };
    // SAFETY: `config` points at `infos`, which outlives the call.
    let ok = unsafe { (api.config_dict)(&config as *const DictConfig as *mut c_void) };
    tracing::info!(
        accepted = ok,
        files = infos.len(),
        cells = cells.len(),
        "wxime_config_dict"
    );
    if ok == 0 {
        bail!("wxime_config_dict rejected the dictionary set");
    }

    for (name, ty) in &cells {
        // SAFETY: name is a leaked NUL-terminated string; fd is NULL on Windows.
        let ok = unsafe { (api.add_cell_dict)(name.as_ptr(), *ty, std::ptr::null_mut()) };
        if ok == 0 {
            // A rejected cell dictionary degrades input silently (no emoji or
            // symbol candidates), so it must not be a debug-level whisper.
            tracing::warn!(name = %name.to_string_lossy(), ty, "add_cell_dict rejected");
        } else {
            tracing::debug!(name = %name.to_string_lossy(), ty, "add_cell_dict");
        }
    }
    Ok(())
}

/// Parse `BuildIn/index.json`.
///
/// The file is machine-generated and flat: an array of objects with `name`,
/// `type`, `version` and `is_cell`. Entries missing required fields or carrying
/// values outside the engine's integer ranges are skipped with a warning.
fn parse_index(json: &str) -> Result<Vec<DictEntry>> {
    let root: serde_json::Value =
        serde_json::from_str(json).context("BuildIn/index.json is not valid JSON")?;
    let array = root
        .as_array()
        .context("BuildIn/index.json is not a JSON array")?;

    let mut out = Vec::with_capacity(array.len());
    for (i, item) in array.iter().enumerate() {
        // A dictionary without a name/path cannot be registered, and one
        // without a numeric type cannot be dispatched. Skipping silently would
        // degrade input without a trace, so say which entry was dropped.
        let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
            tracing::warn!(index = i, "dictionary entry has no name, skipped");
            continue;
        };
        let Some(ty) = item
            .get("type")
            .and_then(|v| v.as_i64())
            .and_then(|value| i32::try_from(value).ok())
        else {
            tracing::warn!(
                index = i,
                name,
                "dictionary entry has invalid type, skipped"
            );
            continue;
        };
        let Some(version) = item
            .get("version")
            .and_then(|v| v.as_i64())
            .and_then(|value| i32::try_from(value).ok())
        else {
            tracing::warn!(
                index = i,
                name,
                "dictionary entry has invalid version, skipped"
            );
            continue;
        };
        out.push(DictEntry {
            name: name.to_string(),
            ty,
            version,
            is_cell: item
                .get("is_cell")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_looking_index() {
        let json = r#"[
            {"name": "mdltrpc.bin", "type": 262149, "version": 485, "is_cell": false},
            {"name": "cell_dict.ssrc.zh", "type": 7, "version": 869, "is_cell": true}
        ]"#;
        let got = parse_index(json).expect("valid JSON");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "mdltrpc.bin");
        assert_eq!(got[0].ty, 262149);
        assert_eq!(got[0].version, 485);
        assert!(!got[0].is_cell);
        assert_eq!(got[1].name, "cell_dict.ssrc.zh");
        assert_eq!(got[1].ty, 7);
        assert!(got[1].is_cell);
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_index("[{\"name\": ").is_err());
        assert!(
            parse_index("{\"name\": \"x\"}").is_err(),
            "must be an array"
        );
    }

    #[test]
    fn skips_objects_without_the_fields_we_need() {
        let json = r#"[{"md5": "abc"}, {"name": "wsd.bin", "type": 65549, "version": 1047}]"#;
        let got = parse_index(json).expect("valid JSON");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "wsd.bin");
        assert_eq!(got[0].version, 1047);
        assert!(!got[0].is_cell, "absent is_cell means a normal dictionary");
    }

    #[test]
    fn skips_dictionary_values_outside_engine_integer_ranges() {
        let json = r#"[
            {"name": "wide-type.bin", "type": 2147483648, "version": 1},
            {"name": "wide-version.bin", "type": 1, "version": -2147483649},
            {"name": "valid.bin", "type": 1, "version": 2}
        ]"#;
        let got = parse_index(json).expect("valid JSON");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "valid.bin");
    }

    #[test]
    fn leaked_strings_outlive_the_call() {
        let s = leak(b"hello".to_vec()).expect("no NUL");
        assert_eq!(s.to_bytes(), b"hello");
        // Leaked on purpose: dropping a CString here would invalidate the
        // pointer the engine keeps.
        assert_eq!(s.to_bytes().len(), 5);
    }
}

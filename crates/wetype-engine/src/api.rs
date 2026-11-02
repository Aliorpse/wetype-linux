//! Symbol table for `libwxhld.dll`.
//!
//! Every entry is resolved **by name** at load time - no addresses, no
//! offsets. That is the whole point of this project: if Tencent ships a new
//! build that keeps the exports, we keep working.
//!
//! Signatures come from `docs/RECIPE.md` (see the analysis repo), which in turn
//! cites the engine's own debug strings and the two working reference
//! implementations.
//!
//! # Why the `Library` is never dropped
//!
//! `libloading::Library` calls `FreeLibrary` on drop. Once the engine has
//! started its worker threads that is unsafe: we measured `ExitProcess`
//! deadlocking inside the loader lock, so `FreeLibrary` is out of the question
//! too.
//!
//! This is enforced by the type, not by discipline: the handle lives in a
//! [`ManuallyDrop`], so plain `drop(EngineApi)` cannot unload the DLL even if a
//! future caller forgets about the hazard. The process is expected to die
//! instead of unwinding the library.

use std::ffi::{OsStr, c_char, c_void};
use std::mem::ManuallyDrop;

use anyhow::{Context, Result};
use libloading::Library;

type RawFn = *mut c_void;

/// Everything we call on the engine, resolved once.
///
/// Field names match the exports exactly (without the `wxime_` prefix) so a
/// grep for `RECIPE.md` entries lands immediately.
///
/// Some exports are retained for future engine features; keeping their symbols
/// in one table also documents the ABI surface and keeps lookup code centralized.
#[allow(dead_code)]
pub struct EngineApi {
    /// Owns the module handle. `ManuallyDrop` on purpose: dropping it would run
    /// `FreeLibrary`, which is unsafe once engine threads exist. See module docs.
    _lib: ManuallyDrop<Library>,

    pub get_version: unsafe extern "C" fn() -> *const c_char,
    pub initialize: unsafe extern "C" fn(*mut c_void),
    /// The engine's own layout assertion: accepts only the sizes it was built
    /// with, and throws otherwise. Called before `initialize` so a mismatched
    /// payload fails loudly instead of corrupting memory.
    pub size_check: unsafe extern "C" fn(i32, i32) -> u8,
    pub config_dict: unsafe extern "C" fn(*mut c_void) -> u8,
    pub add_cell_dict: unsafe extern "C" fn(*const c_char, u32, *mut c_void) -> u8,
    /// `bool` in `AL` only, same caveat as `candidate_next`.
    pub reset_user_id: unsafe extern "C" fn(*const c_char, u32) -> u8,
    pub create_session: unsafe extern "C" fn(*const c_void) -> i64,
    pub destroy_session: unsafe extern "C" fn(i64),
    pub reset_session: unsafe extern "C" fn(i64),
    /// Stops the engine's worker threads. Called on the way out so a normal
    /// `ExitProcess` has a chance to complete instead of deadlocking.
    pub finalize: unsafe extern "C" fn(),
    pub add_session_listener: unsafe extern "C" fn(i64, RawFn, RawFn),
    /// `(session, text, len, blob, blob_len) -> bool`.
    ///
    /// The 5th argument lives on the stack and Ghidra does not model it, but
    /// the working reference calls it with `nullptr, 0` and the engine's own
    /// lambda reads both slots. Passing only four leaves that stack slot
    /// uninitialised, which happens to be zero today and is therefore UB.
    pub process_input: unsafe extern "C" fn(u64, *const c_char, u32, *const c_void, u32) -> u8,
    /// 12 arguments: session, text, id, then four spare `{ptr, u32}` pairs.
    pub select_candidate: unsafe extern "C" fn(
        i64,
        *const c_char,
        u32,
        *const c_void,
        u32,
        *const c_void,
        u32,
        *const c_void,
        u32,
        *const c_void,
        u32,
        *const c_void,
    ),
    /// `bool __cdecl(unsigned __int64, unsigned char, const char*, unsigned int)`:
    /// the 3rd argument is the text to delete, not an output buffer.
    pub drop_last: unsafe extern "C" fn(u64, u8, *const c_char, u32) -> u8,
    /// Returns `bool` in `AL`; the upper bits are not cleared by the engine, so
    /// the return type must be `u8` - reading it as `u64` yields garbage.
    pub candidate_next: unsafe extern "C" fn(*mut c_void, *mut c_void) -> u8,
    pub delete_candidate_iterator: unsafe extern "C" fn(*mut c_void),
}

impl EngineApi {
    /// Load the engine and resolve every symbol.
    ///
    /// `path` may be a bare name (`libwxhld.dll`), in which case the normal
    /// Windows search order applies and the DLL's own dependencies
    /// (`ime_net.dll`, `XNet.dll`, `tensorflowlite_c.dll`, ...) must be
    /// findable the same way - typically by having the install directory on
    /// `PATH`.
    ///
    /// Accepts anything that is an `AsRef<OsStr>` (`&str`, `Path`, `PathBuf`),
    /// which matters because install paths on Windows are not guaranteed to be
    /// valid UTF-8.
    pub fn load(path: impl AsRef<OsStr>) -> Result<Self> {
        let path = path.as_ref();
        // SAFETY: loading a DLL runs its DllMain and any static initialisers.
        // The caller is responsible for pointing this at the shipped artifact.
        let lib = unsafe { Library::new(path) }
            .with_context(|| format!("cannot load {}", path.to_string_lossy()))?;

        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                // SAFETY: the type must match the export's real signature; the
                // signatures are documented in RECIPE.md and verified by the
                // probe. Symbol lifetime ends with `lib`, which we never drop,
                // so dereferencing to a bare fn pointer is sound here.
                let s = unsafe { lib.get::<$ty>(concat!($name, "\0").as_bytes()) }
                    .with_context(|| format!("missing export {}", $name))?;
                *s
            }};
        }

        // Resolve everything into locals first: `lib` is moved into `_lib` when
        // the struct is built, and a struct literal would evaluate the field
        // expressions after the move.
        let get_version = sym!("wxime_get_version", unsafe extern "C" fn() -> *const c_char);
        let initialize = sym!("wxime_initialize", unsafe extern "C" fn(*mut c_void));
        let size_check = sym!("wxime_size_check", unsafe extern "C" fn(i32, i32) -> u8);
        let config_dict = sym!("wxime_config_dict", unsafe extern "C" fn(*mut c_void) -> u8);
        let add_cell_dict = sym!(
            "wxime_add_cell_dict",
            unsafe extern "C" fn(*const c_char, u32, *mut c_void) -> u8
        );
        let create_session = sym!(
            "wxime_create_session",
            unsafe extern "C" fn(*const c_void) -> i64
        );
        let destroy_session = sym!("wxime_destroy_session", unsafe extern "C" fn(i64));
        let reset_user_id = sym!(
            "wxime_reset_user_id",
            unsafe extern "C" fn(*const c_char, u32) -> u8
        );
        let reset_session = sym!("wxime_reset_session", unsafe extern "C" fn(i64));
        let finalize = sym!("wxime_finalize", unsafe extern "C" fn());
        let add_session_listener = sym!(
            "wxime_add_session_listener",
            unsafe extern "C" fn(i64, RawFn, RawFn)
        );
        let process_input = sym!(
            "wxime_process_input",
            unsafe extern "C" fn(u64, *const c_char, u32, *const c_void, u32) -> u8
        );
        let select_candidate = sym!(
            "wxime_select_candidate",
            unsafe extern "C" fn(
                i64,
                *const c_char,
                u32,
                *const c_void,
                u32,
                *const c_void,
                u32,
                *const c_void,
                u32,
                *const c_void,
                u32,
                *const c_void,
            )
        );
        let drop_last = sym!(
            "wxime_drop_last",
            unsafe extern "C" fn(u64, u8, *const c_char, u32) -> u8
        );
        let candidate_next = sym!(
            "wxime_candidate_next",
            unsafe extern "C" fn(*mut c_void, *mut c_void) -> u8
        );
        let delete_candidate_iterator = sym!(
            "wxime_delete_candidate_iterator",
            unsafe extern "C" fn(*mut c_void)
        );

        Ok(Self {
            _lib: ManuallyDrop::new(lib),
            get_version,
            initialize,
            size_check,
            config_dict,
            add_cell_dict,
            reset_user_id,
            create_session,
            destroy_session,
            reset_session,
            finalize,
            add_session_listener,
            process_input,
            select_candidate,
            drop_last,
            candidate_next,
            delete_candidate_iterator,
        })
    }

    /// Engine version string, as reported by the engine itself.
    pub fn version(&self) -> String {
        // SAFETY: get_version returns a NUL-terminated string owned by the
        // engine and valid for the process lifetime.
        unsafe {
            let p = (self.get_version)();
            if p.is_null() {
                return String::from("(null)");
            }
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

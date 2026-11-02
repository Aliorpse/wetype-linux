//! C ABI for frontends (fcitx5 today, ibus or anything else later).
//!
//! The shape is deliberately boring C:
//!
//! * opaque [`WetypeHandle`], one per connection;
//! * every function returns `0` on success and `-1` on failure;
//! * failures leave a message retrievable with [`wetype_last_error`]
//!   (valid until that thread calls `wetype_last_error` again);
//! * all text is `(pointer, length)` - candidate strings are not NUL-terminated
//!   and may contain any bytes, so `strlen` is never used;
//! * everything a call allocates is released by the matching `*_free`.
//!
//! The header `frontends/fcitx5/include/wetype/wetype.h` is generated from this
//! file with cbindgen, so the two cannot drift. The Meson build regenerates it
//! on every build; to refresh the checked-in copy without building the addon:
//!
//! ```sh
//! cargo run -p wetype-ffi --features cbindgen --bin wetype-ffi-header
//! ```

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int};
use std::sync::Mutex;

use wetype_client::Client;
use wetype_types::ipc;

/// Opaque handle owning one `wetype-client` connection.
pub struct WetypeHandle {
    client: Client,
    /// Errors are per-handle so two addon instances cannot overwrite each
    /// other's message.
    last_error: Mutex<CString>,
}

thread_local! {
    /// Last error for calls that have no handle yet (`wetype_connect`).
    static GLOBAL_ERROR: RefCell<CString> = RefCell::new(CString::new("").unwrap());
    /// Stable per-thread copy returned by `wetype_last_error`.
    static RETURNED_ERROR: RefCell<CString> = RefCell::new(CString::new("").unwrap());
}

fn set_global_error(msg: &str) {
    GLOBAL_ERROR.with(|e| {
        *e.borrow_mut() = CString::new(msg.replace('\0', " ")).unwrap_or_default();
    });
}

/// Ownership of a byte buffer handed to C.
struct CBytes {
    ptr: *mut u8,
    len: usize,
}

impl CBytes {
    fn from_slice(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            return Self {
                ptr: std::ptr::null_mut(),
                len: 0,
            };
        }
        let mut boxed = bytes.to_vec().into_boxed_slice();
        let ptr = boxed.as_mut_ptr();
        let len = boxed.len();
        std::mem::forget(boxed);
        Self { ptr, len }
    }
}

/// Reclaim a buffer produced by [`CBytes`].
///
/// # Safety
/// `ptr`/`len` must come from this library and be freed exactly once.
unsafe fn free_bytes(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    // SAFETY: the pointer came from `Box<[u8]>` with this length.
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
}

// ------------------------------------------------------------------- structs

/// One candidate. Pointers are owned by the enclosing `WetypeCandidates`.
#[repr(C)]
pub struct WetypeCandidate {
    pub text: *const u8,
    pub text_len: usize,
    pub cover: u32,
    pub cloud: bool,
    pub comment: *const u8,
    pub comment_len: usize,
}

/// Snapshot returned by key/backspace. Release with [`wetype_candidates_free`].
///
/// All-zero is a valid empty snapshot, which is what `Default` produces and
/// what the free functions leave behind.
#[repr(C)]
#[derive(Default)]
pub struct WetypeCandidates {
    pub generation: u64,
    pub items: *mut WetypeCandidate,
    pub items_len: usize,
    pub truncated: bool,
    pub preedit: *const u8,
    pub preedit_len: usize,
    pub cursor: u32,
    /// Text the engine wants committed right now (punctuation, auto-commit).
    pub commit: *const u8,
    pub commit_len: usize,
    /// Composition without display separators: preedit is "ni'hao", raw is
    /// "nihao". This is what Enter should commit.
    pub raw: *const u8,
    pub raw_len: usize,
}

/// Result of a selection. Release with [`wetype_select_result_free`].
#[repr(C)]
#[derive(Default)]
pub struct WetypeSelectResult {
    /// Text to insert into the application now.
    pub commit: *const u8,
    pub commit_len: usize,
    /// True when the composition ended; `rest` is empty then.
    pub finished: bool,
    /// Composition that is still open after a partial commit. The engine host
    /// has already rebuilt the session, so this is an ordinary snapshot.
    pub rest: WetypeCandidates,
}

fn into_c_candidates(src: &ipc::Candidates) -> WetypeCandidates {
    // Capacity is pinned to the length on purpose: `wetype_candidates_free`
    // rebuilds this Vec with `Vec::from_raw_parts(ptr, len, len)`, which is only
    // sound when capacity == len. `with_capacity` guarantees it; `collect`
    // would merely happen to produce it today.
    let mut items: Vec<WetypeCandidate> = Vec::with_capacity(src.items.len());
    items.extend(src.items.iter().map(|c| {
        let text = CBytes::from_slice(&c.text);
        let comment = CBytes::from_slice(&c.comment);
        WetypeCandidate {
            text: text.ptr,
            text_len: text.len,
            cover: c.cover,
            cloud: c.cloud,
            comment: comment.ptr,
            comment_len: comment.len,
        }
    }));
    let items_ptr = items.as_mut_ptr();
    let items_len = items.len();
    std::mem::forget(items);

    let preedit = CBytes::from_slice(&src.preedit);
    let commit = CBytes::from_slice(&src.commit);
    let raw = CBytes::from_slice(&src.raw);
    WetypeCandidates {
        generation: src.generation,
        items: items_ptr,
        items_len,
        truncated: src.truncated,
        preedit: preedit.ptr,
        preedit_len: preedit.len,
        cursor: src.cursor,
        commit: commit.ptr,
        commit_len: commit.len,
        raw: raw.ptr,
        raw_len: raw.len,
    }
}

/// Own the buffers of a selection result. `rest` is zeroed when the selection
/// finished the composition, so callers can always free it unconditionally.
fn into_c_select_result(src: ipc::SelectResult) -> WetypeSelectResult {
    let commit = CBytes::from_slice(&src.commit);
    let rest = match &src.rest {
        Some(rest) => into_c_candidates(rest),
        None => WetypeCandidates::default(),
    };
    WetypeSelectResult {
        commit: commit.ptr,
        commit_len: commit.len,
        finished: src.finished,
        rest,
    }
}

fn fail(handle: &WetypeHandle, e: impl std::fmt::Display) -> c_int {
    let mut guard = handle
        .last_error
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = CString::new(e.to_string().replace('\0', " ")).unwrap_or_default();
    -1
}

/// Unwrap a handle and turn a client error into `-1` + message.
///
/// The body is a closure so the borrowed handle stays macro-hygienic: a plain
/// expression would not see the `let h` introduced by the expansion.
macro_rules! handle_call {
    ($handle:expr, |$h:ident| $body:expr) => {{
        if $handle.is_null() {
            set_global_error("handle is null");
            return -1;
        }
        // SAFETY: checked non-null above; the caller owns a live handle.
        let $h = unsafe { &*$handle };
        match $body {
            Ok(v) => v,
            Err(e) => return fail($h, e),
        }
    }};
}

// --------------------------------------------------------------------- calls

/// Connect to the daemon socket. Returns NULL on failure.
///
/// # Safety
/// `socket_path` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wetype_connect(socket_path: *const c_char) -> *mut WetypeHandle {
    if socket_path.is_null() {
        set_global_error("socket_path is null");
        return std::ptr::null_mut();
    }
    // SAFETY: caller guarantees a NUL-terminated string.
    let path = match unsafe { CStr::from_ptr(socket_path) }.to_str() {
        Ok(p) => p.to_string(),
        Err(_) => {
            set_global_error("socket_path is not valid UTF-8");
            return std::ptr::null_mut();
        }
    };
    Box::into_raw(Box::new(WetypeHandle {
        client: Client::new(path),
        last_error: Mutex::new(CString::new("").unwrap()),
    }))
}

/// Release a handle. Null is a no-op.
///
/// # Safety
/// `handle` must come from [`wetype_connect`] and be closed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wetype_close(handle: *mut WetypeHandle) {
    if handle.is_null() {
        return;
    }
    // SAFETY: pointer came from Box::into_raw in wetype_connect.
    drop(unsafe { Box::from_raw(handle) });
}

/// Last error message for this handle. Never NULL.
///
/// The returned pointer is a thread-local copy, valid until the same thread
/// calls this function again.
///
/// # Safety
/// `handle` must be a live handle, or NULL for errors raised before one exists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wetype_last_error(handle: *const WetypeHandle) -> *const c_char {
    let message = if handle.is_null() {
        GLOBAL_ERROR.with(|error| error.borrow().to_string_lossy().into_owned())
    } else {
        // SAFETY: caller guarantees the handle is live.
        let h = unsafe { &*handle };
        h.last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .to_string_lossy()
            .into_owned()
    };
    RETURNED_ERROR.with(|error| {
        *error.borrow_mut() = CString::new(message).unwrap_or_default();
        error.borrow().as_ptr()
    })
}

/// Connection generation for this handle.
///
/// Bumped on every successful (re)connect. A frontend that holds engine session
/// ids must compare this with the value it saw when it created them: a
/// different value means the daemon restarted, and every session it holds is
/// dead. The client reconnects on its own, so the caller only has to rebuild
/// its sessions.
///
/// # Safety
/// `handle` must be a live handle, or NULL (which yields 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wetype_generation(handle: *const WetypeHandle) -> u64 {
    if handle.is_null() {
        return 0;
    }
    // SAFETY: caller guarantees the handle is live.
    unsafe { &*handle }.client.generation()
}

/// Whether the daemon socket is connected right now.
///
/// The client keeps reconnecting on its own, so this is a snapshot: `false`
/// means "the daemon is missing or restarting", not "give up". A frontend can
/// poll it to label its UI without waiting for a failed request.
///
/// # Safety
/// `handle` must be a live handle, or NULL (which yields false).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wetype_connected(handle: *const WetypeHandle) -> bool {
    if handle.is_null() {
        return false;
    }
    // SAFETY: caller guarantees the handle is live.
    unsafe { &*handle }.client.connected()
}

/// Wire protocol version this build speaks.
///
/// Exported as a function (cbindgen cannot render a constant that points at
/// another crate) so a host can refuse a mismatched daemon instead of silently
/// losing fields during its decode/re-encode.
#[unsafe(no_mangle)]
pub extern "C" fn wetype_protocol_version() -> u32 {
    wetype_types::PROTOCOL_VERSION
}

/// Round-trip a ping. Returns 0 on success.
///
/// # Safety
/// `handle` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wetype_ping(handle: *mut WetypeHandle, protocol_version: u32) -> c_int {
    handle_call!(handle, |h| h.client.ping(protocol_version));
    0
}

/// Create a session. `app_name` may be empty or NULL. Returns 0 on success.
///
/// # Safety
/// `handle` live, `app_name` NULL or NUL-terminated, `out_session` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wetype_new_session(
    handle: *mut WetypeHandle,
    ctx_id: u32,
    app_name: *const c_char,
    out_session: *mut u64,
) -> c_int {
    if out_session.is_null() {
        set_global_error("out_session is null");
        return -1;
    }
    let app = if app_name.is_null() {
        String::new()
    } else {
        // SAFETY: caller guarantees NUL termination.
        unsafe { CStr::from_ptr(app_name) }
            .to_string_lossy()
            .into_owned()
    };
    let session = handle_call!(handle, |h| h.client.new_session(ctx_id, &app));
    // SAFETY: checked non-null above.
    unsafe { *out_session = session };
    0
}

/// # Safety
/// `handle` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wetype_close_session(handle: *mut WetypeHandle, session: u64) -> c_int {
    handle_call!(handle, |h| h.client.close_session(session));
    0
}

/// # Safety
/// `handle` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wetype_reset(handle: *mut WetypeHandle, session: u64) -> c_int {
    handle_call!(handle, |h| h.client.reset(session));
    0
}

/// Send one printable ASCII key. Returns 0 and fills `out` on success.
///
/// # Safety
/// `handle` live, `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wetype_key(
    handle: *mut WetypeHandle,
    session: u64,
    key: u8,
    out: *mut WetypeCandidates,
) -> c_int {
    if out.is_null() {
        set_global_error("out is null");
        return -1;
    }
    let cands = handle_call!(handle, |h| h.client.key(session, key));
    // SAFETY: checked non-null above.
    unsafe { *out = into_c_candidates(&cands) };
    0
}

/// # Safety
/// `handle` live, `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wetype_backspace(
    handle: *mut WetypeHandle,
    session: u64,
    count: u32,
    out: *mut WetypeCandidates,
) -> c_int {
    if out.is_null() {
        set_global_error("out is null");
        return -1;
    }
    let cands = handle_call!(handle, |h| h.client.backspace(session, count));
    // SAFETY: checked non-null above.
    unsafe { *out = into_c_candidates(&cands) };
    0
}

/// Select by index into the snapshot identified by `generation`.
///
/// # Safety
/// `handle` live, `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wetype_select(
    handle: *mut WetypeHandle,
    session: u64,
    generation: u64,
    index: u32,
    out: *mut WetypeSelectResult,
) -> c_int {
    if out.is_null() {
        set_global_error("out is null");
        return -1;
    }
    let r = handle_call!(handle, |h| h.client.select(session, generation, index));
    // SAFETY: checked non-null above.
    unsafe { *out = into_c_select_result(r) };
    0
}

/// Release a candidates snapshot. Safe on a zeroed struct.
///
/// # Safety
/// `c` must come from `wetype_key`/`wetype_backspace` and be freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wetype_candidates_free(c: *mut WetypeCandidates) {
    if c.is_null() {
        return;
    }
    // SAFETY: caller owns a snapshot produced by this library.
    let c = unsafe { &mut *c };
    // SAFETY: every pointer came from this library and is freed once.
    unsafe { free_candidates(c) };
}

/// Free the buffers of a snapshot and zero it.
///
/// # Safety
/// Every pointer in `c` must come from this library and not be freed twice.
unsafe fn free_candidates(c: &mut WetypeCandidates) {
    if !c.items.is_null() && c.items_len > 0 {
        // SAFETY: the slice was created from a Vec of exactly this length.
        let mut items: Vec<WetypeCandidate> =
            unsafe { Vec::from_raw_parts(c.items, c.items_len, c.items_len) };
        for item in &mut items {
            // SAFETY: every buffer came from CBytes::from_slice.
            unsafe {
                free_bytes(item.text as *mut u8, item.text_len);
                free_bytes(item.comment as *mut u8, item.comment_len);
            }
        }
    }
    // SAFETY: the buffers came from CBytes::from_slice.
    unsafe {
        free_bytes(c.preedit as *mut u8, c.preedit_len);
        free_bytes(c.commit as *mut u8, c.commit_len);
        free_bytes(c.raw as *mut u8, c.raw_len);
    }
    *c = WetypeCandidates::default();
}

/// Release a selection result. Safe on a zeroed struct.
///
/// # Safety
/// `r` must come from `wetype_select` and be freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wetype_select_result_free(r: *mut WetypeSelectResult) {
    if r.is_null() {
        return;
    }
    // SAFETY: caller owns a result produced by this library.
    let r = unsafe { &mut *r };
    // SAFETY: buffers came from CBytes::from_slice.
    unsafe {
        free_bytes(r.commit as *mut u8, r.commit_len);
        free_candidates(&mut r.rest);
    }
    *r = WetypeSelectResult::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_handle_is_rejected_with_an_error() {
        // SAFETY: null is explicitly part of the contract.
        let rc = unsafe { wetype_reset(std::ptr::null_mut(), 1) };
        assert_eq!(rc, -1);
        // SAFETY: reading the thread-local error is always valid.
        let msg = unsafe { CStr::from_ptr(wetype_last_error(std::ptr::null())) };
        assert!(msg.to_str().unwrap().contains("handle"));
    }

    #[test]
    fn candidate_conversion_round_trips_and_frees() {
        let src = ipc::Candidates {
            session: 1,
            generation: 4,
            items: vec![ipc::Candidate {
                text: "你".as_bytes().to_vec(),
                cover: 2,
                cloud: false,
                comment: b"you".to_vec(),
            }],
            truncated: false,
            preedit: b"ni".to_vec(),
            cursor: 2,
            commit: b",".to_vec(),
            raw: b"ni".to_vec(),
        };
        let mut c = into_c_candidates(&src);
        assert_eq!(c.generation, 4);
        assert_eq!(c.items_len, 1);
        // SAFETY: we just built the struct and own the buffers.
        unsafe {
            let item = &*c.items;
            assert_eq!(
                std::slice::from_raw_parts(item.text, item.text_len),
                "你".as_bytes()
            );
            assert_eq!(
                std::slice::from_raw_parts(item.comment, item.comment_len),
                b"you"
            );
            wetype_candidates_free(&mut c);
        }
        assert!(c.items.is_null(), "free must zero the struct");
    }

    #[test]
    fn freeing_an_empty_snapshot_is_safe() {
        let mut c = WetypeCandidates {
            generation: 0,
            items: std::ptr::null_mut(),
            items_len: 0,
            truncated: false,
            preedit: std::ptr::null(),
            preedit_len: 0,
            cursor: 0,
            commit: std::ptr::null(),
            commit_len: 0,
            raw: std::ptr::null(),
            raw_len: 0,
        };
        // SAFETY: a zeroed struct is explicitly allowed.
        unsafe { wetype_candidates_free(&mut c) };
    }
}

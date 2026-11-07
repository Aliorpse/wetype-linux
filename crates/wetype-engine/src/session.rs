//! Sessions, and the one place the engine hands us data asynchronously: the
//! session listener.
//!
//! The listener fires **synchronously inside `wxime_process_input`** (verified
//! with the probe), so a session's snapshot is ready by the time the call
//! returns. That is why this stays a plain `Mutex` instead of a channel.
//!
//! The listener receives a *candidate iterator* that is only valid for the
//! duration of the callback and must be released with
//! `wxime_delete_candidate_iterator`. Everything we need is copied out first -
//! no iterator pointer ever leaves this module.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock};

use wetype_types::ipc;

use crate::api::EngineApi;

/// One candidate, copied out of the engine's struct.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub text: Vec<u8>,
    /// Opaque blob from `+0x20`; only `select_candidate` understands it.
    pub id: Vec<u8>,
    /// How much of the composition this candidate consumes (`+0x64`).
    pub cover: u32,
    pub cloud: bool,
}

/// What the last listener event produced.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Bumped on every candidate refresh; echoed in `Candidates` and required
    /// back by `Select` so a stale index cannot pick the wrong word.
    pub generation: u64,
    pub items: Vec<Candidate>,
    /// Text already selected (pending node type 0). It has been handed to the
    /// application by a partial select, so it is deliberately not part of
    /// `preedit` or `raw`.
    pub selected: Vec<u8>,
    pub preedit: Vec<u8>,
    /// Composition without display separators; what Enter commits. This is the
    /// unselected suffix only: `cover` on each candidate is relative to it.
    pub raw: Vec<u8>,
    pub cursor: u32,
    /// Text the engine asked us to commit (event type 2). Drained on every
    /// reply so each piece is delivered exactly once.
    pub commits: Vec<Vec<u8>>,
    /// Prefix already committed to the application by earlier partial
    /// selections in this composition. The engine's final commit contains the
    /// whole phrase, so the host subtracts this before sending it on.
    pub committed_prefix: Vec<u8>,
}

/// Per-session state shared with the C callback.
pub struct Shared {
    pub snapshot: Mutex<Snapshot>,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            snapshot: Mutex::new(Snapshot::default()),
        })
    }
}

// The callback runs on an engine thread and has no way to receive Rust state
// except the `user` pointer, so the two functions it needs live in statics.
// Function pointers are `Copy + Send + Sync`, which makes this sound.
static NEXT: OnceLock<unsafe extern "C" fn(*mut c_void, *mut c_void) -> u8> = OnceLock::new();
static DELETE: OnceLock<unsafe extern "C" fn(*mut c_void)> = OnceLock::new();

/// Publish the functions the listener needs. Call once after loading.
pub fn install_hooks(api: &EngineApi) {
    let _ = NEXT.set(api.candidate_next);
    let _ = DELETE.set(api.delete_candidate_iterator);
    let _ = DEBUG_EVENTS.set(std::env::var_os("WETYPE_DEBUG_EVENTS").is_some());
}

/// `WETYPE_DEBUG_EVENTS=1` logs the first pointer-sized payload word per event.
/// Kept as a per-session-invariant flag rather than reading the environment on
/// every event: the listener runs on an engine thread and must stay cheap.
static DEBUG_EVENTS: OnceLock<bool> = OnceLock::new();

fn debug_events() -> bool {
    *DEBUG_EVENTS.get().unwrap_or(&false)
}

/// Candidate struct offsets, taken from the candidate layout in `RECIPE.md`.
const C_TEXT: usize = 0x00;
const C_TEXT_LEN: usize = 0x08;
const C_ID: usize = 0x20;
const C_ID_LEN: usize = 0x28;
const C_MARKER: usize = 0x44;
const C_ORIGIN: usize = 0x58;
const C_COVER: usize = 0x64;
/// Marker + origin pair the engine uses for cloud candidates.
const CLOUD_MARKER: u16 = 0xFFFD;
const CLOUD_ORIGIN: i64 = 11000;

/// Copy the whole iterator into `out`, then release it.
///
/// # Safety
/// `iterator` must be a live iterator from a listener payload, and `out` must
/// not alias engine memory.
unsafe fn drain_iterator(iterator: *mut c_void, out: &mut Vec<Candidate>) {
    let (Some(next), Some(del)) = (NEXT.get().copied(), DELETE.get().copied()) else {
        return;
    };
    if iterator.is_null() {
        return;
    }
    // 512 bytes is the struct size the reference implementation uses; the
    // engine only fills its own fields, so a zeroed buffer is fine.
    let mut buf = [0u8; 512];
    // A hostile or buggy engine must not spin us forever.
    for _ in 0..1024 {
        // SAFETY: buffer is 512 bytes and outlives the call.
        if unsafe { next(iterator, buf.as_mut_ptr() as *mut c_void) } == 0 {
            break;
        }
        let text = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(C_TEXT) as *const usize) }
            as *const u8;
        let text_len =
            unsafe { std::ptr::read_unaligned(buf.as_ptr().add(C_TEXT_LEN) as *const u32) };
        let id = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(C_ID) as *const usize) }
            as *const u8;
        let id_len = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(C_ID_LEN) as *const u32) };
        let cover = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(C_COVER) as *const u32) };
        let marker = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(C_MARKER) as *const u16) };
        let origin = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(C_ORIGIN) as *const i64) };

        let take = |p: *const u8, len: u32| -> Vec<u8> {
            if p.is_null() || len > 65536 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(p, len as usize) }.to_vec()
            }
        };
        let text = take(text, text_len);
        // The iterator can yield a blank row (observed after backspacing to an
        // empty composition, and after punctuation auto-commit). Selecting one
        // makes the engine throw, so it never reaches the wire.
        if text.is_empty() {
            continue;
        }
        out.push(Candidate {
            text,
            id: take(id, id_len),
            cover,
            cloud: marker == CLOUD_MARKER && origin == CLOUD_ORIGIN,
        });
    }
    // SAFETY: the iterator came from the engine and is released exactly once.
    unsafe { del(iterator) };
}

/// Node layout of the pending-input list (event type 5):
///
/// ```text
/// +0   u32   type
/// +8   ptr   next
/// +24  u32   length            (types 0 and 1)
/// +24  u8    original byte     (types 2, 4, 5, 6)
/// +32  ptr   data              (types 0 and 1)
/// ```
///
/// Types: 0 selected text, 1 exactly-matched text, 2/4/5 correction records,
/// 6 separator, 7 cursor marker. Corrections keep the user's own byte at +24 -
/// the engine's synthetic replacement is deliberately not shown.
///
/// Returns `(selected, display, raw, cursor)`. Type 0 is returned separately:
/// it is already committed to the application and must not be shown as editable
/// preedit. `display`/`raw` contain only the unselected suffix, which is also
/// the coordinate space of a candidate's `cover`.
///
/// # Safety
/// `head` must be a pending-input list owned by the engine for the duration of
/// the callback.
unsafe fn parse_pending(head: *mut c_void) -> (Vec<u8>, Vec<u8>, Vec<u8>, u32) {
    let mut selected: Vec<u8> = Vec::new();
    let mut display: Vec<u8> = Vec::new();
    let mut raw: Vec<u8> = Vec::new();
    let mut cursor: Option<u32> = None;
    let mut node = head as *const u8;
    let mut guard = 0usize;
    while !node.is_null() && guard < 4096 {
        guard += 1;
        // SAFETY: every field below is read through `read_unaligned`, and the
        // node points at engine-owned memory that outlives this callback.
        unsafe {
            let ty = std::ptr::read_unaligned(node as *const u32);
            match ty {
                0 => {
                    // Already selected and committed to the application; it
                    // is not part of the editable preedit anymore.
                    let len = std::ptr::read_unaligned(node.add(24) as *const u32);
                    let data = std::ptr::read_unaligned(node.add(32) as *const *const u8);
                    if !data.is_null() && len <= 65536 {
                        let value = std::slice::from_raw_parts(data, len as usize);
                        selected.extend_from_slice(value);
                    }
                }
                1 => {
                    let len = std::ptr::read_unaligned(node.add(24) as *const u32);
                    let data = std::ptr::read_unaligned(node.add(32) as *const *const u8);
                    if !data.is_null() && len <= 65536 {
                        let value = std::slice::from_raw_parts(data, len as usize);
                        display.extend_from_slice(value);
                        raw.extend_from_slice(value);
                    }
                }
                // Correction records keep the user's own byte.
                2 | 4 | 5 => {
                    let byte = *node.add(24);
                    display.push(byte);
                    raw.push(byte);
                }
                // A separator is displayed, but only belongs to the raw input
                // when the user actually typed an apostrophe.
                6 => {
                    display.push(b'\'');
                    if *node.add(24) == b'\'' {
                        raw.push(b'\'');
                    }
                }
                7 => cursor = Some(display.len() as u32),
                other => tracing::debug!(target: "events", other, "unknown pending node"),
            }
            node = std::ptr::read_unaligned(node.add(8) as *const *const u8);
        }
    }
    if guard >= 4096 {
        tracing::warn!(target: "events", "pending input list exceeded 4096 nodes");
    }
    let cursor = cursor.unwrap_or(display.len() as u32);
    (selected, display, raw, cursor)
}

/// Session listener.
///
/// Signature confirmed by the decompiler, which prints the C++ type of the
/// lambda the export stores:
/// `void (__cdecl*)(uint64_t, _wxime_event_type*, void**, unsigned, void*)`.
///
/// # Safety
/// `user` must be a `Shared` pointer handed out by [`install_hooks`]'s caller
/// via `Arc::into_raw`.
pub unsafe extern "C" fn listener(
    _session: u64,
    types: *const i32,
    payloads: *const *mut c_void,
    count: u32,
    user: *mut c_void,
) {
    if user.is_null() || types.is_null() || payloads.is_null() {
        return;
    }
    // The Sessions map owns the Arc until the engine session is destroyed, so
    // this borrowed pointer remains valid throughout the callback.
    let shared = unsafe { &*(user as *const Shared) };
    let mut snap = match shared.snapshot.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut items: Option<Vec<Candidate>> = None;
    for i in 0..count as usize {
        let ty = unsafe { *types.add(i) };
        let payload = unsafe { *payloads.add(i) };
        // Type 5 with a null payload is meaningful: it means "the pending-input
        // list is empty", i.e. the composition was cleared. Skipping it leaves a
        // stale preedit on screen forever.
        if payload.is_null() && ty != 5 {
            continue;
        }
        if debug_events() && !payload.is_null() {
            // Event payloads have different layouts, and some are only 12 bytes.
            // Read just the leading pointer-sized word common to every type.
            let q0 = unsafe { std::ptr::read_unaligned(payload as *const usize) };
            tracing::debug!(target: "events", ty, q0 = format_args!("{q0:#x}"), "listener event");
        }
        match ty {
            // 0 = candidate list, 6 = cloud results (already merged by the engine)
            0 | 6 => {
                let it = unsafe { *(payload as *const *mut c_void) };
                let mut out = Vec::new();
                unsafe { drain_iterator(it, &mut out) };
                items = Some(out);
            }
            // 2 = text the engine wants committed right now: {const char*, u32 len}.
            // This is the authoritative commit channel - the client must not
            // derive committed text from candidates.
            2 => {
                let p = unsafe { *(payload as *const *const u8) };
                let len = unsafe { *((payload as *const u8).add(8) as *const u32) };
                if !p.is_null() && len <= 65536 && len > 0 {
                    let bytes = unsafe { std::slice::from_raw_parts(p, len as usize) }.to_vec();
                    tracing::debug!(
                        target: "events",
                        text = %String::from_utf8_lossy(&bytes),
                        "commit requested"
                    );
                    snap.commits.push(bytes);
                }
            }
            // 5 = pending input: the composition, as a singly-linked list.
            5 => {
                // NB: do not call this local `display` - tracing's macros import
                // `tracing::field::display`, which would shadow it.
                let (selected, pending_text, raw, cursor) = if payload.is_null() {
                    (Vec::new(), Vec::new(), Vec::new(), 0)
                } else {
                    unsafe { parse_pending(payload) }
                };
                tracing::debug!(
                    target: "events",
                    selected = %String::from_utf8_lossy(&selected),
                    preedit = %String::from_utf8_lossy(&pending_text),
                    cursor,
                    "pending input"
                );
                snap.selected = selected;
                snap.preedit = pending_text;
                snap.raw = raw;
                snap.cursor = cursor;
            }
            _ => {}
        }
    }
    if let Some(items) = items {
        snap.items = items;
        snap.generation += 1;
    }
}

/// Sessions owned by this process.
#[derive(Default)]
pub struct Sessions {
    map: HashMap<i64, Entry>,
}

/// Everything that must live as long as the engine-side session.
struct Entry {
    shared: Arc<Shared>,
    /// Backing store for the app-name pointer inside `SessionConfig`.
    _app_name: Box<[u8]>,
}

impl Sessions {
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn get(&self, id: i64) -> Option<&Shared> {
        self.map.get(&id).map(|e| e.shared.as_ref())
    }

    pub fn insert(&mut self, id: i64, shared: Arc<Shared>, app_name: Box<[u8]>) {
        self.map.insert(
            id,
            Entry {
                shared,
                _app_name: app_name,
            },
        );
    }

    pub fn remove(&mut self, id: i64) -> bool {
        self.map.remove(&id).is_some()
    }

    pub fn snapshot(&self, id: i64) -> Option<Snapshot> {
        let shared = &self.map.get(&id)?.shared;
        let g = match shared.snapshot.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        Some(g.clone())
    }

    /// Drain the engine's pending commits for this session.
    ///
    /// Commits arrive as events and must be delivered exactly once, so they are
    /// taken out of the snapshot rather than cloned along with it.
    pub fn take_commits(&self, id: i64) -> Vec<Vec<u8>> {
        let Some(entry) = self.map.get(&id) else {
            return Vec::new();
        };
        let mut g = match entry.shared.snapshot.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        std::mem::take(&mut g.commits)
    }

    /// Prefix of this composition already committed by a partial select.
    pub fn committed_prefix(&self, id: i64) -> Vec<u8> {
        let Some(entry) = self.map.get(&id) else {
            return Vec::new();
        };
        let g = match entry.shared.snapshot.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        g.committed_prefix.clone()
    }

    /// Record text that a partial select has just handed to the application.
    pub fn append_committed_prefix(&self, id: i64, text: &[u8]) {
        let Some(entry) = self.map.get(&id) else {
            return;
        };
        let mut g = match entry.shared.snapshot.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        g.committed_prefix.extend_from_slice(text);
    }

    pub fn clear_committed_prefix(&self, id: i64) {
        let Some(entry) = self.map.get(&id) else {
            return;
        };
        let mut g = match entry.shared.snapshot.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        g.committed_prefix.clear();
    }
}

/// `SessionConfig`, 0x138 bytes. Only the fields that matter are set; see
/// `RECIPE.md` for why each one is there.
#[repr(C, align(8))]
struct SessionConfig([u8; 0x138]);

/// Predicate the engine calls at `SessionConfig+0xE8`; the official host uses a
/// function that just returns true.
unsafe extern "C" fn always_true() -> u8 {
    1
}

impl SessionConfig {
    /// Build the configuration that makes candidates arrive.
    ///
    /// `+0x01 = 1` (`with_search_candidate_instantly_session_wrapper`) is the
    /// switch that matters: without it the session is created but the listener
    /// never fires.
    ///
    /// `+0x41` (`cloud.enable_cloud_search_by_input`) is **off by default**,
    /// unlike the reference implementation. Cloud search ships the user's
    /// keystrokes to Tencent's servers; it cannot work here anyway (no
    /// `ilink_uin`, and the embedded Cronet stack reports `IsValid: 0`), so
    /// enabling it would be pure liability. `WETYPE_CLOUD=1` opts in.
    fn build(app_name: *const u8, app_name_len: usize, cloud: bool) -> Self {
        let mut cfg = Self([0u8; 0x138]);
        let b = cfg.0.as_mut_ptr();
        // SAFETY: every offset below is inside the 0x138-byte struct.
        unsafe {
            *b.add(0x01) = 1;
            *b.add(0x41) = u8::from(cloud);
            std::ptr::write_unaligned(b.add(0x50) as *mut u32, 3); // return_cnt
            std::ptr::write_unaligned(b.add(0x54) as *mut u32, 1); // predict_next_n
            std::ptr::write_unaligned(b.add(0xC0) as *mut u64, app_name as u64);
            std::ptr::write_unaligned(
                b.add(0xDC) as *mut u32,
                app_name_len.saturating_sub(1) as u32,
            );
            std::ptr::write_unaligned(
                b.add(0xE8) as *mut u64,
                always_true as *const () as usize as u64,
            );
            *b.add(0x125) = 1;
            std::ptr::write_unaligned(b.add(0x127) as *mut u16, 0x0101);
        }
        cfg
    }
}

/// Application name used when a session does not name one.
pub static DEFAULT_APP: &[u8] = b"Weixin.exe\0";

/// Create a session and attach the listener.
///
/// `app_name` is the foreground application the engine should believe it is
/// serving. The engine keeps a bare pointer to this buffer, so the caller parks
/// the returned allocation in [`Sessions`] until the session is destroyed.
pub fn create(api: &EngineApi, shared: &Arc<Shared>, app_name: &[u8]) -> (i64, Box<[u8]>) {
    let mut name = app_name.to_vec();
    if !name.ends_with(&[0]) {
        name.push(0);
    }
    let name = name.into_boxed_slice();
    let cloud = std::env::var_os("WETYPE_CLOUD").is_some();
    let cfg = SessionConfig::build(name.as_ptr(), name.len(), cloud);
    // SAFETY: `cfg` is a fully initialised SessionConfig.
    let id = unsafe { (api.create_session)(cfg.0.as_ptr() as *const c_void) };
    if id <= 0 {
        return (id, name);
    }
    // The callback borrows the `Arc` the map owns. Nothing is leaked: the
    // pointer is only valid while the map entry lives, and the entry is removed
    // only after `wxime_destroy_session` has returned.
    let user = Arc::as_ptr(shared) as *mut c_void;
    // SAFETY: listener has the documented signature; `user` outlives the
    // session by construction.
    unsafe { (api.add_session_listener)(id, listener as *mut c_void, user) };
    (id, name)
}

/// Convert a snapshot into the wire message.
pub fn to_wire(session: i64, snap: &Snapshot, limit: usize) -> ipc::Candidates {
    let truncated = snap.items.len() > limit;
    ipc::Candidates {
        session: session as u64,
        generation: snap.generation,
        items: snap
            .items
            .iter()
            .take(limit)
            .map(|c| ipc::Candidate {
                text: c.text.clone(),
                cover: c.cover,
                cloud: c.cloud,
                comment: Vec::new(),
            })
            .collect(),
        truncated,
        preedit: snap.preedit.clone(),
        raw: snap.raw.clone(),
        cursor: snap.cursor,
        // Filled by the caller from `take_commits`, never from the snapshot:
        // commits must be delivered exactly once.
        commit: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_config_has_the_switch_that_makes_candidates_arrive() {
        let cfg = SessionConfig::build(DEFAULT_APP.as_ptr(), DEFAULT_APP.len(), false);
        assert_eq!(
            cfg.0[0x01], 1,
            "+0x01 must be set or the listener never fires"
        );
        assert_eq!(cfg.0[0x41], 0, "cloud search is opt-in, not the default");
        assert_eq!(cfg.0[0x125], 1);
    }

    #[test]
    fn session_config_records_the_app_name() {
        let cfg = SessionConfig::build(DEFAULT_APP.as_ptr(), DEFAULT_APP.len(), false);
        let ptr = u64::from_unaligned(&cfg.0[0xC0..]) as *const u8;
        let len = u32::from_unaligned(&cfg.0[0xDC..]);
        assert_eq!(len, 10, "\"Weixin.exe\" is 10 bytes");
        assert!(!ptr.is_null());
    }

    #[test]
    fn wire_snapshot_truncates_and_keeps_generation() {
        let snap = Snapshot {
            generation: 7,
            items: (0..5)
                .map(|i| Candidate {
                    text: vec![b'a' + i],
                    id: vec![i],
                    cover: 1,
                    cloud: false,
                })
                .collect(),
            selected: Vec::new(),
            preedit: b"ni".to_vec(),
            raw: b"ni".to_vec(),
            cursor: 2,
            commits: Vec::new(),
            committed_prefix: Vec::new(),
        };
        let wire = to_wire(3, &snap, 3);
        assert_eq!(wire.session, 3);
        assert_eq!(wire.generation, 7);
        assert_eq!(wire.items.len(), 3);
        assert!(wire.truncated);
        assert_eq!(wire.preedit, b"ni");
    }

    trait FromUnaligned {
        fn from_unaligned(bytes: &[u8]) -> Self;
    }
    impl FromUnaligned for u64 {
        fn from_unaligned(bytes: &[u8]) -> Self {
            u64::from_le_bytes(bytes[..8].try_into().unwrap())
        }
    }
    impl FromUnaligned for u32 {
        fn from_unaligned(bytes: &[u8]) -> Self {
            u32::from_le_bytes(bytes[..4].try_into().unwrap())
        }
    }
}
